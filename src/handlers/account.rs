use actix_session::Session;
use actix_web::{web, HttpRequest, HttpResponse, Result};
use diesel::prelude::*;
use serde::Deserialize;
use tera::Tera;

use crate::config::Config;
use crate::db::DbPool;
use crate::db::schema::users;
use crate::middleware::auth::{get_current_user, login_user, logout_user};
use crate::models::{NewUser, User};
use crate::utils::client_ip;

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
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool);
    if current_user.is_some() {
        return Ok(HttpResponse::Found().insert_header(("Location", "/")).finish());
    }
    let mut ctx = tera::Context::new();
    ctx.insert("current_user", &Option::<User>::None);
    ctx.insert("error", &Option::<String>::None);
    ctx.insert("config", &serde_json::json!({ "site_name": cfg.site_name }));
    let html = tmpl.render("login.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
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
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let user = User::by_username_or_email(&mut conn, &form.username)
        .map_err(actix_web::error::ErrorInternalServerError)?;

    let error = match user {
        Some(ref u) if u.verify_password(&form.password) && u.is_active() => {
            login_user(&session, &mut conn, u.id, client_ip(&req, cfg.behind_reverse_proxy))
                .map_err(actix_web::error::ErrorInternalServerError)?;
            return Ok(HttpResponse::Found().insert_header(("Location", "/")).finish());
        }
        Some(ref u) if u.is_banned() => Some("Your account has been banned."),
        Some(_) => Some("Invalid username or password."),
        None => Some("Invalid username or password."),
    };

    let mut ctx = tera::Context::new();
    ctx.insert("current_user", &Option::<User>::None);
    ctx.insert("error", &error);
    ctx.insert("config", &serde_json::json!({ "site_name": cfg.site_name }));
    let html = tmpl.render("login.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().status(actix_web::http::StatusCode::UNAUTHORIZED)
        .content_type("text/html").body(html))
}

pub async fn register_get(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool);
    if current_user.is_some() {
        return Ok(HttpResponse::Found().insert_header(("Location", "/")).finish());
    }
    let mut ctx = tera::Context::new();
    ctx.insert("current_user", &Option::<User>::None);
    ctx.insert("errors", &Vec::<String>::new());
    ctx.insert("config", &serde_json::json!({ "site_name": cfg.site_name }));
    let html = tmpl.render("register.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
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
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let mut errors: Vec<String> = Vec::new();

    if form.username.len() < 3 || form.username.len() > 32 {
        errors.push("Username must be 3–32 characters.".into());
    }
    if !form.username.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
        errors.push("Username may only contain letters, numbers, _ and -.".into());
    }
    if form.password != form.password_confirm {
        errors.push("Passwords do not match.".into());
    }
    if form.password.len() < 6 {
        errors.push("Password must be at least 6 characters.".into());
    }
    if User::by_username(&mut conn, &form.username)
        .map_err(actix_web::error::ErrorInternalServerError)?.is_some() {
        errors.push("Username is already taken.".into());
    }
    if User::by_email(&mut conn, &form.email)
        .map_err(actix_web::error::ErrorInternalServerError)?.is_some() {
        errors.push("Email is already in use.".into());
    }

    if !errors.is_empty() {
        let mut ctx = tera::Context::new();
        ctx.insert("current_user", &Option::<User>::None);
        ctx.insert("errors", &errors);
        ctx.insert("config", &serde_json::json!({ "site_name": cfg.site_name }));
        let html = tmpl.render("register.html", &ctx)
            .map_err(actix_web::error::ErrorInternalServerError)?;
        return Ok(HttpResponse::Ok().status(actix_web::http::StatusCode::BAD_REQUEST)
            .content_type("text/html").body(html));
    }

    let ip = client_ip(&req, cfg.behind_reverse_proxy);
    let new_user = NewUser::new(&form.username, Some(&form.email), &form.password, ip.clone());
    diesel::insert_into(users::table)
        .values(&new_user)
        .execute(&mut conn)
        .map_err(actix_web::error::ErrorInternalServerError)?;

    let user = User::by_username(&mut conn, &form.username)
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorInternalServerError("Failed to fetch user"))?;

    login_user(&session, &mut conn, user.id, ip)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Found().insert_header(("Location", "/")).finish())
}

pub async fn logout(session: Session, pool: web::Data<DbPool>) -> HttpResponse {
    logout_user(&session, &pool);
    HttpResponse::Found().insert_header(("Location", "/")).finish()
}

pub async fn profile(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool)
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    let mut ctx = tera::Context::new();
    ctx.insert("current_user", &current_user);
    ctx.insert("config", &serde_json::json!({ "site_name": cfg.site_name }));
    let html = tmpl.render("profile.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

/// The account pages used to live under `/account/`; they now sit at the root like upstream.
/// 308 keeps the method, so old login/register forms still post to the right place.
pub async fn legacy_redirect(req: HttpRequest, path: web::Path<String>) -> HttpResponse {
    let page = path.into_inner();
    if !matches!(page.as_str(), "login" | "register" | "logout" | "profile") {
        return HttpResponse::NotFound().finish();
    }
    let mut location = format!("/{}", page);
    if !req.query_string().is_empty() {
        location = format!("{}?{}", location, req.query_string());
    }
    HttpResponse::PermanentRedirect().insert_header(("Location", location)).finish()
}
