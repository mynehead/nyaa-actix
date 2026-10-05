//! Command line subcommands that run instead of the web server.
//!
//! `nyaa-actix create-user <username> <password> [--level regular|trusted|moderator|admin] [--email <addr>]`
//! adds an active account to the database named by DATABASE_URL (default nyaa.db), with the
//! password hashed the same way registration does. Handy for a local admin, for example
//! `nyaa-actix create-user admin admin --level admin`; use a real password anywhere but a dev box.
//! It is safe to run while the server is up. Run the built exe directly then
//! (`.\target\debug\nyaa-actix.exe create-user ...`): after a source change `cargo run` relinks
//! the exe, which Windows refuses while the server has it open.
//!
//! `nyaa-actix migrate-storage [--dry-run]` copies the torrent info dicts and avatars from
//! TORRENT_STORAGE_PATH and AVATAR_STORAGE_PATH into the S3 bucket the S3_* settings name
//! (see `storage`), keeping the same keys. Files already in the bucket with the same size
//! are skipped, so it can be run again after an interruption, or once more right before
//! switching STORAGE_BACKEND to s3 to pick up uploads made in the meantime. Local files
//! are left in place.
//!
//! `nyaa-actix reindex` rebuilds the Meilisearch index named by MEILI_URL / MEILI_KEY /
//! MEILI_INDEX from the database. Searches keep using the old index until the new one is
//! complete. Run it once after setting up Meilisearch, and again if the index is lost.

use diesel::prelude::*;

use crate::db::DbConnection;
use crate::db::schema::users;
use crate::storage::{S3Settings, Storage};
use crate::models::user::{NewUser, User, UserLevel};

const USAGE: &str = "usage: nyaa-actix create-user <username> <password> [--level regular|trusted|moderator|admin] [--email <addr>]
       nyaa-actix migrate-storage [--dry-run]
       nyaa-actix reindex";

/// Runs the subcommand named in `args` (program name already stripped).
/// Returns None when there is no subcommand, so the caller starts the server.
pub async fn run(args: &[String]) -> Option<Result<(), String>> {
    match args.first().map(String::as_str) {
        Some("create-user") => Some(create_user(&args[1..])),
        Some("migrate-storage") => Some(migrate_storage(&args[1..]).await),
        Some("reindex") => Some(reindex()),
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}\nWith no subcommand, starts the web server.");
            Some(Ok(()))
        }
        Some(other) => Some(Err(format!("unknown subcommand `{other}`\n{USAGE}"))),
        None => None,
    }
}

struct CreateUser {
    username: String,
    password: String,
    level: UserLevel,
    email: Option<String>,
}

fn parse_create_user(args: &[String]) -> Result<CreateUser, String> {
    let mut positional = Vec::new();
    let mut level = UserLevel::Regular;
    let mut email = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--level" => {
                let v = it.next().ok_or("--level needs a value")?;
                level = parse_level(v).ok_or_else(|| format!("unknown level `{v}`"))?;
            }
            "--email" => email = Some(it.next().ok_or("--email needs a value")?.clone()),
            _ => positional.push(arg.clone()),
        }
    }
    match <[String; 2]>::try_from(positional) {
        Ok([username, password]) if !username.is_empty() && !password.is_empty() => {
            Ok(CreateUser { username, password, level, email })
        }
        _ => Err(USAGE.to_string()),
    }
}

fn parse_level(v: &str) -> Option<UserLevel> {
    match v.to_ascii_lowercase().as_str() {
        "regular" | "user" | "0" => Some(UserLevel::Regular),
        "trusted" | "1" => Some(UserLevel::Trusted),
        "moderator" | "mod" | "2" => Some(UserLevel::Moderator),
        "admin" | "superadmin" | "3" => Some(UserLevel::SuperAdmin),
        _ => None,
    }
}

/// Opens DATABASE_URL (default nyaa.db) with migrations applied.
fn open_db() -> Result<(DbConnection, String), String> {
    dotenvy::dotenv().ok();
    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "nyaa.db".into());
    let mut conn = crate::db::connect(&database_url)
        .map_err(|e| format!("cannot open {database_url}: {e}"))?;
    crate::db::run_migrations(&mut conn)
        .map_err(|e| format!("migrations failed: {e}"))?;
    Ok((conn, database_url))
}

fn create_user(args: &[String]) -> Result<(), String> {
    let opts = parse_create_user(args)?;
    let (mut conn, database_url) = open_db()?;

    if User::by_username(&mut conn, &opts.username).map_err(|e| e.to_string())?.is_some() {
        return Err(format!("user `{}` already exists", opts.username));
    }
    let mut new_user = NewUser::new(&opts.username, opts.email.as_deref(), &opts.password);
    new_user.level = opts.level as i32;
    diesel::insert_into(users::table)
        .values(&new_user)
        .execute(&mut conn)
        .map_err(|e| e.to_string())?;
    println!("created {:?} user `{}` in {database_url}", opts.level, opts.username);
    Ok(())
}

async fn migrate_storage(args: &[String]) -> Result<(), String> {
    let dry_run = match args {
        [] => false,
        [flag] if flag == "--dry-run" => true,
        _ => return Err(USAGE.to_string()),
    };
    dotenvy::dotenv().ok();
    let path = |key: &str, default: &str| std::env::var(key).unwrap_or_else(|_| default.into());
    // Same defaults as Config; read directly so this doesn't need SECRET_KEY
    let local = Storage::local(&path("TORRENT_STORAGE_PATH", "./torrents"), &path("AVATAR_STORAGE_PATH", "./avatars"))?;
    let s3 = Storage::s3(&S3Settings::from_vars(|k| std::env::var(k).ok())?)?;
    println!("{} files from {} to {}", if dry_run { "Checking" } else { "Copying" }, local.description(), s3.description());
    let (copied, skipped) = local.copy_all(&s3, dry_run).await?;
    println!("{} {copied}, already there {skipped}", if dry_run { "would copy" } else { "copied" });
    if !dry_run {
        println!("Set STORAGE_BACKEND=s3 and restart the server to use the bucket.");
    }
    Ok(())
}

fn reindex() -> Result<(), String> {
    let (mut conn, database_url) = open_db()?;
    let meili = crate::search::meili::Meili::from_env()
        .ok_or("MEILI_URL is not set; point it at Meilisearch, e.g. MEILI_URL=http://127.0.0.1:7700")?;
    let start = std::time::Instant::now();
    let count = crate::search::index::rebuild(&mut conn, &meili, |n| eprint!("\rindexed {n} torrents"))
        .map_err(|e| format!("\nreindex failed: {e:#}"))?;
    eprintln!();
    println!("indexed {count} torrents from {database_url} into `{}` in {:.1?}", meili.index(), start.elapsed());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_level_and_email_in_any_order() {
        let c = parse_create_user(&args("--level admin bob secret --email b@x.y")).unwrap();
        assert_eq!((c.username.as_str(), c.password.as_str()), ("bob", "secret"));
        assert_eq!(c.level, UserLevel::SuperAdmin);
        assert_eq!(c.email.as_deref(), Some("b@x.y"));
    }

    #[test]
    fn defaults_to_regular_and_rejects_bad_input() {
        assert_eq!(parse_create_user(&args("bob secret")).unwrap().level, UserLevel::Regular);
        assert!(parse_create_user(&args("bob")).is_err());
        assert!(parse_create_user(&args("bob secret --level god")).is_err());
        assert!(parse_create_user(&args("bob secret --level")).is_err());
    }

    #[test]
    fn created_user_can_log_in() {
        let mut conn = crate::db::connect(":memory:").unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        let mut u = NewUser::new("admin", None, "admin");
        u.level = UserLevel::SuperAdmin as i32;
        diesel::insert_into(users::table).values(&u).execute(&mut conn).unwrap();
        let user = User::by_username(&mut conn, "admin").unwrap().unwrap();
        assert!(user.verify_password("admin") && user.is_active() && user.is_superadmin());
    }
}
