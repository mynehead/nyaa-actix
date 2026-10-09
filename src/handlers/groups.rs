use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use tera::Tera;

use crate::auth::{CurrentUser, LoggedIn, Permission};
use crate::config::Config;
use crate::db::schema::group_members;
use crate::db::schema::groups;
use crate::db::{DbConnection, DbPool};
use crate::models::{Group, NewGroup, User};
use crate::search::db::{with_stats, SearchQuery};
use crate::search::search;
use crate::utils::context::base_context;
use crate::utils::context::SearchState;
use crate::utils::pagination::Pagination;
use crate::utils::{internal_error, sanitize_string, sanitize_text};

pub async fn group_list(
    CurrentUser(current_user): CurrentUser,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let all_groups = Group::all(&mut conn).map_err(internal_error)?;

    let groups: Vec<serde_json::Value> = all_groups
        .into_iter()
        .map(|g| {
            let owner = User::by_id(&mut conn, g.owner_id).ok().flatten().map(|u| u.username);
            serde_json::json!({ "group": g, "owner": owner })
        })
        .collect();

    let mut ctx = base_context(&cfg, current_user.as_ref());
    ctx.insert("active_page", "groups");
    ctx.insert("groups", &groups);
    let html = tmpl.render("groups.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

/// The create and edit forms' fields.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct GroupForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub description: String,
}

const NAME_LEN: std::ops::RangeInclusive<usize> = 3..=64;
const TAG_LEN: std::ops::RangeInclusive<usize> = 1..=32;
const SLUG_LEN: std::ops::RangeInclusive<usize> = 1..=64;
const MAX_DESCRIPTION_LEN: usize = 2048;

impl GroupForm {
    fn from_group(g: &Group) -> Self {
        GroupForm {
            name: g.name.clone(),
            tag: g.tag.clone(),
            slug: g.slug.clone(),
            description: g.description.clone().unwrap_or_default(),
        }
    }

    /// Trimmed and stripped of control characters, as stored.
    fn cleaned(&self) -> GroupForm {
        GroupForm {
            name: sanitize_string(self.name.trim()),
            tag: sanitize_string(self.tag.trim()),
            slug: self.slug.trim().to_string(),
            description: sanitize_text(self.description.trim()),
        }
    }

    /// Errors for a cleaned form. `editing` is the group being edited, which may keep its
    /// own name, tag and slug; all three are UNIQUE, so a clash is an error here rather
    /// than a failed insert.
    fn validate(&self, conn: &mut DbConnection, editing: Option<&Group>) -> QueryResult<Vec<String>> {
        let mut errors = Vec::new();
        let len = |s: &str| s.chars().count();
        if !NAME_LEN.contains(&len(&self.name)) {
            errors.push(format!("Name must be {} to {} characters.", NAME_LEN.start(), NAME_LEN.end()));
        }
        if !TAG_LEN.contains(&len(&self.tag)) {
            errors.push(format!("Tag must be {} to {} characters.", TAG_LEN.start(), TAG_LEN.end()));
        } else if self.tag.contains(['[', ']']) {
            errors.push("Tag can't contain [ or ]; it is shown in brackets.".into());
        }
        // The slug is a URL path segment
        if !SLUG_LEN.contains(&len(&self.slug))
            || !self.slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            errors.push(format!(
                "Slug must be {} to {} lowercase letters, digits, - or _.",
                SLUG_LEN.start(),
                SLUG_LEN.end()
            ));
        }
        if len(&self.description) > MAX_DESCRIPTION_LEN {
            errors.push(format!("Description must be at most {MAX_DESCRIPTION_LEN} characters."));
        }
        let others = groups::table.filter(groups::id.ne(editing.map_or(0, |g| g.id)));
        let taken = |n: i64| n > 0;
        if taken(others.filter(groups::name.eq(&self.name)).count().get_result(conn)?) {
            errors.push("That name is already taken.".into());
        }
        if taken(others.filter(groups::tag.eq(&self.tag)).count().get_result(conn)?) {
            errors.push("That tag is already taken.".into());
        }
        if taken(others.filter(groups::slug.eq(&self.slug)).count().get_result(conn)?) {
            errors.push("Slug already taken.".into());
        }
        Ok(errors)
    }

    fn description(&self) -> Option<String> {
        Some(self.description.clone()).filter(|d| !d.is_empty())
    }
}

fn render_group_form(
    tmpl: &Tera,
    cfg: &Config,
    user: &User,
    template: &str,
    group: Option<&Group>,
    form: &GroupForm,
    errors: &[String],
) -> Result<String> {
    let mut ctx = base_context(cfg, Some(user));
    ctx.insert("group", &group);
    ctx.insert("form", form);
    ctx.insert("errors", errors);
    tmpl.render(template, &ctx).map_err(internal_error)
}

/// Only moderators and admins may create groups.
fn group_creator(user: User) -> Result<User> {
    if !user.can(Permission::CreateGroups) {
        return Err(actix_web::error::ErrorForbidden("Only moderators and admins can create groups"));
    }
    Ok(user)
}

pub async fn create_group_get(
    LoggedIn(user): LoggedIn,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let current_user = group_creator(user)?;
    let html = render_group_form(&tmpl, &cfg, &current_user, "group_create.html", None, &GroupForm::default(), &[])?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn create_group_post(
    LoggedIn(user): LoggedIn,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    form: web::Form<GroupForm>,
) -> Result<HttpResponse> {
    let current_user = group_creator(user)?;
    let mut conn = pool.get().map_err(internal_error)?;

    let form = form.cleaned();
    let errors = form.validate(&mut conn, None).map_err(internal_error)?;
    if !errors.is_empty() {
        let html = render_group_form(&tmpl, &cfg, &current_user, "group_create.html", None, &form, &errors)?;
        return Ok(HttpResponse::BadRequest().content_type("text/html").body(html));
    }

    let new_group = NewGroup {
        name: form.name.clone(),
        tag: form.tag.clone(),
        slug: form.slug.clone(),
        description: form.description(),
        created_time: chrono::Utc::now().naive_utc(),
        owner_id: current_user.id,
    };
    diesel::insert_into(groups::table).values(&new_group).execute(&mut conn).map_err(internal_error)?;

    Ok(HttpResponse::Found().insert_header(("Location", format!("/group/{}", form.slug))).finish())
}

#[derive(Debug, Deserialize)]
pub struct GroupSearchParams {
    pub q: Option<String>,
    pub s: Option<String>,
    pub o: Option<String>,
    pub c: Option<String>,
    pub f: Option<String>,
    pub p: Option<i64>,
}

pub async fn view_group(
    CurrentUser(current_user): CurrentUser,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
    params: web::Query<GroupSearchParams>,
) -> Result<HttpResponse> {
    let slug = path.into_inner();
    let moderator = current_user.as_ref().is_some_and(|u| u.can(Permission::ModerateTorrents));
    let mut conn = pool.get().map_err(internal_error)?;

    let group = Group::by_slug(&mut conn, &slug)
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Group not found"))?;

    let q = SearchQuery::from_params(
        params.q.clone(),
        None,
        Some(group.id),
        params.c.as_deref(),
        params.f.as_deref(),
        params.s.as_deref(),
        params.o.as_deref(),
        params.p,
        cfg.results_per_page,
        moderator,
    );

    crate::utils::pagination::check_max_pages(q.page, cfg.max_pages)?;
    let result = search(&mut conn, cfg.meili.as_ref(), &q).map_err(internal_error)?;
    let pagination = Pagination::capped(q.page, result.total, q.per_page, cfg.max_pages);

    let owner = User::by_id(&mut conn, group.owner_id).map_err(internal_error)?;

    let can_edit = current_user.as_ref().map(|u| group.can_edit(&mut conn, u.id)).unwrap_or(false);

    let mut ctx = base_context(&cfg, current_user.as_ref());
    ctx.insert("group", &group);
    ctx.insert("owner", &owner);
    ctx.insert("can_edit", &can_edit);
    ctx.insert("can_report", &crate::handlers::reports::can_report(current_user.as_ref(), &cfg));
    ctx.insert("flash_messages", &crate::utils::flash::take(&session));
    let torrents = with_stats(&mut conn, result.torrents).map_err(internal_error)?;
    ctx.insert("torrents", &torrents);
    ctx.insert("pagination", &pagination);
    ctx.insert("search", &SearchState::new(&params.q, &params.c, &params.f, &params.s, &params.o));
    let html = tmpl.render("group.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn edit_group_get(
    LoggedIn(current_user): LoggedIn,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let group = Group::by_slug(&mut conn, &path.into_inner())
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Group not found"))?;
    if !group.can_edit(&mut conn, current_user.id) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let form = GroupForm::from_group(&group);
    let html = render_group_form(&tmpl, &cfg, &current_user, "group_edit.html", Some(&group), &form, &[])?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn edit_group_post(
    LoggedIn(current_user): LoggedIn,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
    form: web::Form<GroupForm>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let group = Group::by_slug(&mut conn, &path.into_inner())
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Group not found"))?;
    if !group.can_edit(&mut conn, current_user.id) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let form = form.cleaned();
    let errors = form.validate(&mut conn, Some(&group)).map_err(internal_error)?;
    if !errors.is_empty() {
        let html = render_group_form(&tmpl, &cfg, &current_user, "group_edit.html", Some(&group), &form, &errors)?;
        return Ok(HttpResponse::BadRequest().content_type("text/html").body(html));
    }
    diesel::update(groups::table.find(group.id))
        .set((
            groups::name.eq(&form.name),
            groups::tag.eq(&form.tag),
            groups::slug.eq(&form.slug),
            groups::description.eq(form.description()),
        ))
        .execute(&mut conn)
        .map_err(internal_error)?;
    Ok(HttpResponse::Found().insert_header(("Location", format!("/group/{}", form.slug))).finish())
}

#[derive(Debug, Deserialize)]
pub struct MemberForm {
    pub username: String,
    pub can_upload: Option<String>,
    pub can_edit: Option<String>,
}

pub async fn manage_members_get(
    LoggedIn(current_user): LoggedIn,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let group = Group::by_slug(&mut conn, &path.into_inner())
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Group not found"))?;
    if !group.can_edit(&mut conn, current_user.id) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let members_raw = group.members_with_perms(&mut conn).map_err(internal_error)?;
    let members: Vec<serde_json::Value> = members_raw
        .into_iter()
        .filter_map(|(uid, perms)| {
            User::by_id(&mut conn, uid).ok()?.map(|u| {
                serde_json::json!({
                    "username": u.username,
                    "can_upload": perms & crate::models::PERM_UPLOAD != 0,
                    "can_edit": perms & crate::models::PERM_EDIT != 0,
                })
            })
        })
        .collect();

    let mut ctx = base_context(&cfg, Some(&current_user));
    ctx.insert("group", &group);
    ctx.insert("members", &members);
    ctx.insert("is_owner", &(current_user.id == group.owner_id));
    let html = tmpl.render("group_members.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn manage_members_post(
    LoggedIn(current_user): LoggedIn,
    pool: web::Data<DbPool>,
    path: web::Path<String>,
    form: web::Form<MemberForm>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let group = Group::by_slug(&mut conn, &path.into_inner())
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Group not found"))?;
    if !group.can_edit(&mut conn, current_user.id) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }

    let target = User::by_username(&mut conn, &form.username)
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorBadRequest("User not found"))?;

    let mut perms = 0i32;
    if form.can_upload.is_some() {
        perms |= crate::models::PERM_UPLOAD;
    }
    if form.can_edit.is_some() {
        perms |= crate::models::PERM_EDIT;
    }

    let existing: Option<i32> = group_members::table
        .filter(group_members::group_id.eq(group.id))
        .filter(group_members::user_id.eq(target.id))
        .select(group_members::permissions)
        .first(&mut conn)
        .optional()
        .map_err(internal_error)?;

    // Editors manage uploaders; only the owner grants or takes away edit rights, so one
    // editor can't add accomplices or push the other editors out
    let touches_editor =
        perms & crate::models::PERM_EDIT != 0 || existing.is_some_and(|p| p & crate::models::PERM_EDIT != 0);
    if touches_editor && current_user.id != group.owner_id {
        return Err(actix_web::error::ErrorForbidden("Only the group owner can change editors"));
    }

    if perms == 0 {
        if existing.is_some() {
            diesel::delete(
                group_members::table
                    .filter(group_members::group_id.eq(group.id))
                    .filter(group_members::user_id.eq(target.id)),
            )
            .execute(&mut conn)
            .map_err(internal_error)?;
        }
    } else if existing.is_some() {
        diesel::update(
            group_members::table
                .filter(group_members::group_id.eq(group.id))
                .filter(group_members::user_id.eq(target.id)),
        )
        .set(group_members::permissions.eq(perms))
        .execute(&mut conn)
        .map_err(internal_error)?;
    } else {
        diesel::insert_into(group_members::table)
            .values((
                group_members::group_id.eq(group.id),
                group_members::user_id.eq(target.id),
                group_members::permissions.eq(perms),
            ))
            .execute(&mut conn)
            .map_err(internal_error)?;
    }

    Ok(HttpResponse::Found().insert_header(("Location", format!("/group/{}/members", group.slug))).finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{PERM_EDIT, PERM_UPLOAD};
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::{
        cookie::{Cookie, Key},
        http::StatusCode,
        test, App,
    };
    use diesel::r2d2::Pool;

    /// Group 1 owned by user 1 (a moderator); user 2 is an editor, user 3 uploads, user 4 isn't a member.
    fn pool() -> DbPool {
        let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(":memory:")).unwrap();
        let mut conn = pool.get().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        for sql in [
            "INSERT INTO users (id, username, password_hash, status, level) VALUES \
             (1, 'owner', 'x', 1, 2), (2, 'editor', 'x', 1, 0), (3, 'uploader', 'x', 1, 0), (4, 'other', 'x', 1, 0)",
            "INSERT INTO groups (id, name, tag, slug, owner_id) VALUES (1, 'Group', 'G', 'grp', 1)",
            "INSERT INTO group_members (group_id, user_id, permissions) VALUES (1, 2, 3), (1, 3, 1)",
        ] {
            diesel::sql_query(sql).execute(&mut conn).unwrap();
        }
        pool
    }

    use crate::middleware::auth::test_support::login;

    fn perms(pool: &DbPool, user: i32) -> Option<i32> {
        group_members::table
            .filter(group_members::group_id.eq(1))
            .filter(group_members::user_id.eq(user))
            .select(group_members::permissions)
            .first(&mut pool.get().unwrap())
            .optional()
            .unwrap()
    }

    #[actix_web::test]
    async fn only_the_owner_changes_editors() {
        let pool = pool();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                .route("/login/{id}", web::get().to(login))
                .route("/group/{slug}/members", web::post().to(manage_members_post)),
        )
        .await;
        let cookie = |user: i32| {
            let app = &app;
            async move {
                let req = test::TestRequest::get().uri(&format!("/login/{user}")).to_request();
                let res = test::call_service(app, req).await;
                res.response().cookies().next().unwrap().into_owned()
            }
        };
        let post = |cookie: Cookie<'static>, form: &'static [(&'static str, &'static str)]| {
            let app = &app;
            async move {
                let req =
                    test::TestRequest::post().uri("/group/grp/members").cookie(cookie).set_form(form).to_request();
                test::call_service(app, req).await.status()
            }
        };
        let editor = cookie(2).await;

        // An editor can still manage uploaders
        assert_eq!(post(editor.clone(), &[("username", "other"), ("can_upload", "1")]).await, StatusCode::FOUND);
        assert_eq!(perms(&pool, 4), Some(PERM_UPLOAD));
        // ...but can't make editors, demote one, or remove one
        assert_eq!(
            post(editor.clone(), &[("username", "uploader"), ("can_upload", "1"), ("can_edit", "1")]).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(post(editor.clone(), &[("username", "editor")]).await, StatusCode::FORBIDDEN);
        assert_eq!(perms(&pool, 3), Some(PERM_UPLOAD));
        assert_eq!(perms(&pool, 2), Some(PERM_UPLOAD | PERM_EDIT));

        // The owner can
        let owner = cookie(1).await;
        assert_eq!(post(owner, &[("username", "editor"), ("can_upload", "1")]).await, StatusCode::FOUND);
        assert_eq!(perms(&pool, 2), Some(PERM_UPLOAD));
    }

    #[actix_web::test]
    async fn only_moderators_and_admins_create_groups() {
        let pool = pool();
        let mut tera = Tera::new("templates/**/*").unwrap();
        crate::utils::tera_filters::register(&mut tera);
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .app_data(web::Data::new(tera))
                .app_data(web::Data::new(Config::for_tests()))
                .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                .route("/login/{id}", web::get().to(login))
                .route("/groups/create", web::get().to(create_group_get))
                .route("/groups/create", web::post().to(create_group_post)),
        )
        .await;
        diesel::sql_query(
            "INSERT INTO users (id, username, password_hash, status, level) VALUES (5, 'admin', 'x', 1, 3)",
        )
        .execute(&mut pool.get().unwrap())
        .unwrap();
        let cookie = |user: i32| {
            let app = &app;
            async move {
                let req = test::TestRequest::get().uri(&format!("/login/{user}")).to_request();
                test::call_service(app, req).await.response().cookies().next().unwrap().into_owned()
            }
        };
        let create = |cookie: Option<Cookie<'static>>, slug: &'static str| {
            let app = &app;
            async move {
                let mut get = test::TestRequest::get().uri("/groups/create");
                let mut post = test::TestRequest::post().uri("/groups/create").set_form([
                    ("name", slug),
                    ("tag", slug),
                    ("slug", slug),
                ]);
                if let Some(c) = cookie {
                    get = get.cookie(c.clone());
                    post = post.cookie(c);
                }
                let get = test::call_service(app, get.to_request()).await.status();
                (get, test::call_service(app, post.to_request()).await.status())
            }
        };
        let exists = |slug: &str| Group::by_slug(&mut pool.get().unwrap(), slug).unwrap().is_some();

        assert_eq!(create(None, "anon").await, (StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED));
        // Regular (user 4) and trusted users are refused
        assert_eq!(create(Some(cookie(4).await), "regular").await, (StatusCode::FORBIDDEN, StatusCode::FORBIDDEN));
        diesel::sql_query("UPDATE users SET level = 1 WHERE id = 4").execute(&mut pool.get().unwrap()).unwrap();
        assert_eq!(create(Some(cookie(4).await), "trusted").await, (StatusCode::FORBIDDEN, StatusCode::FORBIDDEN));
        assert!(!exists("anon") && !exists("regular") && !exists("trusted"));

        // Moderators (user 1) and admins can
        assert_eq!(create(Some(cookie(1).await), "moderator").await, (StatusCode::OK, StatusCode::FOUND));
        assert_eq!(create(Some(cookie(5).await), "admin").await, (StatusCode::OK, StatusCode::FOUND));
        assert!(exists("moderator") && exists("admin"));
    }

    #[actix_web::test]
    async fn create_and_edit_reject_bad_or_taken_names() {
        let pool = pool();
        let mut tera = Tera::new("templates/**/*").unwrap();
        crate::utils::tera_filters::register(&mut tera);
        let cfg = Config::for_tests();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .app_data(web::Data::new(tera))
                .app_data(web::Data::new(cfg))
                .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                .route("/login/{id}", web::get().to(login))
                .route("/groups/create", web::post().to(create_group_post))
                .route("/group/{slug}/edit", web::post().to(edit_group_post)),
        )
        .await;
        let req = test::TestRequest::get().uri("/login/1").to_request();
        let cookie = test::call_service(&app, req).await.response().cookies().next().unwrap().into_owned();
        let post = |uri: &'static str, form: &'static [(&'static str, &'static str)]| {
            let (app, cookie) = (&app, cookie.clone());
            async move {
                let req = test::TestRequest::post().uri(uri).cookie(cookie).set_form(form).to_request();
                let res = test::call_service(app, req).await;
                let status = res.status();
                (status, String::from_utf8(test::read_body(res).await.to_vec()).unwrap())
            }
        };

        let (status, page) =
            post("/groups/create", &[("name", "Group"), ("tag", "G"), ("slug", "Bad Slug"), ("description", "")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        for text in ["That name is already taken.", "That tag is already taken.", "Slug must be"] {
            assert!(page.contains(text), "{text}: {page}");
        }
        assert!(page.contains("value=\"Bad Slug\""), "the form is refilled");

        let (status, _) =
            post("/groups/create", &[("name", " New Group "), ("tag", "NG"), ("slug", "new-group")]).await;
        assert_eq!(status, StatusCode::FOUND);
        let created = Group::by_slug(&mut pool.get().unwrap(), "new-group").unwrap().unwrap();
        assert_eq!((created.name.as_str(), created.owner_id), ("New Group", 1));

        // Editing may keep the group's own values, but not take another group's or blank them
        let (status, page) = post("/group/grp/edit", &[("name", "Group"), ("tag", "NG"), ("slug", "")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(page.contains("That tag is already taken.") && page.contains("Slug must be"), "{page}");
        assert!(!page.contains("That name is already taken."), "{page}");
        let (status, _) = post("/group/grp/edit", &[("name", "Group"), ("tag", "G"), ("slug", "grp-2")]).await;
        assert_eq!(status, StatusCode::FOUND);
        assert!(Group::by_slug(&mut pool.get().unwrap(), "grp-2").unwrap().is_some());
    }
}
