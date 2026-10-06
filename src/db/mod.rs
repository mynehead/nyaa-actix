pub mod schema;

use diesel::connection::SimpleConnection;
use diesel::pg::PgConnection;
use diesel::r2d2::{self, ManageConnection, R2D2Connection};
use diesel::sqlite::SqliteConnection;
use diesel::{Connection, ConnectionError, ConnectionResult, RunQueryDsl};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

/// One connection type for both databases. `DATABASE_URL` picks the backend: a
/// `postgres://` or `postgresql://` URL connects to PostgreSQL, anything else is a SQLite
/// file path (or `:memory:`). Queries are written once against this type; Diesel
/// renders the right SQL for whichever variant is live.
#[derive(diesel::MultiConnection)]
pub enum DbConnection {
    Sqlite(SqliteConnection),
    Pg(PgConnection),
}

pub type DbPool = r2d2::Pool<DbManager>;

const SQLITE_MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations/sqlite");
const POSTGRES_MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations/postgres");

pub fn is_postgres_url(url: &str) -> bool {
    url.starts_with("postgres://") || url.starts_with("postgresql://")
}

/// Opens a connection to the backend `url` names. Unlike `DbConnection::establish`,
/// which tries each backend in turn, this never falls back to SQLite when a Postgres
/// server is unreachable (that would create a stray file named after the URL).
pub fn connect(url: &str) -> ConnectionResult<DbConnection> {
    if is_postgres_url(url) {
        return PgConnection::establish(url).map(DbConnection::Pg);
    }
    let mut conn = SqliteConnection::establish(url)?;
    // The server and `create-user` may write at once; wait for the lock instead of failing.
    // SQLite ignores the schema's foreign keys unless each connection turns them on;
    // PostgreSQL always enforces them, so this keeps both backends alike.
    conn.batch_execute("PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON;")
        .map_err(ConnectionError::CouldntSetupConfiguration)?;
    Ok(DbConnection::Sqlite(conn))
}

/// Runs the pending migrations from the folder that matches the connection's backend.
pub fn run_migrations(conn: &mut DbConnection) -> Result<(), String> {
    let result = match conn {
        DbConnection::Sqlite(c) => repair_renumbered_avatar_migration(c)
            .map_err(Into::into)
            .and_then(|_| c.run_pending_migrations(SQLITE_MIGRATIONS).map(|_| ())),
        DbConnection::Pg(c) => c.run_pending_migrations(POSTGRES_MIGRATIONS).map(|_| ()),
    };
    result.map_err(|e| e.to_string())
}

#[derive(diesel::QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

fn count(conn: &mut SqliteConnection, query: &str) -> diesel::QueryResult<i64> {
    diesel::sql_query(query).get_result::<Count>(conn).map(|c| c.n)
}

/// The avatar migration first shipped as version 20261005000000, the same version as the
/// listing indexes, and was later renumbered to 20261005000001. A SQLite database that ran
/// the first numbering already has `users.avatar_time` but recorded 20261005000000 for it,
/// so the renumbered migration would fail ("duplicate column name") and the indexes were
/// never created. Record the avatar migration as applied and create the missing indexes.
fn repair_renumbered_avatar_migration(conn: &mut SqliteConnection) -> diesel::QueryResult<()> {
    let has_column =
        count(conn, "SELECT COUNT(*) AS n FROM pragma_table_info('users') WHERE name = 'avatar_time'")? > 0;
    if !has_column {
        return Ok(());
    }
    let recorded =
        count(conn, "SELECT COUNT(*) AS n FROM __diesel_schema_migrations WHERE version = '20261005000001'")? > 0;
    if recorded {
        return Ok(());
    }
    log::warn!("Repairing migration history: avatar column was added under an earlier migration version");
    conn.batch_execute(include_str!("../../migrations/sqlite/2026-10-05-000000_listing_indexes/up.sql"))?;
    diesel::sql_query("INSERT INTO __diesel_schema_migrations (version) VALUES ('20261005000001')").execute(conn)?;
    Ok(())
}

/// r2d2 manager that opens connections with [`connect`].
pub struct DbManager {
    url: String,
}

impl DbManager {
    pub fn new(url: &str) -> Self {
        DbManager { url: url.to_string() }
    }
}

impl ManageConnection for DbManager {
    type Connection = DbConnection;
    type Error = r2d2::Error;

    fn connect(&self) -> Result<DbConnection, r2d2::Error> {
        connect(&self.url).map_err(r2d2::Error::ConnectionError)
    }

    fn is_valid(&self, conn: &mut DbConnection) -> Result<(), r2d2::Error> {
        conn.ping().map_err(r2d2::Error::QueryError)
    }

    fn has_broken(&self, conn: &mut DbConnection) -> bool {
        conn.is_broken()
    }
}

pub fn init_pool(database_url: &str) -> DbPool {
    r2d2::Pool::builder().build(DbManager::new(database_url)).expect("Failed to create database pool")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::{nyaa_statistics, nyaa_torrents, user_sessions, users};
    use crate::models::{NewStatistic, NewTorrent, NewUser, TorrentFlags, User};
    use crate::search::db::{search, with_stats, SearchQuery, SearchSort};
    use diesel::prelude::*;

    #[test]
    fn picks_backend_from_url() {
        assert!(is_postgres_url("postgres://u@localhost/nyaa"));
        assert!(is_postgres_url("postgresql://u@localhost/nyaa"));
        assert!(!is_postgres_url("nyaa.db"));
        assert!(!is_postgres_url(":memory:"));
        assert!(matches!(connect(":memory:").unwrap(), DbConnection::Sqlite(_)));
    }

    /// A database migrated while the avatar migration was still numbered 20261005000000.
    #[test]
    fn repairs_database_from_old_avatar_migration_number() {
        let mut conn = connect(":memory:").unwrap();
        let DbConnection::Sqlite(c) = &mut conn else { unreachable!() };
        c.run_pending_migrations(SQLITE_MIGRATIONS).unwrap();
        c.batch_execute(
            "DROP INDEX nyaa_torrents_category_idx; DROP INDEX nyaa_torrents_uploader_idx; \
                         DROP INDEX nyaa_torrents_group_idx; \
                         DELETE FROM __diesel_schema_migrations WHERE version = '20261005000001';",
        )
        .unwrap();
        // The old numbering failed here with "duplicate column name: avatar_time"
        run_migrations(&mut conn).unwrap();
        let DbConnection::Sqlite(c) = &mut conn else { unreachable!() };
        assert_eq!(
            count(
                c,
                "SELECT COUNT(*) AS n FROM sqlite_master WHERE type = 'index' \
                             AND name IN ('nyaa_torrents_category_idx', \
                             'nyaa_torrents_uploader_idx', 'nyaa_torrents_group_idx')"
            )
            .unwrap(),
            3
        );
        assert_eq!(
            count(
                c,
                "SELECT COUNT(*) AS n FROM __diesel_schema_migrations \
                             WHERE version = '20261005000001'"
            )
            .unwrap(),
            1
        );
        // A fresh database and a second run are left alone
        run_migrations(&mut conn).unwrap();
        run_migrations(&mut connect(":memory:").unwrap()).unwrap();
    }

    #[test]
    fn both_backends_have_the_same_migrations() {
        let names = |dir: &str| {
            let mut v: Vec<_> =
                std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
            v.sort();
            v
        };
        assert_eq!(
            names("migrations/sqlite"),
            names("migrations/postgres"),
            "every migration needs a SQLite and a PostgreSQL version"
        );
    }

    /// Torrents saved with the old bit values (HIDDEN=1, ANONYMOUS=2, REMAKE=4, TRUSTED=8),
    /// the flags they must have after the migration, on either backend.
    const OLD_AND_NEW_FLAGS: [(i32, TorrentFlags); 5] = [
        (0x01, TorrentFlags::HIDDEN),
        (0x02, TorrentFlags::ANONYMOUS),
        (0x04, TorrentFlags::REMAKE),
        (0x08, TorrentFlags::TRUSTED),
        (
            0x01 | 0x08 | 0x10 | 0x80,
            TorrentFlags::HIDDEN
                .union(TorrentFlags::TRUSTED)
                .union(TorrentFlags::COMPLETE)
                .union(TorrentFlags::COMMENT_LOCKED),
        ),
    ];

    /// Inserts torrents with the old flags, runs the flag migration and checks the result;
    /// running it again (down.sql is the same statement) restores the old values.
    fn check_flag_migration(conn: &mut DbConnection, sql: &str) -> QueryResult<()> {
        diesel::sql_query("INSERT INTO users (id, username, password_hash) VALUES (900, 'flags', 'x')")
            .execute(conn)?;
        for (i, (old, _)) in OLD_AND_NEW_FLAGS.iter().enumerate() {
            diesel::sql_query(format!(
                "INSERT INTO nyaa_torrents (id, info_hash, display_name, torrent_name, information, description, \
                 flags, uploader_id, main_category_id, sub_category_id) \
                 VALUES ({id}, '{hash}', 't', 't', '', '', {old}, 900, 1, 2)",
                id = 900 + i,
                hash = "x".repeat(19) + &i.to_string(),
            ))
            .execute(conn)?;
        }
        let flags = |conn: &mut DbConnection| -> QueryResult<Vec<i32>> {
            nyaa_torrents::table
                .filter(nyaa_torrents::uploader_id.eq(900))
                .order(nyaa_torrents::id)
                .select(nyaa_torrents::flags)
                .load(conn)
        };
        diesel::sql_query(sql).execute(conn)?;
        assert_eq!(flags(conn)?, OLD_AND_NEW_FLAGS.iter().map(|(_, new)| new.bits()).collect::<Vec<_>>());
        diesel::sql_query(sql).execute(conn)?;
        assert_eq!(flags(conn)?, OLD_AND_NEW_FLAGS.iter().map(|(old, _)| *old).collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    fn flag_migration_converts_old_bits_on_sqlite() {
        let mut conn = connect(":memory:").unwrap();
        run_migrations(&mut conn).unwrap();
        check_flag_migration(
            &mut conn,
            include_str!("../../migrations/sqlite/2026-10-06-160100_upstream_flag_bits/up.sql"),
        )
        .unwrap();
    }

    /// The other tests run on in-memory SQLite. This one runs the Postgres migrations and the
    /// main query shapes against a real server when `TEST_POSTGRES_URL` is set (CI sets it),
    /// all inside a transaction that is rolled back.
    #[test]
    fn postgres_migrations_and_queries() {
        let Ok(url) = std::env::var("TEST_POSTGRES_URL") else {
            eprintln!("TEST_POSTGRES_URL not set; skipping the Postgres test");
            return;
        };
        let mut conn = connect(&url).unwrap();
        assert!(matches!(conn, DbConnection::Pg(_)));
        run_migrations(&mut conn).unwrap();
        conn.test_transaction::<_, diesel::result::Error, _>(|conn| {
            diesel::insert_into(users::table).values(&NewUser::new("pgtest", None, "secret")).execute(conn)?;
            let user = User::by_username(conn, "pgtest")?.unwrap();
            let now = chrono::Utc::now().naive_utc();
            for (n, name) in ["Alpha Show", "beta SHOW", "Gamma Movie"].iter().enumerate() {
                diesel::insert_into(nyaa_torrents::table)
                    .values(&NewTorrent {
                        id: None,
                        info_hash: vec![n as u8 + 1; 20],
                        display_name: name.to_string(),
                        torrent_name: "t".into(),
                        information: String::new(),
                        description: String::new(),
                        filesize: 1,
                        encoding: "utf-8".into(),
                        flags: 0,
                        uploader_id: Some(user.id),
                        uploader_ip: Some(vec![127, 0, 0, 1]),
                        has_torrent: 1,
                        comment_count: 0,
                        created_time: now,
                        updated_time: now,
                        main_category_id: 1,
                        sub_category_id: 2,
                        group_id: None,
                    })
                    .execute(conn)?;
                let id: i32 = nyaa_torrents::table
                    .filter(nyaa_torrents::display_name.eq(name))
                    .select(nyaa_torrents::id)
                    .first(conn)?;
                diesel::insert_into(nyaa_statistics::table)
                    .values(&NewStatistic {
                        torrent_id: id,
                        seed_count: n as i32 * 10,
                        leech_count: 0,
                        download_count: 0,
                        last_updated: now,
                    })
                    .execute(conn)?;
            }

            // Search ignores case on Postgres too, like SQLite's LIKE
            let mut q = SearchQuery::from_params(
                Some("show".into()),
                Some(user.id),
                None,
                None,
                None,
                None,
                None,
                None,
                75,
                false,
            );
            let found = search(conn, &q)?;
            assert_eq!(found.total, 2);
            q.term = None;
            q.sort = SearchSort::Seeders;
            let page = search(conn, &q)?.torrents;
            let listed = with_stats(conn, page)?;
            let seeds: Vec<i32> = listed.iter().map(|t| t.seed_count).collect();
            assert_eq!(seeds, vec![20, 10, 0]);

            // Preferences insert, then update, the same row
            User::set_hide_comments(conn, user.id, true)?;
            User::set_hide_comments(conn, user.id, false)?;
            User::set_hide_comments(conn, user.id, true)?;
            assert!(User::hide_comments(conn, user.id)?);

            // The uploader sees their own hidden torrent in the general listing
            diesel::update(nyaa_torrents::table.filter(nyaa_torrents::display_name.eq("Gamma Movie")))
                .set(nyaa_torrents::flags.eq(TorrentFlags::HIDDEN.bits()))
                .execute(conn)?;
            let mut q = SearchQuery::from_params(None, None, None, None, None, None, None, None, 75, false);
            let others = search(conn, &q)?.total;
            q.viewer_id = Some(user.id);
            assert_eq!(search(conn, &q)?.total, others + 1);

            // Sessions: touch, expiry check and revocation
            diesel::insert_into(user_sessions::table)
                .values((
                    user_sessions::id.eq("pg-session"),
                    user_sessions::user_id.eq(user.id),
                    user_sessions::created_time.eq(now),
                    user_sessions::last_seen.eq(now - chrono::Duration::hours(1)),
                ))
                .execute(conn)?;
            crate::middleware::auth::touch_session(conn, "pg-session", Some(vec![10, 0, 0, 1]))?;
            let ip: Option<Vec<u8>> = users::table.find(user.id).select(users::last_login_ip).first(conn)?;
            assert_eq!(ip, Some(vec![10, 0, 0, 1]));
            assert_eq!(crate::middleware::auth::logout_everywhere(conn, user.id)?, 1);

            check_flag_migration(
                conn,
                include_str!("../../migrations/postgres/2026-10-06-160100_upstream_flag_bits/up.sql"),
            )?;
            Ok(())
        });
    }
}
