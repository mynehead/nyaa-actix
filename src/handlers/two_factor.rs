//! Two-factor sign-in pages: the code step at /login/2fa, setting it up and turning it off
//! at /profile/2fa, and the admin reset on the user page. The codes and their storage are
//! in [`crate::auth::mfa`].

use actix_session::Session;
use actix_web::{web, HttpRequest, HttpResponse, Result};
use diesel::prelude::*;
use serde::Deserialize;
use tera::Tera;

use crate::auth::mfa::{self, SecondFactor, UserMfa, RECOVERY_CODES_LOW};
use crate::auth::policy::can_ban;
use crate::auth::{LoggedIn, Permission};
use crate::config::Config;
use crate::db::{DbConnection, DbPool};
use crate::handlers::account::{
    client_key, redirect, MfaPending, LOGIN_FAILURES_BY_ACCOUNT, LOGIN_FAILURES_BY_IP, MFA_PENDING_KEY,
};
use crate::middleware::auth::{login_user, logout_everywhere, AuthMethod};
use crate::models::{user_link, AdminLog, User};
use crate::utils::context::base_context;
use crate::utils::{client_ip, flash, internal_error};

const SETUP_URL: &str = crate::middleware::mfa_required::SETUP_URL;
/// The not yet confirmed secret shown on the setup page, Base32. Kept in the (encrypted)
/// session cookie, so reloading the page shows the same QR code.
const SETUP_SECRET_KEY: &str = "mfa_setup";

#[derive(Debug, Deserialize)]
pub struct CodeForm {
    #[serde(default)]
    pub code: String,
}

#[derive(Debug, Deserialize)]
pub struct ConfirmForm {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub current_password: String,
}

fn render(tmpl: &Tera, name: &str, ctx: &tera::Context, status: actix_web::http::StatusCode) -> Result<HttpResponse> {
    let html = tmpl.render(name, ctx).map_err(internal_error)?;
    Ok(HttpResponse::build(status).content_type("text/html").body(html))
}

fn login_page(tmpl: &Tera, cfg: &Config, error: &str, status: actix_web::http::StatusCode) -> Result<HttpResponse> {
    let mut ctx = base_context(cfg, None);
    ctx.insert("error", error);
    render(tmpl, "login_2fa.html", &ctx, status)
}

/// The second step of logging in, after the right password.
pub async fn login_2fa_get(session: Session, tmpl: web::Data<Tera>, cfg: web::Data<Config>) -> Result<HttpResponse> {
    if MfaPending::get(&session).is_none() {
        return Ok(redirect("/login"));
    }
    let mut ctx = base_context(&cfg, None);
    ctx.insert("error", &Option::<String>::None);
    render(&tmpl, "login_2fa.html", &ctx, actix_web::http::StatusCode::OK)
}

pub async fn login_2fa_post(
    req: HttpRequest,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    form: web::Form<CodeForm>,
) -> Result<HttpResponse> {
    use actix_web::http::StatusCode;
    let Some(pending) = MfaPending::get(&session) else {
        session.remove(MFA_PENDING_KEY);
        flash::push(&session, "warning", "", "Your sign-in timed out. Please log in again.");
        return Ok(redirect("/login"));
    };
    // The same limits as wrong passwords: 10 per account, so six digits can't be guessed
    let (ip, account) = (client_key(&req), format!("user:{}", pending.user_id));
    if LOGIN_FAILURES_BY_IP.is_blocked(&ip) || LOGIN_FAILURES_BY_ACCOUNT.is_blocked(&account) {
        return login_page(
            &tmpl,
            &cfg,
            "Too many failed login attempts. Try again in 15 minutes.",
            StatusCode::TOO_MANY_REQUESTS,
        );
    }

    let mut conn = pool.get().map_err(internal_error)?;
    let user = User::by_id(&mut conn, pending.user_id).map_err(internal_error)?.filter(User::is_active);
    let Some(user) = user else {
        session.remove(MFA_PENDING_KEY);
        return Ok(redirect("/login"));
    };
    let method = match UserMfa::get(&mut conn, user.id).map_err(internal_error)? {
        // Turned off (by an admin) since the password step: the password is enough again
        None => AuthMethod::Password,
        Some(mfa) => match mfa.verify(&mut conn, &cfg.secret_key, &form.code).map_err(internal_error)? {
            Some(SecondFactor::Totp) => AuthMethod::PasswordTotp,
            Some(SecondFactor::RecoveryCode) => AuthMethod::PasswordRecovery,
            None => {
                LOGIN_FAILURES_BY_IP.hit(&ip);
                LOGIN_FAILURES_BY_ACCOUNT.hit(&account);
                return login_page(&tmpl, &cfg, "Invalid code.", StatusCode::UNAUTHORIZED);
            }
        },
    };

    session.remove(MFA_PENDING_KEY);
    LOGIN_FAILURES_BY_ACCOUNT.clear(&account);
    login_user(&session, &mut conn, user.id, client_ip(&req), method).map_err(internal_error)?;
    if method == AuthMethod::PasswordRecovery {
        let left = UserMfa::recovery_codes_left(&mut conn, user.id).map_err(internal_error)?;
        log::info!("User {} signed in with a recovery code; {} left", user.id, left);
        if left <= RECOVERY_CODES_LOW {
            flash::push(
                &session,
                "warning",
                "Recovery code used.",
                &format!("You have {left} left. Create new ones on your profile's Two-factor page."),
            );
        }
    }
    Ok(redirect("/"))
}

/// The setup page (QR code and confirmation form) or, once set up, the management page.
pub async fn page(
    LoggedIn(user): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let mut ctx = base_context(&cfg, Some(&user));
    ctx.insert("flash_messages", &flash::take(&session));
    ctx.insert("required", &cfg.mfa.required_for(user.level()));
    match UserMfa::get(&mut conn, user.id).map_err(internal_error)? {
        Some(mfa) => {
            ctx.insert("enabled", &true);
            ctx.insert("enabled_time", &mfa.enabled_time);
            ctx.insert(
                "recovery_codes_left",
                &UserMfa::recovery_codes_left(&mut conn, user.id).map_err(internal_error)?,
            );
        }
        None => {
            let secret = session
                .get::<String>(SETUP_SECRET_KEY)
                .ok()
                .flatten()
                .and_then(|s| mfa::secret_from_base32(&s))
                .unwrap_or_else(mfa::new_secret);
            let base32 = mfa::secret_base32(&secret);
            session.insert(SETUP_SECRET_KEY, &base32)?;
            let url = mfa::otpauth_url(&secret, &cfg.mfa.issuer, &user.username);
            ctx.insert("enabled", &false);
            ctx.insert("qr_svg", &mfa::qr_svg(&url));
            // Groups of four, easier to type
            let grouped: Vec<String> =
                base32.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
            ctx.insert("secret", &grouped.join(" "));
        }
    }
    render(&tmpl, "two_factor.html", &ctx, actix_web::http::StatusCode::OK)
}

/// Checks the password off the async workers; wrong ones count like failed logins.
async fn password_ok(req: &HttpRequest, user: &User, password: &str) -> Result<bool> {
    let account = format!("user:{}", user.id);
    if LOGIN_FAILURES_BY_ACCOUNT.is_blocked(&account) || LOGIN_FAILURES_BY_IP.is_blocked(&client_key(req)) {
        return Ok(false);
    }
    let (checked, password) = (user.clone(), password.to_string());
    let ok = web::block(move || checked.verify_password(&password)).await?;
    if !ok {
        LOGIN_FAILURES_BY_ACCOUNT.hit(&account);
        LOGIN_FAILURES_BY_IP.hit(&client_key(req));
    }
    Ok(ok)
}

/// Shows freshly made recovery codes, once.
fn codes_page(tmpl: &Tera, cfg: &Config, user: &User, codes: &[String]) -> Result<HttpResponse> {
    let mut ctx = base_context(cfg, Some(user));
    ctx.insert("codes", codes);
    render(tmpl, "two_factor_codes.html", &ctx, actix_web::http::StatusCode::OK)
}

/// New recovery codes for `user_id`: the plain codes to show, after storing their hashes.
fn replace_recovery_codes(conn: &mut DbConnection, user_id: i32) -> QueryResult<Vec<String>> {
    let codes = mfa::new_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| mfa::hash_recovery_code(c)).collect();
    UserMfa::store_recovery_codes(conn, user_id, &hashes)?;
    Ok(codes)
}

/// Confirms the setup with the current password and a code from the app.
pub async fn enable_post(
    LoggedIn(user): LoggedIn,
    req: HttpRequest,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    form: web::Form<ConfirmForm>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    if UserMfa::is_enabled(&mut conn, user.id).map_err(internal_error)? {
        return Ok(redirect(SETUP_URL));
    }
    let secret = session.get::<String>(SETUP_SECRET_KEY).ok().flatten().and_then(|s| mfa::secret_from_base32(&s));
    let Some(secret) = secret else {
        flash::push(&session, "danger", "Setup failed!", "The setup expired; scan the new QR code.");
        return Ok(redirect(SETUP_URL));
    };
    if !password_ok(&req, &user, &form.current_password).await? {
        flash::push(&session, "danger", "Setup failed!", "Incorrect password.");
        return Ok(redirect(SETUP_URL));
    }
    let Some(step) = mfa::check_code_now(&secret, &form.code) else {
        flash::push(&session, "danger", "Setup failed!", "That code is not right. Check the time on your device.");
        return Ok(redirect(SETUP_URL));
    };

    let codes = mfa::new_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| mfa::hash_recovery_code(c)).collect();
    let encrypted = mfa::encrypt_secret(&cfg.secret_key, user.id, &secret);
    UserMfa::enable(&mut conn, user.id, &encrypted, step, &hashes).map_err(internal_error)?;
    session.remove(SETUP_SECRET_KEY);
    // Sessions signed in with the password alone end; this one starts over as two-factor
    logout_everywhere(&mut conn, user.id).map_err(internal_error)?;
    login_user(&session, &mut conn, user.id, client_ip(&req), AuthMethod::PasswordTotp).map_err(internal_error)?;
    log::info!("User {} turned on two-factor sign-in", user.id);
    codes_page(&tmpl, &cfg, &user, &codes)
}

/// Checks password and a current code (or recovery code, which is used up) before
/// changing two-factor; pushes the flash and gives None when either is wrong.
async fn confirm(
    req: &HttpRequest,
    session: &Session,
    conn: &mut DbConnection,
    cfg: &Config,
    user: &User,
    form: &ConfirmForm,
) -> Result<Option<UserMfa>> {
    let Some(mfa) = UserMfa::get(conn, user.id).map_err(internal_error)? else {
        return Ok(None);
    };
    if !password_ok(req, user, &form.current_password).await? {
        flash::push(session, "danger", "Change failed!", "Incorrect password.");
        return Ok(None);
    }
    if mfa.verify(conn, &cfg.secret_key, &form.code).map_err(internal_error)?.is_none() {
        LOGIN_FAILURES_BY_ACCOUNT.hit(&format!("user:{}", user.id));
        flash::push(session, "danger", "Change failed!", "Invalid code.");
        return Ok(None);
    }
    Ok(Some(mfa))
}

pub async fn disable_post(
    LoggedIn(user): LoggedIn,
    req: HttpRequest,
    session: Session,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    form: web::Form<ConfirmForm>,
) -> Result<HttpResponse> {
    if cfg.mfa.required_for(user.level()) {
        flash::push(&session, "danger", "", "Your account class must use two-factor sign-in.");
        return Ok(redirect(SETUP_URL));
    }
    let mut conn = pool.get().map_err(internal_error)?;
    if confirm(&req, &session, &mut conn, &cfg, &user, &form).await?.is_none() {
        return Ok(redirect(SETUP_URL));
    }
    UserMfa::disable(&mut conn, user.id).map_err(internal_error)?;
    log::info!("User {} turned off two-factor sign-in", user.id);
    flash::push(&session, "success", "Two-factor sign-in turned off.", "");
    Ok(redirect("/profile"))
}

pub async fn recovery_codes_post(
    LoggedIn(user): LoggedIn,
    req: HttpRequest,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    form: web::Form<ConfirmForm>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    if confirm(&req, &session, &mut conn, &cfg, &user, &form).await?.is_none() {
        return Ok(redirect(SETUP_URL));
    }
    let codes = replace_recovery_codes(&mut conn, user.id).map_err(internal_error)?;
    log::info!("User {} made new recovery codes", user.id);
    codes_page(&tmpl, &cfg, &user, &codes)
}

/// "Reset two-factor" on the user page: admins only, on users below them.
pub async fn admin_reset_post(
    LoggedIn(admin): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    path: web::Path<String>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let user = User::by_username(&mut conn, &path.into_inner())
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("User not found"))?;
    if !admin.can(Permission::ResetTwoFactor) || !can_ban(&admin, &user) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let reset = conn
        .transaction::<_, diesel::result::Error, _>(|conn| {
            let reset = UserMfa::disable(conn, user.id)?;
            if reset {
                AdminLog::add(
                    conn,
                    admin.id,
                    &format!("Two-factor sign-in of {} was reset", user_link(&user.username)),
                )?;
            }
            Ok(reset)
        })
        .map_err(internal_error)?;
    if reset {
        flash::push(&session, "success", "", &format!("Two-factor sign-in of {} was reset.", user.username));
    } else {
        flash::push(&session, "danger", "", "This user has no two-factor sign-in.");
    }
    Ok(HttpResponse::SeeOther()
        .insert_header(("Location", format!("/user/{}", urlencoding::encode(&user.username))))
        .finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::{user_sessions, users};
    use crate::middleware::auth::test_support::login as login_as;
    use crate::models::{NewUser, UserLevel};
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::cookie::{Cookie, Key};
    use actix_web::http::StatusCode;
    use actix_web::{test, App};
    use diesel::r2d2::Pool;

    const PASSWORD: &str = "hunter22";

    /// alice (1, regular), bob (2, regular), root (3, admin), dave (77, regular; only the
    /// throttle test fails codes for him, as the limits are process-wide).
    fn pool() -> DbPool {
        let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(":memory:")).unwrap();
        let mut conn = pool.get().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        for (name, level) in
            [("alice", UserLevel::Regular), ("bob", UserLevel::Regular), ("root", UserLevel::SuperAdmin)]
        {
            let mut user = NewUser::new(name, Some(&format!("{name}@example.com")), PASSWORD);
            user.level = level as i32;
            diesel::insert_into(users::table).values(&user).execute(&mut conn).unwrap();
        }
        diesel::insert_into(users::table)
            .values((
                users::id.eq(77),
                users::username.eq("dave"),
                users::password_hash.eq(crate::models::hash_password(PASSWORD)),
                users::status.eq(1),
                users::level.eq(0),
                users::created_time.eq(chrono::Utc::now().naive_utc()),
            ))
            .execute(&mut conn)
            .unwrap();
        pool
    }

    fn config() -> Config {
        let mut cfg = Config::for_tests();
        cfg.secret_key = "test key".into();
        cfg.mfa.issuer = "Nyaa".into();
        cfg
    }

    macro_rules! app {
        ($pool:expr, $cfg:expr) => {{
            let mut tera = Tera::new("templates/**/*").unwrap();
            crate::utils::tera_filters::register(&mut tera);
            test::init_service(
                App::new()
                    .app_data(web::Data::new($cfg.clone()))
                    .app_data(web::Data::new($pool.clone()))
                    .app_data(web::Data::new(tera))
                    .wrap(actix_web::middleware::from_fn(crate::middleware::mfa_required::require_two_factor))
                    .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                    .route("/", web::get().to(HttpResponse::Ok))
                    .route("/test-login/{id}", web::get().to(login_as))
                    .route("/login", web::post().to(crate::handlers::account::login_post))
                    .route("/login/2fa", web::get().to(login_2fa_get))
                    .route("/login/2fa", web::post().to(login_2fa_post))
                    .route("/profile/2fa", web::get().to(page))
                    .route("/profile/2fa/enable", web::post().to(enable_post))
                    .route("/profile/2fa/disable", web::post().to(disable_post))
                    .route("/profile/2fa/recovery-codes", web::post().to(recovery_codes_post))
                    .route("/user/{username}/reset-2fa", web::post().to(admin_reset_post)),
            )
            .await
        }};
    }

    /// Sends the request with `cookie`, keeps a renewed cookie; gives status, Location and body.
    macro_rules! call {
        ($app:expr, $cookie:expr, $req:expr) => {{
            let req = $req;
            let req = match $cookie.as_ref() {
                Some(c) => req.cookie(Cookie::clone(c)),
                None => req,
            };
            let res = test::call_service(&$app, req.to_request()).await;
            if let Some(c) = res.response().cookies().next() {
                *$cookie = Some(c.into_owned());
            }
            let status = res.status();
            let location = res.headers().get("Location").map(|l| l.to_str().unwrap().to_string());
            let body = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
            (status, location, body)
        }};
    }

    fn get(uri: &str) -> test::TestRequest {
        test::TestRequest::get().uri(uri)
    }

    fn post(uri: &str, form: &[(&str, &str)]) -> test::TestRequest {
        test::TestRequest::post().uri(uri).set_form(form)
    }

    fn between<'a>(s: &'a str, start: &str, end: &str) -> &'a str {
        let from = s.find(start).unwrap_or_else(|| panic!("no {start} in {s}")) + start.len();
        &s[from..from + s[from..].find(end).unwrap()]
    }

    /// The codes on the recovery codes page.
    fn recovery_codes(html: &str) -> Vec<String> {
        let pre = between(html, "<pre id=\"recovery_codes\"", "</pre>");
        pre[pre.find('>').unwrap() + 1..].split_whitespace().map(String::from).collect()
    }

    fn enabled(pool: &DbPool, user_id: i32) -> bool {
        UserMfa::is_enabled(&mut pool.get().unwrap(), user_id).unwrap()
    }

    #[actix_web::test]
    async fn set_up_log_in_with_a_code_and_turn_off() {
        let (pool, cfg) = (pool(), config());
        let app = app!(pool, cfg);
        let mut cookie: Option<Cookie<'static>> = None;
        call!(app, &mut cookie, get("/test-login/1"));

        let (status, _, html) = call!(app, &mut cookie, get("/profile/2fa"));
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("<svg"), "{html}");
        let key = between(&html, "<code id=\"totp_secret\">", "</code>").replace(' ', "");
        let secret = mfa::secret_from_base32(&key).unwrap();
        // Reloading keeps the same key, so a scanned code still matches
        let (_, _, again) = call!(app, &mut cookie, get("/profile/2fa"));
        assert!(again.contains(between(&html, "<code id=\"totp_secret\">", "</code>")));

        let code = mfa::code_at(&secret, mfa::unix_now());
        let (_, location, _) =
            call!(app, &mut cookie, post("/profile/2fa/enable", &[("code", &code), ("current_password", "wrong")]));
        assert_eq!(location.as_deref(), Some(SETUP_URL));
        assert!(!enabled(&pool, 1));
        let (status, _, html) =
            call!(app, &mut cookie, post("/profile/2fa/enable", &[("code", &code), ("current_password", PASSWORD)]));
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(enabled(&pool, 1));
        let codes = recovery_codes(&html);
        assert_eq!(codes.len(), 10, "{codes:?}");
        // Stored encrypted, not as the raw secret
        let stored: Vec<u8> = crate::db::schema::user_mfa::table
            .find(1)
            .select(crate::db::schema::user_mfa::totp_secret)
            .first(&mut pool.get().unwrap())
            .unwrap();
        assert!(!stored.windows(secret.len()).any(|w| w == secret.as_slice()));

        // A fresh login now stops at the code step
        let mut login: Option<Cookie<'static>> = None;
        let (_, location, _) = call!(app, &mut login, post("/login", &[("username", "alice"), ("password", PASSWORD)]));
        assert_eq!(location.as_deref(), Some("/login/2fa"));
        let (status, _, _) = call!(app, &mut login, get("/profile/2fa"));
        assert_eq!(status, StatusCode::UNAUTHORIZED, "not signed in yet");
        let (status, _, html) = call!(app, &mut login, post("/login/2fa", &[("code", "000000")]));
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(html.contains("Invalid code."));
        // The code that confirmed the setup was used up; a recovery code works
        let (status, _, _) = call!(app, &mut login, post("/login/2fa", &[("code", &code)]));
        assert_eq!(status, StatusCode::UNAUTHORIZED, "replayed code");
        let (_, location, _) = call!(app, &mut login, post("/login/2fa", &[("code", &codes[0])]));
        assert_eq!(location.as_deref(), Some("/"));
        let (status, _, html) = call!(app, &mut login, get("/profile/2fa"));
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("<strong id=\"recovery_codes_left\">9</strong>"), "{html}");
        let methods: Vec<String> = user_sessions::table
            .select(user_sessions::auth_method)
            .order(user_sessions::created_time)
            .load(&mut pool.get().unwrap())
            .unwrap();
        assert!(methods.contains(&"password+recovery".to_string()), "{methods:?}");
        // The pending login is gone once used
        let (_, location, _) = call!(app, &mut login, post("/login/2fa", &[("code", &codes[1])]));
        assert_eq!(location.as_deref(), Some("/login"));

        // New recovery codes need password and a code; the old ones stop working
        let (status, _, html) = call!(
            app,
            &mut login,
            post("/profile/2fa/recovery-codes", &[("code", &codes[2]), ("current_password", PASSWORD)])
        );
        assert_eq!(status, StatusCode::OK);
        let new_codes = recovery_codes(&html);
        assert_eq!(UserMfa::recovery_codes_left(&mut pool.get().unwrap(), 1).unwrap(), 10);

        let (_, location, _) = call!(
            app,
            &mut login,
            post("/profile/2fa/disable", &[("code", &codes[3]), ("current_password", PASSWORD)])
        );
        assert_eq!(location.as_deref(), Some(SETUP_URL));
        assert!(enabled(&pool, 1), "old recovery code");
        let (_, location, _) = call!(
            app,
            &mut login,
            post("/profile/2fa/disable", &[("code", &new_codes[0]), ("current_password", PASSWORD)])
        );
        assert_eq!(location.as_deref(), Some("/profile"));
        assert!(!enabled(&pool, 1));

        // Back to password only
        let mut again: Option<Cookie<'static>> = None;
        let (_, location, _) = call!(app, &mut again, post("/login", &[("username", "alice"), ("password", PASSWORD)]));
        assert_eq!(location.as_deref(), Some("/"));
    }

    #[actix_web::test]
    async fn wrong_codes_count_towards_the_login_limit() {
        let (pool, cfg) = (pool(), config());
        let app = app!(pool, cfg);
        let secret = mfa::new_secret();
        UserMfa::enable(&mut pool.get().unwrap(), 77, &mfa::encrypt_secret(&cfg.secret_key, 77, &secret), 0, &[])
            .unwrap();
        let mut login: Option<Cookie<'static>> = None;
        for _ in 0..10 {
            // The right password again doesn't reset the count
            let (_, location, _) =
                call!(app, &mut login, post("/login", &[("username", "dave"), ("password", PASSWORD)]));
            assert_eq!(location.as_deref(), Some("/login/2fa"));
            let (status, _, _) = call!(app, &mut login, post("/login/2fa", &[("code", "123456")]));
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let code = mfa::code_at(&secret, mfa::unix_now());
        let (status, _, _) = call!(app, &mut login, post("/login/2fa", &[("code", &code)]));
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[actix_web::test]
    async fn admins_reset_and_required_level_forces_setup() {
        let (pool, mut cfg) = (pool(), config());
        cfg.mfa.required_level = Some(UserLevel::Moderator);
        let app = app!(pool, cfg);
        let enable = |id: i32| {
            UserMfa::enable(
                &mut pool.get().unwrap(),
                id,
                &mfa::encrypt_secret("test key", id, &mfa::new_secret()),
                0,
                &[],
            )
            .unwrap()
        };
        enable(1);

        // root has no two-factor yet: everything but the setup page sends them there
        let mut root: Option<Cookie<'static>> = None;
        call!(app, &mut root, get("/test-login/3"));
        let (status, location, _) = call!(app, &mut root, get("/"));
        assert_eq!((status, location.as_deref()), (StatusCode::SEE_OTHER, Some(SETUP_URL)));
        let (_, location, _) = call!(app, &mut root, post("/user/alice/reset-2fa", &[]));
        assert_eq!(location.as_deref(), Some(SETUP_URL));
        assert!(enabled(&pool, 1));
        let (status, _, html) = call!(app, &mut root, get(SETUP_URL));
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("required for your account"));

        enable(3);
        let (status, _, _) = call!(app, &mut root, get("/"));
        assert_eq!(status, StatusCode::OK);
        let (_, location, _) = call!(app, &mut root, post("/user/alice/reset-2fa", &[]));
        assert_eq!(location.as_deref(), Some("/user/alice"));
        assert!(!enabled(&pool, 1));
        let (entries, _) = AdminLog::page(&mut pool.get().unwrap(), 1, 10).unwrap();
        assert_eq!(entries[0].entry.log, "Two-factor sign-in of [alice](/user/alice) was reset");

        // Regular users can't reset anyone, and required users can't turn it off
        enable(1);
        let mut bob: Option<Cookie<'static>> = None;
        call!(app, &mut bob, get("/test-login/2"));
        let (status, _, _) = call!(app, &mut bob, post("/user/alice/reset-2fa", &[]));
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(enabled(&pool, 1));
        let (_, location, _) =
            call!(app, &mut root, post("/profile/2fa/disable", &[("code", "x"), ("current_password", PASSWORD)]));
        assert_eq!(location.as_deref(), Some(SETUP_URL));
        assert!(enabled(&pool, 3));
    }
}
