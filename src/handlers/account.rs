use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use actix_multipart::Multipart;
use actix_session::Session;
use actix_web::{web, HttpRequest, HttpResponse, Result};
use diesel::prelude::*;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tera::Tera;

use crate::auth::mfa::UserMfa;
use crate::auth::CurrentUser;
use crate::config::Config;
use crate::db::schema::users;
use crate::db::{DbConnection, DbPool};
use crate::middleware::auth::{login_user, logout_everywhere, logout_user, session_auth_method, AuthMethod};
use crate::models::{password_matches, Ban, NewUser, User};
use crate::storage::{Kind, Storage};
use crate::utils::context::base_context;
use crate::utils::throttle::Throttle;
use crate::utils::{avatar, client_addr, client_ip, flash, internal_error};

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct RegisterForm {
    pub username: String,
    pub email: String,
    pub password: String,
    pub password_confirm: String,
}

pub async fn login_get(
    CurrentUser(current_user): CurrentUser,
    session: Session,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    if current_user.is_some() {
        return Ok(HttpResponse::Found().insert_header(("Location", "/")).finish());
    }
    let mut ctx = base_context(&cfg, None);
    ctx.insert("flash_messages", &flash::take(&session));
    ctx.insert("error", &Option::<String>::None);
    let html = tmpl.render("login.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn login_post(
    req: HttpRequest,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    form: web::Form<LoginForm>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let user = User::by_username_or_email(&mut conn, &form.username).map_err(internal_error)?;

    // Count by account, so trying the username and then the email address shares one limit
    let ip = client_key(&req);
    let account = match &user {
        Some(u) => format!("user:{}", u.id),
        None => format!("name:{}", form.username.trim().to_lowercase()),
    };
    if LOGIN_FAILURES_BY_IP.is_blocked(&ip) || LOGIN_FAILURES_BY_ACCOUNT.is_blocked(&account) {
        let mut ctx = base_context(&cfg, None);
        ctx.insert("error", "Too many failed login attempts. Try again in 15 minutes.");
        ctx.insert("username", &form.username);
        let html = tmpl.render("login.html", &ctx).map_err(internal_error)?;
        return Ok(HttpResponse::TooManyRequests().content_type("text/html").body(html));
    }

    // Argon2 takes tens of milliseconds of CPU; keep it off the async workers
    let (checked_user, password) = (user.clone(), form.password.clone());
    let password_ok = web::block(move || password_matches(checked_user.as_ref(), &password)).await?;
    if !password_ok {
        LOGIN_FAILURES_BY_IP.hit(&ip);
        LOGIN_FAILURES_BY_ACCOUNT.hit(&account);
    }

    let error = match user {
        Some(ref u) if password_ok && u.is_active() => {
            return complete_login(&session, &mut conn, u, client_ip(&req));
        }
        // Like upstream, the ban (and its reason) only shows after the right password
        Some(ref u) if password_ok && u.is_banned() => {
            let reason =
                Ban::banned(&mut conn, Some(u.id), None).map_err(internal_error)?.into_iter().next().map(|b| b.reason);
            Some(match reason {
                Some(reason) => format!(
                    "You are banned with the reason \"{}\" If you believe that this \
                                         is a mistake, contact a moderator.",
                    reason
                ),
                None => "Your account has been banned.".to_string(),
            })
        }
        _ => Some("Invalid username or password.".to_string()),
    };

    let mut ctx = base_context(&cfg, None);
    ctx.insert("error", &error);
    ctx.insert("username", &form.username);
    let html = tmpl.render("login.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().status(actix_web::http::StatusCode::UNAUTHORIZED).content_type("text/html").body(html))
}

pub async fn register_get(
    CurrentUser(current_user): CurrentUser,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    if current_user.is_some() {
        return Ok(HttpResponse::Found().insert_header(("Location", "/")).finish());
    }
    let mut ctx = base_context(&cfg, None);
    ctx.insert("errors", &Vec::<String>::new());
    let html = tmpl.render("register.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn register_post(
    req: HttpRequest,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    form: web::Form<RegisterForm>,
) -> Result<HttpResponse> {
    let ip = client_key(&req);
    if REGISTRATIONS_BY_IP.is_blocked(&ip) {
        let mut ctx = base_context(&cfg, None);
        ctx.insert("errors", &["Too many registrations from your address. Try again later."]);
        ctx.insert("username", &form.username);
        ctx.insert("email", &form.email);
        let html = tmpl.render("register.html", &ctx).map_err(internal_error)?;
        return Ok(HttpResponse::TooManyRequests().content_type("text/html").body(html));
    }
    REGISTRATIONS_BY_IP.hit(&ip);

    let mut conn = pool.get().map_err(internal_error)?;
    let (username, email) = (form.username.trim(), form.email.trim());
    let mut errors = register_errors(username, email, &form.password, &form.password_confirm);
    if User::username_taken(&mut conn, username).map_err(internal_error)? {
        errors.push("Username is already taken.".into());
    }
    if User::by_email(&mut conn, email).map_err(internal_error)?.is_some() {
        errors.push("Email is already in use.".into());
    }

    let render_errors = |errors: &[String]| -> Result<HttpResponse> {
        let mut ctx = base_context(&cfg, None);
        ctx.insert("errors", errors);
        ctx.insert("username", username);
        ctx.insert("email", email);
        let html = tmpl.render("register.html", &ctx).map_err(internal_error)?;
        Ok(HttpResponse::BadRequest().content_type("text/html").body(html))
    };
    if !errors.is_empty() {
        return render_errors(&errors);
    }

    let (name, mail, password) = (username.to_string(), email.to_string(), form.password.clone());
    let mut new_user = web::block(move || NewUser::new(&name, Some(&mail), &password)).await?;
    new_user.registration_ip = client_ip(&req);
    // Two sign-ups for the same name or email at once: the second hits the UNIQUE index
    match diesel::insert_into(users::table).values(&new_user).execute(&mut conn) {
        Err(diesel::result::Error::DatabaseError(diesel::result::DatabaseErrorKind::UniqueViolation, _)) => {
            return render_errors(&["Username or email is already taken.".to_string()]);
        }
        result => result.map_err(internal_error)?,
    };

    let user = User::by_username(&mut conn, username)
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorInternalServerError("Failed to fetch user"))?;

    complete_login(&session, &mut conn, &user, client_ip(&req))
}

/// Session key of a login waiting for its second factor at /login/2fa.
pub(crate) const MFA_PENDING_KEY: &str = "mfa_pending";
/// How long the second step may take.
const MFA_PENDING_SECS: i64 = 5 * 60;

/// A user whose password was right but who still has to enter a two-factor code.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MfaPending {
    pub user_id: i32,
    /// Unix time after which the password has to be entered again.
    pub expires: i64,
}

impl MfaPending {
    /// The pending login in `session`, if it hasn't expired.
    pub(crate) fn get(session: &Session) -> Option<MfaPending> {
        let pending: MfaPending = session.get(MFA_PENDING_KEY).ok()??;
        (pending.expires > chrono::Utc::now().timestamp()).then_some(pending)
    }
}

/// The one way a checked password (login or registration) becomes a session. Users with
/// two-factor go to /login/2fa first; everyone else is signed in right away. Any other
/// way of signing in (such as SSO, later) should end here too.
pub(crate) fn complete_login(
    session: &Session,
    conn: &mut DbConnection,
    user: &User,
    ip: Option<Vec<u8>>,
) -> Result<HttpResponse> {
    if UserMfa::is_enabled(conn, user.id).map_err(internal_error)? {
        // Failed logins stay counted until the second factor is right too, so knowing the
        // password doesn't reset the limit on guessing codes
        let expires = chrono::Utc::now().timestamp() + MFA_PENDING_SECS;
        session.insert(MFA_PENDING_KEY, MfaPending { user_id: user.id, expires })?;
        return Ok(redirect("/login/2fa"));
    }
    LOGIN_FAILURES_BY_ACCOUNT.clear(&format!("user:{}", user.id));
    // Also records last_login_date and last_login_ip, which IP bans from the user page use
    login_user(session, conn, user.id, ip, AuthMethod::Password).map_err(internal_error)?;
    Ok(redirect("/"))
}

/// Upstream's `RegisterForm` rules, counted in characters: usernames are 3 to 32 ASCII
/// letters, digits, `_` or `-`; the email is required; passwords are 6 to 1024 characters.
fn register_errors(username: &str, email: &str, password: &str, password_confirm: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if !(3..=32).contains(&username.chars().count()) {
        errors.push("Username must be 3–32 characters.".into());
    }
    if !username.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        errors.push("Username may only contain letters, numbers, _ and -.".into());
    }
    if !looks_like_email(email) || !(5..=128).contains(&email.chars().count()) {
        errors.push("Please enter a valid email address.".into());
    }
    if password != password_confirm {
        errors.push("Passwords do not match.".into());
    }
    if !(6..=1024).contains(&password.chars().count()) {
        errors.push("Password must be 6–1024 characters.".into());
    }
    errors
}

/// Failed logins allowed per address and per account name before a 15-minute pause.
/// The per-address limit is high because users behind one NAT or proxy share it.
pub(crate) static LOGIN_FAILURES_BY_IP: LazyLock<Throttle> =
    LazyLock::new(|| Throttle::new(50, Duration::from_secs(15 * 60)));
pub(crate) static LOGIN_FAILURES_BY_ACCOUNT: LazyLock<Throttle> =
    LazyLock::new(|| Throttle::new(10, Duration::from_secs(15 * 60)));
/// Registration attempts per address per hour.
static REGISTRATIONS_BY_IP: LazyLock<Throttle> = LazyLock::new(|| Throttle::new(20, Duration::from_secs(60 * 60)));

pub(crate) fn client_key(req: &HttpRequest) -> String {
    format!("ip:{}", client_addr(req).map(|a| a.to_string()).unwrap_or_default())
}

pub async fn logout(session: Session, pool: web::Data<DbPool>) -> HttpResponse {
    logout_user(&session, &pool);
    HttpResponse::Found().insert_header(("Location", "/")).finish()
}

/// The account pages used to live under `/account/`; they now sit at the root like upstream.
/// 308 keeps the method, so an old login form still posts to the right place.
pub async fn legacy_redirect(req: HttpRequest, path: web::Path<String>) -> HttpResponse {
    let page = path.into_inner();
    if !matches!(page.as_str(), "login" | "register" | "logout" | "profile" | "profile/avatar") {
        return HttpResponse::NotFound().finish();
    }
    let mut location = format!("/{}", page);
    if !req.query_string().is_empty() {
        location = format!("{}?{}", location, req.query_string());
    }
    HttpResponse::PermanentRedirect().insert_header(("Location", location)).finish()
}

const PROFILE_URL: &str = "/profile";

pub(crate) fn redirect(location: &str) -> HttpResponse {
    HttpResponse::Found().insert_header(("Location", location)).finish()
}

/// Both tab forms of upstream's `ProfileForm`: "Password" and "Email" post
/// `authorized_submit`, "Preferences" posts `submit_settings`.
#[derive(Debug, Default, Deserialize)]
pub struct ProfileForm {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub current_password: String,
    #[serde(default)]
    pub new_password: String,
    #[serde(default)]
    pub password_confirm: String,
    pub hide_comments: Option<String>,
    pub authorized_submit: Option<String>,
    pub submit_settings: Option<String>,
    /// Which tab sent the form, to show its errors there.
    pub tab: Option<String>,
}

type FieldErrors = HashMap<&'static str, Vec<String>>;

/// Good enough to catch typos; there is no verification mail yet to prove it works.
fn looks_like_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else { return false };
    !local.is_empty()
        && !domain.contains('@')
        && !s.chars().any(char::is_whitespace)
        && domain.split('.').count() >= 2
        && domain.split('.').all(|part| !part.is_empty())
}

impl ProfileForm {
    /// Upstream's `ProfileForm` validators, with its messages.
    fn validate(&self, conn: &mut DbConnection) -> QueryResult<FieldErrors> {
        let mut errors = FieldErrors::new();
        let mut add = |field, msg: &str| errors.entry(field).or_default().push(msg.to_string());
        if self.current_password.is_empty() {
            add("current_password", "This field is required.");
        }
        let email = self.email.trim();
        if !email.is_empty() {
            if !looks_like_email(email) {
                add("email", "Invalid email address.");
            }
            if !(5..=128).contains(&email.chars().count()) {
                add("email", "Field must be between 5 and 128 characters long.");
            }
            if User::by_email(conn, email)?.is_some() {
                add("email", "This email address has been taken");
            }
        }
        if !self.new_password.is_empty() {
            if self.new_password != self.password_confirm {
                add("new_password", "Two passwords must match");
            }
            if !(6..=1024).contains(&self.new_password.chars().count()) {
                add("new_password", "Password must be at least 6 characters long.");
            }
        }
        Ok(errors)
    }
}

/// The profile page; `errors` go to the form of `active_tab` ("password", "email" or "preferences").
#[allow(clippy::too_many_arguments)]
fn render_profile(
    session: &Session,
    conn: &mut DbConnection,
    tmpl: &Tera,
    cfg: &Config,
    user: &User,
    active_tab: &str,
    errors: FieldErrors,
    email_value: &str,
) -> Result<HttpResponse> {
    let hide_comments = User::hide_comments(conn, user.id).map_err(internal_error)?;
    let no_errors = FieldErrors::new();
    let mut ctx = base_context(cfg, Some(user));
    ctx.insert("flash_messages", &flash::take(session));
    ctx.insert("avatar_url", &user.avatar_url(cfg));
    ctx.insert("hide_comments", &hide_comments);
    ctx.insert("active_tab", active_tab);
    ctx.insert("password_errors", if active_tab == "password" { &errors } else { &no_errors });
    ctx.insert("email_errors", if active_tab == "email" { &errors } else { &no_errors });
    ctx.insert("email_value", email_value);
    let mfa = UserMfa::get(conn, user.id).map_err(internal_error)?;
    if let Some(mfa) = &mfa {
        ctx.insert("two_factor_since", &mfa.enabled_time);
        ctx.insert("recovery_codes_left", &UserMfa::recovery_codes_left(conn, user.id).map_err(internal_error)?);
    }
    ctx.insert("two_factor", &mfa.is_some());
    let html = tmpl.render("profile.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn profile(
    CurrentUser(current_user): CurrentUser,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let Some(current_user) = current_user else {
        return Ok(redirect("/login"));
    };
    let mut conn = pool.get().map_err(internal_error)?;
    render_profile(&session, &mut conn, &tmpl, &cfg, &current_user, "password", FieldErrors::new(), "")
}

/// Upstream `profile()` POST: email and password changes need the current password;
/// preferences don't. Every outcome but a validation error redirects back with a flash.
pub async fn profile_post(
    CurrentUser(user): CurrentUser,
    req: HttpRequest,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    form: web::Form<ProfileForm>,
) -> Result<HttpResponse> {
    let Some(user) = user else {
        return Ok(redirect("/login"));
    };
    let mut conn = pool.get().map_err(internal_error)?;
    let internal = internal_error;

    if form.authorized_submit.is_some() {
        let errors = form.validate(&mut conn).map_err(internal)?;
        if !errors.is_empty() {
            let tab = if form.tab.as_deref() == Some("email") { "email" } else { "password" };
            return render_profile(&session, &mut conn, &tmpl, &cfg, &user, tab, errors, form.email.trim());
        }
        let email = form.email.trim();
        if !email.is_empty() {
            if !user.verify_password(&form.current_password) {
                flash::push(&session, "danger", "Email change failed!", "Incorrect password.");
                return Ok(redirect(PROFILE_URL));
            }
            User::set_email(&mut conn, user.id, email).map_err(internal)?;
            flash::push(&session, "success", "Email successfully changed!", "");
        }
        if !form.new_password.is_empty() {
            if !user.verify_password(&form.current_password) {
                flash::push(&session, "danger", "Password change failed!", "Incorrect password.");
                return Ok(redirect(PROFILE_URL));
            }
            User::set_password(&mut conn, user.id, &form.new_password).map_err(internal)?;
            // A new password ends every other session; this one starts over
            let method = session_auth_method(&session, &mut conn);
            logout_everywhere(&mut conn, user.id).map_err(internal)?;
            login_user(&session, &mut conn, user.id, client_ip(&req), method).map_err(internal_error)?;
            flash::push(&session, "success", "Password successfully changed!", "");
        }
    } else if form.submit_settings.is_some() {
        User::set_hide_comments(&mut conn, user.id, form.hide_comments.is_some()).map_err(internal)?;
        flash::push(&session, "success", "Preferences successfully changed!", "");
    }
    Ok(redirect(PROFILE_URL))
}

/// The "Change avatar" form on the Preferences tab (multipart, field `avatar`).
pub async fn avatar_post(
    CurrentUser(user): CurrentUser,
    session: Session,
    pool: web::Data<DbPool>,
    storage: web::Data<Storage>,
    mut payload: Multipart,
) -> Result<HttpResponse> {
    let Some(user) = user else {
        return Ok(redirect("/login"));
    };
    let fail = |text: &str| {
        flash::push(&session, "danger", "Avatar change failed!", text);
        Ok(redirect(PROFILE_URL))
    };

    let mut upload = None;
    while let Some(item) = payload.next().await {
        let mut field = item.map_err(actix_web::error::ErrorBadRequest)?;
        if field.name() != Some("avatar") {
            continue;
        }
        match crate::handlers::torrents::read_field(&mut field, avatar::MAX_AVATAR_UPLOAD).await {
            Ok(data) => upload = Some(data),
            Err(_) => return fail("The file is too large; at most 4 MiB."),
        }
        break;
    }
    let Some(data) = upload.filter(|d| !d.is_empty()) else {
        return fail("No file selected.");
    };

    // Decoding and resizing is CPU work; keep it off the async workers
    let png = match web::block(move || avatar::process(&data)).await? {
        Ok(png) => png,
        Err(msg) => return fail(msg),
    };
    storage.put(Kind::Avatar, user.id, png).await.map_err(internal_error)?;
    let mut conn = pool.get().map_err(internal_error)?;
    User::set_avatar_time(&mut conn, user.id, chrono::Utc::now().naive_utc()).map_err(internal_error)?;
    flash::push(&session, "success", "Avatar successfully changed!", "");
    Ok(redirect(PROFILE_URL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::{
        cookie::{Cookie, Key},
        http::StatusCode,
        test, App,
    };
    use diesel::r2d2::Pool;

    const PASSWORD: &str = "hunter22";

    fn pool() -> DbPool {
        // One connection, so every request sees the same in-memory database
        let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(":memory:")).unwrap();
        let mut conn = pool.get().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        for (name, email) in [("alice", "alice@example.com"), ("bob", "bob@example.com")] {
            diesel::insert_into(users::table)
                .values(&NewUser::new(name, Some(email), PASSWORD))
                .execute(&mut conn)
                .unwrap();
        }
        pool
    }

    fn config(test: &str) -> Config {
        let avatars = std::env::temp_dir().join(format!("nyaa-avatar-test-{}-{}", std::process::id(), test));
        Config {
            database_url: String::new(),
            secret_key: String::new(),
            site_name: "Nyaa".into(),
            site_flavor: "nyaa".into(),
            results_per_page: 75,
            max_pages: 0,
            torrent_storage_path: String::new(),
            avatar_storage_path: avatars.to_string_lossy().into_owned(),
            enable_gravatar: false,
            maintenance: Default::default(),
            site_url: "http://localhost:8080".into(),
            tracker_urls: vec![],
            trusted_proxies: vec![],
            meili: None,
            tracker: None,
            ratelimit_account_age: 0,
            editing_time_limit: 0,
            upload_limit: Default::default(),
            trusted: Default::default(),
            tickets: Default::default(),
            mfa: Default::default(),
        }
    }

    use crate::middleware::auth::test_support::login as login_as;

    /// The profile routes plus a login shortcut; returns the app and alice's session cookie.
    macro_rules! app {
        ($pool:expr, $cfg:expr) => {{
            let mut tera = Tera::new("templates/**/*").unwrap();
            crate::utils::tera_filters::register(&mut tera);
            let app = test::init_service(
                App::new()
                    .app_data(web::Data::new($cfg.clone()))
                    .app_data(web::Data::new($pool.clone()))
                    .app_data(web::Data::new(
                        Storage::local(&$cfg.avatar_storage_path, &$cfg.avatar_storage_path).unwrap(),
                    ))
                    .app_data(web::Data::new(tera))
                    .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                    .route("/login/{id}", web::get().to(login_as))
                    .route("/login", web::post().to(login_post))
                    .route("/register", web::post().to(register_post))
                    .route("/logout", web::post().to(logout))
                    .route("/account/{page:.+}", web::route().to(legacy_redirect))
                    .route("/profile", web::get().to(profile))
                    .route("/profile", web::post().to(profile_post))
                    .route("/profile/avatar", web::post().to(avatar_post))
                    .route("/avatar/{id}", web::get().to(crate::handlers::users::avatar)),
            )
            .await;
            let res = test::call_service(&app, test::TestRequest::get().uri("/login/1").to_request()).await;
            let cookie: Cookie<'static> = res.response().cookies().next().unwrap().into_owned();
            (app, cookie)
        }};
    }

    /// Follows the redirect back to the profile with the updated session cookie; gives its HTML.
    macro_rules! follow {
        ($app:expr, $res:expr, $cookie:expr) => {{
            let res = $res;
            assert_eq!(res.status(), StatusCode::FOUND);
            assert_eq!(res.headers().get("Location").unwrap(), PROFILE_URL);
            if let Some(c) = res.response().cookies().next() {
                *$cookie = c.into_owned();
            }
            let req = test::TestRequest::get().uri(PROFILE_URL).cookie($cookie.clone()).to_request();
            let res = test::call_service($app, req).await;
            if let Some(c) = res.response().cookies().next() {
                *$cookie = c.into_owned();
            }
            String::from_utf8(test::read_body(res).await.to_vec()).unwrap()
        }};
    }

    fn post(cookie: &Cookie<'static>, form: &[(&str, &str)]) -> test::TestRequest {
        test::TestRequest::post().uri(PROFILE_URL).cookie(cookie.clone()).set_form(form)
    }

    fn alice(pool: &DbPool) -> User {
        User::by_id(&mut pool.get().unwrap(), 1).unwrap().unwrap()
    }

    /// Posts the login form; gives the status and page.
    macro_rules! try_login {
        ($app:expr, $username:expr, $password:expr) => {{
            let req = test::TestRequest::post()
                .uri("/login")
                .set_form([("username", $username), ("password", $password)])
                .to_request();
            let res = test::call_service(&$app, req).await;
            let status = res.status();
            (status, String::from_utf8(test::read_body(res).await.to_vec()).unwrap())
        }};
    }

    #[actix_web::test]
    async fn register_rules_count_characters() {
        let ok = |u: &str, e: &str, p: &str| register_errors(u, e, p, p).is_empty();
        assert!(ok("new_user-1", "a@b.co", "secret"));
        assert!(!ok("ab", "a@b.co", "secret"));
        assert!(!ok(&"a".repeat(33), "a@b.co", "secret"));
        // Upstream allows ASCII only; non-ASCII letters were allowed but then counted in bytes
        assert!(!ok("ゆきゆき", "a@b.co", "secret"));
        assert!(!ok("new", "", "secret") && !ok("new", "not-an-email", "secret"));
        // Six characters, even when they take more bytes
        assert!(ok("new", "a@b.co", "éééééé") && !ok("new", "a@b.co", "ééééé"));
        assert!(!register_errors("new", "a@b.co", "secret", "secreT").is_empty());
    }

    #[actix_web::test]
    async fn register_rejects_lookalike_names_and_bad_email() {
        let (pool, cfg) = (pool(), config("register"));
        let (app, _) = app!(pool, cfg);
        let register = |username: &'static str, email: &'static str| {
            let app = &app;
            async move {
                let req = test::TestRequest::post()
                    .uri("/register")
                    .set_form([
                        ("username", username),
                        ("email", email),
                        ("password", PASSWORD),
                        ("password_confirm", PASSWORD),
                    ])
                    .to_request();
                let res = test::call_service(app, req).await;
                let status = res.status();
                (status, String::from_utf8(test::read_body(res).await.to_vec()).unwrap())
            }
        };
        let (status, page) = register("Alice", "x@example.com").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(page.contains("Username is already taken."), "{page}");
        let (status, page) = register("carl", "carl").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(page.contains("Please enter a valid email address."), "{page}");
        let (status, _) = register(" carl ", "carl@example.com").await;
        assert_eq!(status, StatusCode::FOUND);
        assert!(User::by_username(&mut pool.get().unwrap(), "carl").unwrap().is_some(), "stored trimmed");
    }

    #[actix_web::test]
    async fn ban_reason_needs_the_right_password() {
        let (pool, cfg) = (pool(), config("ban-reason"));
        let (app, _) = app!(pool, cfg);
        diesel::update(users::table.find(2)).set(users::status.eq(2)).execute(&mut pool.get().unwrap()).unwrap();

        let (status, body) = try_login!(app, "bob", "wrong");
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("Invalid username or password.") && !body.contains("banned"));

        let (status, body) = try_login!(app, "bob", PASSWORD);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("banned"));
    }

    #[actix_web::test]
    async fn repeated_failed_logins_lock_the_account_for_a_while() {
        let (pool, cfg) = (pool(), config("throttle"));
        let (app, _) = app!(pool, cfg);
        // A user no other test logs in as: the counters are process-wide. Username and
        // email count against the same account.
        diesel::insert_into(users::table)
            .values(&NewUser::new("carol", Some("carol@example.com"), PASSWORD))
            .execute(&mut pool.get().unwrap())
            .unwrap();
        for i in 0..10 {
            let name = if i % 2 == 0 { "carol" } else { "carol@example.com" };
            assert_eq!(try_login!(app, name, "wrong").0, StatusCode::UNAUTHORIZED);
        }
        // Locked, even with the right password
        let (status, body) = try_login!(app, "carol", PASSWORD);
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(body.contains("Too many failed login attempts"));
    }

    #[actix_web::test]
    async fn profile_shows_tabs_without_email_line() {
        let (pool, cfg) = (pool(), config("tabs"));
        let (app, cookie) = app!(pool, cfg);
        let req = test::TestRequest::get().uri(PROFILE_URL).cookie(cookie).to_request();
        let html = String::from_utf8(test::read_body(test::call_service(&app, req).await).await.to_vec()).unwrap();
        assert!(!html.contains("<dt class=\"col-sm-2\">Email:</dt>"), "{}", html);
        for tab in [
            "Password</a>",
            "Email</a>",
            "Preferences</a>",
            "Repeat New Password",
            "New Email Address",
            "Images will be scaled and cropped to 256x256.",
            "Change avatar",
            "Hide comments by default",
        ] {
            assert!(html.contains(tab), "missing {}", tab);
        }
        // Only the email tab shows the address
        assert_eq!(html.matches("alice@example.com").count(), 1);
        // Tera escapes slashes in attributes, which browsers undo
        assert!(html.contains("src=\"&#x2F;static&#x2F;img&#x2F;avatar&#x2F;default.png\""), "{}", html);
    }

    #[actix_web::test]
    async fn logged_out_profile_redirects_to_login() {
        let (pool, cfg) = (pool(), config("logged-out"));
        let (app, _) = app!(pool, cfg);
        let res = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(PROFILE_URL)
                .set_form([("submit_settings", "Update"), ("hide_comments", "y")])
                .to_request(),
        )
        .await;
        assert_eq!(res.headers().get("Location").unwrap(), "/login");
        assert!(!User::hide_comments(&mut pool.get().unwrap(), 1).unwrap());
    }

    #[actix_web::test]
    async fn password_change_needs_current_password() {
        let (pool, cfg) = (pool(), config("password"));
        let (app, mut cookie) = app!(pool, cfg);

        let res = test::call_service(
            &app,
            post(
                &cookie,
                &[
                    ("tab", "password"),
                    ("current_password", "wrong"),
                    ("new_password", "newpass1"),
                    ("password_confirm", "newpass1"),
                    ("authorized_submit", "Update"),
                ],
            )
            .to_request(),
        )
        .await;
        let html = follow!(&app, res, &mut cookie);
        assert!(html.contains("<strong>Password change failed!</strong> Incorrect password."), "{}", html);
        assert!(alice(&pool).verify_password(PASSWORD));

        let res = test::call_service(
            &app,
            post(
                &cookie,
                &[
                    ("tab", "password"),
                    ("current_password", PASSWORD),
                    ("new_password", "newpass1"),
                    ("password_confirm", "newpass1"),
                    ("authorized_submit", "Update"),
                ],
            )
            .to_request(),
        )
        .await;
        let html = follow!(&app, res, &mut cookie);
        assert!(html.contains("<strong>Password successfully changed!</strong>"), "{}", html);
        assert!(alice(&pool).verify_password("newpass1"));
        assert!(!alice(&pool).verify_password(PASSWORD));
        assert!(alice(&pool).password_hash.starts_with("$argon2"));
    }

    /// Whether `cookie` still gets alice's profile, rather than a redirect to the login page.
    macro_rules! signed_in {
        ($app:expr, $cookie:expr) => {{
            let req = test::TestRequest::get().uri(PROFILE_URL).cookie($cookie.clone()).to_request();
            test::call_service(&$app, req).await.status() == StatusCode::OK
        }};
    }

    #[actix_web::test]
    async fn a_copied_cookie_stops_working_after_logout() {
        let (pool, cfg) = (pool(), config("logout"));
        let (app, cookie) = app!(pool, cfg);
        assert!(signed_in!(app, cookie));
        let res =
            test::call_service(&app, test::TestRequest::post().uri("/logout").cookie(cookie.clone()).to_request())
                .await;
        assert_eq!(res.status(), StatusCode::FOUND);
        // The old cookie is still validly signed, but its session row is gone
        assert!(!signed_in!(app, cookie));
    }

    #[actix_web::test]
    async fn a_new_password_ends_other_sessions() {
        let (pool, cfg) = (pool(), config("password-sessions"));
        let (app, mut cookie) = app!(pool, cfg);
        let res = test::call_service(&app, test::TestRequest::get().uri("/login/1").to_request()).await;
        let other: Cookie<'static> = res.response().cookies().next().unwrap().into_owned();
        assert!(signed_in!(app, other));

        let form = [
            ("tab", "password"),
            ("current_password", PASSWORD),
            ("new_password", "newpass1"),
            ("password_confirm", "newpass1"),
            ("authorized_submit", "Update"),
        ];
        let html = follow!(&app, test::call_service(&app, post(&cookie, &form).to_request()).await, &mut cookie);
        assert!(html.contains("Password successfully changed!"), "{}", html);
        // The browser that changed it stays signed in with its new cookie; the other doesn't
        assert!(signed_in!(app, cookie));
        assert!(!signed_in!(app, other));
    }

    #[actix_web::test]
    async fn login_records_ip_and_old_account_urls_redirect() {
        let (pool, cfg) = (pool(), config("login-ip"));
        let (app, _) = app!(pool, cfg);
        let req = test::TestRequest::post()
            .uri("/login")
            .peer_addr("10.1.2.3:4000".parse().unwrap())
            .set_form([("username", "bob"), ("password", PASSWORD)])
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status(), StatusCode::FOUND);
        let bob = User::by_id(&mut pool.get().unwrap(), 2).unwrap().unwrap();
        assert_eq!(bob.last_login_ip, Some(crate::utils::pack_ip("10.1.2.3".parse().unwrap())));
        assert!(bob.last_login_date.is_some());

        for (old, new) in [("/account/login?next=x", "/login?next=x"), ("/account/profile/avatar", "/profile/avatar")] {
            let res = test::call_service(&app, test::TestRequest::post().uri(old).to_request()).await;
            assert_eq!(res.status(), StatusCode::PERMANENT_REDIRECT);
            assert_eq!(res.headers().get("Location").unwrap(), new);
        }
        let res = test::call_service(&app, test::TestRequest::get().uri("/account/nope").to_request()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn validation_errors_show_on_the_submitting_tab() {
        let (pool, cfg) = (pool(), config("validation"));
        let (app, cookie) = app!(pool, cfg);

        let res = test::call_service(
            &app,
            post(
                &cookie,
                &[
                    ("tab", "password"),
                    ("current_password", ""),
                    ("new_password", "abc"),
                    ("password_confirm", "abd"),
                    ("authorized_submit", "Update"),
                ],
            )
            .to_request(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let html = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
        assert!(html.contains("This field is required."));
        assert!(
            html.contains("<li>Two passwords must match</li><li>Password must be at least 6 characters long.</li>"),
            "{}",
            html
        );
        assert!(html.contains("<li role=\"presentation\" class=\"active\">\n\t\t<a href=\"#password-change\""));

        let res = test::call_service(
            &app,
            post(
                &cookie,
                &[
                    ("tab", "email"),
                    ("current_password", PASSWORD),
                    ("email", "bob@example.com"),
                    ("authorized_submit", "Update"),
                ],
            )
            .to_request(),
        )
        .await;
        let html = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
        assert!(html.contains("This email address has been taken"), "{}", html);
        assert!(html.contains("<a href=\"#email-change\" id=\"email-change-tab\" role=\"tab\" data-toggle=\"tab\" aria-controls=\"profile\" aria-expanded=\"true\">"));
        assert!(html.contains("value=\"bob@example.com\""));

        let res = test::call_service(
            &app,
            post(
                &cookie,
                &[
                    ("tab", "email"),
                    ("current_password", PASSWORD),
                    ("email", "not-an-email"),
                    ("authorized_submit", "Update"),
                ],
            )
            .to_request(),
        )
        .await;
        let html = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
        assert!(html.contains("Invalid email address."));
        assert_eq!(alice(&pool).email.as_deref(), Some("alice@example.com"));
    }

    #[actix_web::test]
    async fn email_change_needs_current_password() {
        let (pool, cfg) = (pool(), config("email"));
        let (app, mut cookie) = app!(pool, cfg);

        let res = test::call_service(
            &app,
            post(
                &cookie,
                &[
                    ("tab", "email"),
                    ("current_password", "wrong"),
                    ("email", "new@example.com"),
                    ("authorized_submit", "Update"),
                ],
            )
            .to_request(),
        )
        .await;
        let html = follow!(&app, res, &mut cookie);
        assert!(html.contains("<strong>Email change failed!</strong> Incorrect password."));
        assert_eq!(alice(&pool).email.as_deref(), Some("alice@example.com"));

        let res = test::call_service(
            &app,
            post(
                &cookie,
                &[
                    ("tab", "email"),
                    ("current_password", PASSWORD),
                    ("email", " new@example.com "),
                    ("authorized_submit", "Update"),
                ],
            )
            .to_request(),
        )
        .await;
        let html = follow!(&app, res, &mut cookie);
        assert!(html.contains("<strong>Email successfully changed!</strong>"));
        assert!(html.contains("<div id=\"current_email\">new@example.com</div>"));
        assert_eq!(alice(&pool).email.as_deref(), Some("new@example.com"));
        // Flashes show once
        let req = test::TestRequest::get().uri(PROFILE_URL).cookie(cookie.clone()).to_request();
        let html = String::from_utf8(test::read_body(test::call_service(&app, req).await).await.to_vec()).unwrap();
        assert!(!html.contains("successfully changed"));
    }

    #[actix_web::test]
    async fn hide_comments_preference_round_trips() {
        let (pool, cfg) = (pool(), config("prefs"));
        let (app, mut cookie) = app!(pool, cfg);

        let res = test::call_service(
            &app,
            post(&cookie, &[("tab", "preferences"), ("hide_comments", "y"), ("submit_settings", "Update")])
                .to_request(),
        )
        .await;
        let html = follow!(&app, res, &mut cookie);
        assert!(html.contains("<strong>Preferences successfully changed!</strong>"));
        assert!(html.contains("value=\"y\" checked>"));
        assert!(User::hide_comments(&mut pool.get().unwrap(), 1).unwrap());

        let res = test::call_service(
            &app,
            post(&cookie, &[("tab", "preferences"), ("submit_settings", "Update")]).to_request(),
        )
        .await;
        let html = follow!(&app, res, &mut cookie);
        assert!(!html.contains("value=\"y\" checked>"));
        assert!(!User::hide_comments(&mut pool.get().unwrap(), 1).unwrap());
    }

    fn multipart(cookie: &Cookie<'static>, data: &[u8]) -> test::TestRequest {
        let boundary = "XBOUNDARYX";
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"a.png\"\r\n\
                                Content-Type: image/png\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        test::TestRequest::post()
            .uri("/profile/avatar")
            .cookie(cookie.clone())
            .insert_header(("Content-Type", format!("multipart/form-data; boundary={boundary}")))
            .set_payload(body)
    }

    #[actix_web::test]
    async fn avatar_upload_is_scaled_stored_and_served() {
        let (pool, cfg) = (pool(), config("avatar"));
        let (app, mut cookie) = app!(pool, cfg);

        let res = test::call_service(&app, multipart(&cookie, b"definitely not a png").to_request()).await;
        let html = follow!(&app, res, &mut cookie);
        assert!(html.contains("<strong>Avatar change failed!</strong> Unsupported image."), "{}", html);
        assert!(alice(&pool).avatar_time.is_none());

        let res = test::call_service(&app, multipart(&cookie, &avatar::tests::sample_png(600, 300)).to_request()).await;
        let html = follow!(&app, res, &mut cookie);
        assert!(html.contains("<strong>Avatar successfully changed!</strong>"));
        let url = alice(&pool).avatar_url(&cfg);
        assert!(url.starts_with("/avatar/1?v="), "{}", url);
        assert!(html.contains(&format!("src=\"{}\"", url.replace('/', "&#x2F;"))));

        let res = test::call_service(&app, test::TestRequest::get().uri(&url).to_request()).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get("Content-Type").unwrap(), "image/png");
        let img = image::load_from_memory(&test::read_body(res).await).unwrap();
        assert_eq!((img.width(), img.height()), (256, 256));

        let res = test::call_service(&app, test::TestRequest::get().uri("/avatar/2").to_request()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        std::fs::remove_dir_all(&cfg.avatar_storage_path).ok();
    }

    #[::core::prelude::v1::test]
    fn gravatar_only_when_enabled_and_no_upload() {
        let mut cfg = config("gravatar");
        let mut user = User { avatar_time: None, ..User::by_id(&mut pool().get().unwrap(), 1).unwrap().unwrap() };
        assert_eq!(user.avatar_url(&cfg), "/static/img/avatar/default.png");
        cfg.enable_gravatar = true;
        // md5("alice@example.com")
        assert_eq!(
            user.avatar_url(&cfg),
            "https://www.gravatar.com/avatar/c160f8cc69a4f0bf2b0362752353d060\
            ?s=120&d=http%3A%2F%2Flocalhost%3A8080%2Fstatic%2Fimg%2Favatar%2Fdefault.png&r=pg"
        );
        user.avatar_time = Some(chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap().naive_utc());
        assert_eq!(user.avatar_url(&cfg), "/avatar/1?v=1700000000");
    }

    #[::core::prelude::v1::test]
    fn email_shape_check() {
        for ok in ["a@b.co", "first.last+tag@sub.example.org"] {
            assert!(looks_like_email(ok), "{}", ok);
        }
        for bad in ["", "a@b", "@b.co", "a@@b.co", "a b@c.de", "a@b..c", "a@.b"] {
            assert!(!looks_like_email(bad), "{}", bad);
        }
    }
}
