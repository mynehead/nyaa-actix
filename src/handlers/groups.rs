use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use diesel::prelude::*;
use serde::Deserialize;
use tera::Tera;

use crate::config::Config;
use crate::db::DbPool;
use crate::db::schema::groups;
use crate::utils::context::base_context;
use crate::middleware::auth::get_current_user;
use crate::models::{Group, NewGroup, User};
use crate::db::schema::group_members;
use crate::search::db::{with_stats, SearchQuery};
use crate::search::search;
use crate::utils::context::SearchState;
use crate::utils::pagination::Pagination;

pub async fn group_list(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool);
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let all_groups = Group::all(&mut conn)
        .map_err(actix_web::error::ErrorInternalServerError)?;

    let groups: Vec<serde_json::Value> = all_groups.into_iter().map(|g| {
        let owner = User::by_id(&mut conn, g.owner_id).ok().flatten().map(|u| u.username);
        serde_json::json!({ "group": g, "owner": owner })
    }).collect();

    let mut ctx = base_context(&cfg, current_user.as_ref());
    ctx.insert("active_page", "groups");
    ctx.insert("groups", &groups);
    let html = tmpl.render("groups.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

#[derive(Debug, Deserialize)]
pub struct CreateGroupForm {
    pub name: String,
    pub tag: String,
    pub slug: String,
    pub description: Option<String>,
}

pub async fn create_group_get(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool)
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    let mut ctx = base_context(&cfg, Some(&current_user));
    ctx.insert("errors", &Vec::<String>::new());
    let html = tmpl.render("group_create.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn create_group_post(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    form: web::Form<CreateGroupForm>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool)
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;

    let mut errors: Vec<String> = Vec::new();
    if form.name.len() < 3 { errors.push("Name must be at least 3 characters.".into()); }
    if form.tag.is_empty() { errors.push("Tag is required.".into()); }
    if form.slug.is_empty() { errors.push("Slug is required.".into()); }
    if Group::by_slug(&mut conn, &form.slug).map_err(actix_web::error::ErrorInternalServerError)?.is_some() {
        errors.push("Slug already taken.".into());
    }

    if !errors.is_empty() {
        let mut ctx = base_context(&cfg, Some(&current_user));
        ctx.insert("errors", &errors);
        let html = tmpl.render("group_create.html", &ctx)
            .map_err(actix_web::error::ErrorInternalServerError)?;
        return Ok(HttpResponse::BadRequest().content_type("text/html").body(html));
    }

    let new_group = NewGroup {
        name: form.name.trim().to_string(),
        tag: form.tag.trim().to_string(),
        slug: form.slug.trim().to_string(),
        description: form.description.as_ref().filter(|d| !d.is_empty()).cloned(),
        created_time: chrono::Utc::now().naive_utc(),
        owner_id: current_user.id,
    };
    diesel::insert_into(groups::table)
        .values(&new_group)
        .execute(&mut conn)
        .map_err(actix_web::error::ErrorInternalServerError)?;

    let group = Group::by_slug(&mut conn, &form.slug)
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorInternalServerError("Failed to fetch group"))?;

    Ok(HttpResponse::Found()
        .insert_header(("Location", format!("/group/{}", group.slug)))
        .finish())
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
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
    params: web::Query<GroupSearchParams>,
) -> Result<HttpResponse> {
    let slug = path.into_inner();
    let current_user = get_current_user(&session, &pool);
    let is_admin = current_user.as_ref().map(|u| u.is_moderator()).unwrap_or(false);
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;

    let group = Group::by_slug(&mut conn, &slug)
        .map_err(actix_web::error::ErrorInternalServerError)?
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
        is_admin,
    );

    let result = search(&mut conn, cfg.meili.as_ref(), &q)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    let pagination = Pagination::new(q.page, result.total, q.per_page);

    let owner = User::by_id(&mut conn, group.owner_id)
        .map_err(actix_web::error::ErrorInternalServerError)?;

    let can_edit = current_user.as_ref()
        .map(|u| group.can_edit(&mut conn, u.id))
        .unwrap_or(false);

    let mut ctx = base_context(&cfg, current_user.as_ref());
    ctx.insert("group", &group);
    ctx.insert("owner", &owner);
    ctx.insert("can_edit", &can_edit);
    ctx.insert("can_report", &crate::handlers::reports::can_report(current_user.as_ref(), &cfg));
    ctx.insert("flash_messages", &crate::utils::flash::take(&session));
    let torrents = with_stats(&mut conn, result.torrents)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    ctx.insert("torrents", &torrents);
    ctx.insert("pagination", &pagination);
    ctx.insert("search", &SearchState::new(&params.q, &params.c, &params.f, &params.s, &params.o));
    let html = tmpl.render("group.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

#[derive(Debug, Deserialize)]
pub struct EditGroupForm {
    pub name: String,
    pub tag: String,
    pub slug: String,
    pub description: Option<String>,
}

pub async fn edit_group_get(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool)
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let group = Group::by_slug(&mut conn, &path.into_inner())
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Group not found"))?;
    if !group.can_edit(&mut conn, current_user.id) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let mut ctx = base_context(&cfg, Some(&current_user));
    ctx.insert("group", &group);
    let html = tmpl.render("group_edit.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn edit_group_post(
    session: Session,
    pool: web::Data<DbPool>,
    path: web::Path<String>,
    form: web::Form<EditGroupForm>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool)
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let group = Group::by_slug(&mut conn, &path.into_inner())
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Group not found"))?;
    if !group.can_edit(&mut conn, current_user.id) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    diesel::update(groups::table.find(group.id))
        .set((
            groups::name.eq(form.name.trim()),
            groups::tag.eq(form.tag.trim()),
            groups::slug.eq(form.slug.trim()),
            groups::description.eq(form.description.as_deref().filter(|d| !d.is_empty())),
        ))
        .execute(&mut conn)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Found()
        .insert_header(("Location", format!("/group/{}", form.slug.trim())))
        .finish())
}

#[derive(Debug, Deserialize)]
pub struct MemberForm {
    pub username: String,
    pub can_upload: Option<String>,
    pub can_edit: Option<String>,
}

pub async fn manage_members_get(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool)
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let group = Group::by_slug(&mut conn, &path.into_inner())
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Group not found"))?;
    if !group.can_edit(&mut conn, current_user.id) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let members_raw = group.members_with_perms(&mut conn)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    let members: Vec<serde_json::Value> = members_raw.into_iter().filter_map(|(uid, perms)| {
        User::by_id(&mut conn, uid).ok()?.map(|u| serde_json::json!({
            "username": u.username,
            "can_upload": perms & crate::models::PERM_UPLOAD != 0,
            "can_edit": perms & crate::models::PERM_EDIT != 0,
        }))
    }).collect();

    let mut ctx = base_context(&cfg, Some(&current_user));
    ctx.insert("group", &group);
    ctx.insert("members", &members);
    let html = tmpl.render("group_members.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn manage_members_post(
    session: Session,
    pool: web::Data<DbPool>,
    path: web::Path<String>,
    form: web::Form<MemberForm>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool)
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let group = Group::by_slug(&mut conn, &path.into_inner())
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Group not found"))?;
    if !group.can_edit(&mut conn, current_user.id) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }

    let target = User::by_username(&mut conn, &form.username)
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorBadRequest("User not found"))?;

    let mut perms = 0i32;
    if form.can_upload.is_some() { perms |= crate::models::PERM_UPLOAD; }
    if form.can_edit.is_some() { perms |= crate::models::PERM_EDIT; }

    let existing: Option<i32> = group_members::table
        .filter(group_members::group_id.eq(group.id))
        .filter(group_members::user_id.eq(target.id))
        .select(group_members::permissions)
        .first(&mut conn)
        .optional()
        .map_err(actix_web::error::ErrorInternalServerError)?;

    if perms == 0 {
        if existing.is_some() {
            diesel::delete(group_members::table
                .filter(group_members::group_id.eq(group.id))
                .filter(group_members::user_id.eq(target.id)))
                .execute(&mut conn)
                .map_err(actix_web::error::ErrorInternalServerError)?;
        }
    } else if existing.is_some() {
        diesel::update(group_members::table
            .filter(group_members::group_id.eq(group.id))
            .filter(group_members::user_id.eq(target.id)))
            .set(group_members::permissions.eq(perms))
            .execute(&mut conn)
            .map_err(actix_web::error::ErrorInternalServerError)?;
    } else {
        diesel::insert_into(group_members::table)
            .values((
                group_members::group_id.eq(group.id),
                group_members::user_id.eq(target.id),
                group_members::permissions.eq(perms),
            ))
            .execute(&mut conn)
            .map_err(actix_web::error::ErrorInternalServerError)?;
    }

    Ok(HttpResponse::Found()
        .insert_header(("Location", format!("/group/{}/members", group.slug)))
        .finish())
}
