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
