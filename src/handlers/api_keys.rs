//! /profile/api-keys: making and revoking the keys scripts use for the JSON API (see
//! [`crate::auth::api_key`]).

use actix_session::Session;
use actix_web::{web, HttpRequest, HttpResponse, Result};
use serde::Deserialize;
use tera::Tera;

use crate::auth::api_key::{ApiKey, MAX_KEYS, MAX_NAME_LEN};
use crate::auth::LoggedIn;
use crate::config::Config;
use crate::db::DbPool;
use crate::handlers::account::redirect;
use crate::handlers::two_factor::password_ok;
use crate::models::User;
use crate::utils::context::base_context;
use crate::utils::{flash, internal_error, with_conn};

const PAGE_URL: &str = "/profile/api-keys";

#[derive(Debug, Deserialize)]
pub struct CreateForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub current_password: String,
}

/// The page, with `new_key` shown once right after it was made.
async fn render(
    session: &Session,
    pool: &DbPool,
    tmpl: &Tera,
    cfg: &Config,
    user: &User,
    new_key: Option<String>,
) -> Result<HttpResponse> {
    let user_id = user.id;
    let keys = with_conn(pool, move |conn| ApiKey::list(conn, user_id)).await?;
    let mut ctx = base_context(cfg, Some(user));
    ctx.insert("flash_messages", &flash::take(session));
    ctx.insert("keys", &keys);
    ctx.insert("max_keys", &MAX_KEYS);
    ctx.insert("new_key", &new_key);
    let html = tmpl.render("api_keys.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn page(
    LoggedIn(user): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    render(&session, &pool, &tmpl, &cfg, &user, None).await
}

/// Makes a key; needs the current password, so a stolen session cookie can't leave a key behind.
pub async fn create(
    LoggedIn(user): LoggedIn,
    req: HttpRequest,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    form: web::Form<CreateForm>,
) -> Result<HttpResponse> {
    let name = form.name.trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
        flash::push(&session, "danger", "", &format!("Give the key a name of up to {MAX_NAME_LEN} characters."));
        return Ok(redirect(PAGE_URL));
    }
    if !password_ok(&req, &user, &form.current_password).await? {
        flash::push(&session, "danger", "", "Incorrect password.");
        return Ok(redirect(PAGE_URL));
    }
    let user_id = user.id;
    let Some(key) = with_conn(&pool, move |conn| ApiKey::create(conn, user_id, &name)).await? else {
        flash::push(&session, "danger", "", &format!("You already have {MAX_KEYS} keys. Revoke one first."));
        return Ok(redirect(PAGE_URL));
    };
    log::info!("User {} made an API key", user.id);
    render(&session, &pool, &tmpl, &cfg, &user, Some(key)).await
}

pub async fn revoke(
    LoggedIn(user): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let (user_id, id) = (user.id, path.into_inner());
    if with_conn(&pool, move |conn| ApiKey::revoke(conn, user_id, id)).await? {
        log::info!("User {} revoked an API key", user.id);
        flash::push(&session, "success", "", "API key revoked.");
    }
    Ok(redirect(PAGE_URL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::users;
    use crate::middleware::auth::test_support::login as login_as;
    use crate::models::NewUser;
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::cookie::Key;
    use actix_web::{test, App};
    use diesel::prelude::*;
    use diesel::r2d2::Pool;

    #[actix_web::test]
    async fn make_show_once_and_revoke() {
        let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(&crate::db::test_url())).unwrap();
        {
            let mut conn = pool.get().unwrap();
            crate::db::run_migrations(&mut conn).unwrap();
            let user = NewUser::new("keyuser", None, "hunter22");
            diesel::insert_into(users::table).values(&user).execute(&mut conn).unwrap();
        }
        let user_id: i32 = users::table.select(users::id).first(&mut pool.get().unwrap()).unwrap();
        let mut tera = Tera::new("templates/**/*").unwrap();
        crate::utils::tera_filters::register(&mut tera);
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Config::for_tests()))
                .app_data(web::Data::new(pool.clone()))
                .app_data(web::Data::new(tera))
                .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                .route("/test-login/{id}", web::get().to(login_as))
                .route("/profile/api-keys", web::get().to(page))
                .route("/profile/api-keys", web::post().to(create))
                .route("/profile/api-keys/{id}/revoke", web::post().to(revoke)),
        )
        .await;
        let login =
            test::call_service(&app, test::TestRequest::get().uri(&format!("/test-login/{user_id}")).to_request())
                .await;
        let cookie = login.response().cookies().next().unwrap().into_owned();
        let body = |req: test::TestRequest| {
            let req = req.cookie(cookie.clone()).to_request();
            async { String::from_utf8(test::call_and_read_body(&app, req).await.to_vec()).unwrap() }
        };
        let post = |uri: &str, form: &[(&str, &str)]| test::TestRequest::post().uri(uri).set_form(form.to_vec());

        assert!(body(test::TestRequest::get().uri(PAGE_URL)).await.contains("Create key"));
        let wrong = body(post(PAGE_URL, &[("name", "x"), ("current_password", "nope")])).await;
        assert!(!wrong.contains("nyaa_"));

        let made = body(post(PAGE_URL, &[("name", "my <script>"), ("current_password", "hunter22")])).await;
        let start = made.find("value=\"nyaa_").expect("key shown") + 7;
        let key = &made[start..start + 53];
        assert!(made.contains("my &lt;script&gt;"));
        assert!(ApiKey::check(&mut pool.get().unwrap(), user_id, key).unwrap());
        assert!(!body(test::TestRequest::get().uri(PAGE_URL)).await.contains(key), "shown once");

        let id = ApiKey::list(&mut pool.get().unwrap(), user_id).unwrap()[0].id;
        body(post(&format!("/profile/api-keys/{id}/revoke"), &[])).await;
        assert!(!ApiKey::check(&mut pool.get().unwrap(), user_id, key).unwrap());
    }
}
