pub mod schema;

use diesel::pg::PgConnection;
use diesel::r2d2::{self, ManageConnection, R2D2Connection};
use diesel::{Connection, ConnectionResult};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

/// The site's database is PostgreSQL (`DATABASE_URL`).
pub type DbConnection = PgConnection;

pub type DbPool = r2d2::Pool<DbManager>;

/// `DATABASE_URL` when it is unset: the database in `.env.example`'s local example.
pub const DEFAULT_DATABASE_URL: &str = "postgres://nyaa:password@localhost:5432/nyaa";

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations/postgres");

pub fn connect(url: &str) -> ConnectionResult<DbConnection> {
    PgConnection::establish(url)
}

pub fn run_migrations(conn: &mut DbConnection) -> Result<(), String> {
    conn.run_pending_migrations(MIGRATIONS).map(|_| ()).map_err(|e| e.to_string())
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

/// r2d2's defaults are 10 connections and a 30 s wait for one. Database work runs on
/// actix's blocking threads (`utils::with_conn`), so a few more connections let more of it
/// run at once, and a full pool fails a request after 5 s instead of stalling it.
const POOL_SIZE: u32 = 32;
const POOL_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn init_pool(database_url: &str) -> DbPool {
    r2d2::Pool::builder()
        .max_size(POOL_SIZE)
        .connection_timeout(POOL_WAIT)
        .build(DbManager::new(database_url))
        .expect("Failed to create database pool")
}

/// The database URL for a test: a new database on the `TEST_DATABASE_URL` server (also
/// read from `.env`), copied from a migrated template, so tests stay apart.
#[cfg(test)]
pub fn test_url() -> String {
    use diesel::RunQueryDsl;
    static BASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let base = BASE.get_or_init(|| {
        dotenvy::dotenv().ok();
        let url = std::env::var("TEST_DATABASE_URL").expect(
            "tests need a PostgreSQL server: set TEST_DATABASE_URL, e.g. postgres://postgres:postgres@localhost:5432/postgres",
        );
        make_test_template(&url)
    });
    let name = format!("nyaa_test_{}_{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let mut admin = connect(base).unwrap();
    diesel::sql_query(format!("CREATE DATABASE {name} TEMPLATE nyaa_test_template")).execute(&mut admin).unwrap();
    // shortcut: drops a query string from the URL, add it back if a test server ever needs one
    format!("{}/{name}", base.rsplit_once('/').unwrap().0)
}

/// Drops the databases earlier runs left behind and migrates a fresh template.
/// shortcut: two `cargo test` runs against the same server at once drop each other's databases.
#[cfg(test)]
fn make_test_template(url: &str) -> String {
    use diesel::connection::SimpleConnection;
    use diesel::RunQueryDsl;
    #[derive(diesel::QueryableByName)]
    struct Name {
        #[diesel(sql_type = diesel::sql_types::Text)]
        datname: String,
    }
    let mut admin = connect(url).unwrap();
    let old: Vec<Name> = diesel::sql_query("SELECT datname FROM pg_database WHERE datname LIKE 'nyaa\\_test\\_%'")
        .load(&mut admin)
        .unwrap();
    for Name { datname } in old {
        diesel::sql_query(format!("DROP DATABASE {datname} WITH (FORCE)")).execute(&mut admin).unwrap();
    }
    diesel::sql_query("CREATE DATABASE nyaa_test_template").execute(&mut admin).unwrap();
    let template = format!("{}/nyaa_test_template", url.rsplit_once('/').unwrap().0);
    let mut conn = connect(&template).unwrap();
    run_migrations(&mut conn).unwrap();
    // The test fixtures write blobs as X'..', which PostgreSQL reads as a bit string
    conn.batch_execute(
        "CREATE FUNCTION test_bit_to_bytea(b bit) RETURNS bytea IMMUTABLE LANGUAGE sql AS $$
             SELECT coalesce(decode(string_agg(lpad(to_hex(substring(b FROM i FOR 8)::int), 2, '0'), ''
                 ORDER BY i), 'hex'), '') FROM generate_series(1, length(b), 8) AS i $$;
         CREATE CAST (bit AS bytea) WITH FUNCTION test_bit_to_bytea(bit) AS ASSIGNMENT;",
    )
    .unwrap();
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::{nyaa_statistics, nyaa_torrents, user_sessions, users};
    use crate::models::{NewStatistic, NewTorrent, NewUser, TorrentFlags, User};
    use crate::search::db::{search, with_stats, ListParams, SearchQuery, SearchSort};
    use diesel::prelude::*;

    /// The torrent page's comments and every POST's ban check have an index to use.
    #[test]
    fn comment_and_ban_lookups_have_indexes() {
        #[derive(diesel::QueryableByName)]
        struct Index {
            #[diesel(sql_type = diesel::sql_types::Text)]
            indexname: String,
        }
        let mut conn = connect(&test_url()).unwrap();
        let names: Vec<String> = diesel::sql_query("SELECT indexname FROM pg_indexes")
            .load::<Index>(&mut conn)
            .unwrap()
            .into_iter()
            .map(|i| i.indexname)
            .collect();
        for want in ["nyaa_comments_torrent_idx", "nyaa_comments_user_idx", "bans_user_ip_idx"] {
            assert!(names.iter().any(|n| n == want), "{want}");
        }
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

    /// The main query shapes and the flag migration, inside a transaction that is rolled back.
    #[test]
    fn postgres_migrations_and_queries() {
        let mut conn = connect(&test_url()).unwrap();
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

            // Search ignores case
            let mut q = SearchQuery::from_params(
                &ListParams { q: Some("show".into()), ..Default::default() },
                Some(user.id),
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
            let mut q = SearchQuery::from_params(&ListParams::default(), None, None, 75, false);
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

            // Two-factor: encrypted secret, one-time codes
            use crate::auth::mfa::{self, SecondFactor, UserMfa};
            let secret = mfa::new_secret();
            let codes = mfa::new_recovery_codes();
            let hashes: Vec<String> = codes.iter().map(|c| mfa::hash_recovery_code(c)).collect();
            UserMfa::enable(conn, user.id, &mfa::encrypt_secret("k", user.id, &secret), 0, &hashes)?;
            let two_factor = UserMfa::get(conn, user.id)?.unwrap();
            let code = mfa::code_at(&secret, mfa::unix_now());
            assert_eq!(two_factor.verify(conn, "k", &code)?, Some(SecondFactor::Totp));
            assert_eq!(two_factor.verify(conn, "k", &code)?, None);
            assert_eq!(two_factor.verify(conn, "k", &codes[0])?, Some(SecondFactor::RecoveryCode));
            assert_eq!(UserMfa::recovery_codes_left(conn, user.id)?, 9);
            assert!(UserMfa::disable(conn, user.id)?);

            check_flag_migration(
                conn,
                include_str!("../../migrations/postgres/2026-10-06-160100_upstream_flag_bits/up.sql"),
            )?;
            Ok(())
        });
    }
}
