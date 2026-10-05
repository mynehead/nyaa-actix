pub mod schema;

use diesel::r2d2::{self, ConnectionManager};
use diesel::SqliteConnection;

pub type DbPool = r2d2::Pool<ConnectionManager<SqliteConnection>>;

pub fn init_pool(database_url: &str) -> DbPool {
    let manager = ConnectionManager::<SqliteConnection>::new(database_url);
    r2d2::Pool::builder()
        .build(manager)
        .expect("Failed to create database pool")
}

#[cfg(test)]
mod tests {
    use diesel::prelude::*;
    use diesel_migrations::MigrationHarness;

    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Integer)]
        flags: i32,
        #[diesel(sql_type = diesel::sql_types::Integer)]
        sub_category_id: i32,
    }

    #[test]
    fn parity_migration_converts_flags_and_all_categories() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        conn.run_next_migration(crate::MIGRATIONS).unwrap(); // initial schema only
        // Old bits: HIDDEN=1, ANONYMOUS=2, REMAKE=4, TRUSTED=8; COMPLETE=16 is unchanged
        diesel::sql_query(
            "INSERT INTO nyaa_torrents (id, info_hash, display_name, torrent_name, flags, \
             main_category_id, sub_category_id) VALUES \
             (1, X'01', 'a', 'a', 1 | 8 | 16, 1, 0), (2, X'02', 'b', 'b', 2 | 4, 2, 2)",
        ).execute(&mut conn).unwrap();

        conn.run_pending_migrations(crate::MIGRATIONS).unwrap();

        let rows: Vec<Row> = diesel::sql_query("SELECT flags, sub_category_id FROM nyaa_torrents ORDER BY id")
            .load(&mut conn).unwrap();
        use crate::models::TorrentFlags as F;
        assert_eq!(rows[0].flags, (F::HIDDEN | F::TRUSTED | F::COMPLETE).bits());
        assert_eq!(rows[1].flags, (F::ANONYMOUS | F::REMAKE).bits());
        assert_eq!((rows[0].sub_category_id, rows[1].sub_category_id), (1, 2));

        let all_rows: i64 = diesel::sql_query("SELECT COUNT(*) AS flags, 0 AS sub_category_id FROM nyaa_sub_categories WHERE id = 0")
            .get_result::<Row>(&mut conn).unwrap().flags.into();
        assert_eq!(all_rows, 0);
    }

    #[test]
    fn parity_migration_reverts() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        conn.run_pending_migrations(crate::MIGRATIONS).unwrap();
        conn.revert_last_migration(crate::MIGRATIONS).unwrap();
        conn.run_pending_migrations(crate::MIGRATIONS).unwrap();
    }
}
