use std::collections::HashMap;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::db::schema::{invites, users};
use crate::db::DbConnection;

/// An invite code for REGISTRATION_MODE=invite, made on /admin/invites. One code makes one
/// account; once used, `inviter_id -> used_by` records who invited whom.
#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = invites)]
pub struct Invite {
    pub id: i32,
    // code_hash is left out: it is only ever looked up, never read back
    pub inviter_id: i32,
    /// Only this address may use the code; it then counts as verified.
    pub email: Option<String>,
    pub created_time: NaiveDateTime,
    pub expires_time: NaiveDateTime,
    pub used_by: Option<i32>,
    pub used_time: Option<NaiveDateTime>,
    pub revoked: bool,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = invites)]
pub struct NewInvite {
    pub code_hash: String,
    pub inviter_id: i32,
    pub email: Option<String>,
    pub created_time: NaiveDateTime,
    pub expires_time: NaiveDateTime,
}

/// An invite with what /admin/invites shows next to it.
#[derive(Debug, Clone, Serialize)]
pub struct InviteEntry {
    #[serde(flatten)]
    pub invite: Invite,
    pub inviter_name: String,
    pub used_by_name: Option<String>,
    /// "open", "used", "revoked" or "expired".
    pub state: &'static str,
}

/// A fresh code (128 random bits, URL-safe) and the hash to store for it.
pub fn new_invite_code() -> (String, String) {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let code = URL_SAFE_NO_PAD.encode(bytes);
    let hash = hash_invite_code(&code);
    (code, hash)
}

pub fn hash_invite_code(code: &str) -> String {
    hex::encode(Sha256::digest(code.trim().as_bytes()))
}

impl Invite {
    pub fn state(&self, now: NaiveDateTime) -> &'static str {
        if self.used_by.is_some() {
            "used"
        } else if self.revoked {
            "revoked"
        } else if self.expires_time <= now {
            "expired"
        } else {
            "open"
        }
    }

    /// Whether `email` may register with this invite (any address when it isn't bound to one).
    pub fn allows_email(&self, email: &str) -> bool {
        self.email.as_deref().is_none_or(|bound| bound.eq_ignore_ascii_case(email.trim()))
    }

    pub fn by_id(conn: &mut DbConnection, id: i32) -> QueryResult<Option<Invite>> {
        invites::table.find(id).select(Invite::as_select()).first(conn).optional()
    }

    /// The invite for a code someone typed in, if it can still be used.
    pub fn usable_by_code(conn: &mut DbConnection, code: &str, now: NaiveDateTime) -> QueryResult<Option<Invite>> {
        if code.trim().is_empty() {
            return Ok(None);
        }
        let invite = invites::table
            .filter(invites::code_hash.eq(hash_invite_code(code)))
            .select(Invite::as_select())
            .first(conn)
            .optional()?;
        Ok(invite.filter(|i| i.state(now) == "open"))
    }

    pub fn insert(conn: &mut DbConnection, invite: &NewInvite) -> QueryResult<usize> {
        diesel::insert_into(invites::table).values(invite).execute(conn)
    }

    /// Marks the invite used by `user_id`; false when someone else got there first or it
    /// was revoked in the meantime.
    pub fn claim(conn: &mut DbConnection, id: i32, user_id: i32, now: NaiveDateTime) -> QueryResult<bool> {
        let claimed = diesel::update(
            invites::table
                .find(id)
                .filter(invites::used_by.is_null())
                .filter(invites::revoked.eq(false))
                .filter(invites::expires_time.gt(now)),
        )
        .set((invites::used_by.eq(user_id), invites::used_time.eq(now)))
        .execute(conn)?;
        Ok(claimed == 1)
    }

    /// Revokes an unused invite; false when it was already used.
    pub fn revoke(conn: &mut DbConnection, id: i32) -> QueryResult<bool> {
        let revoked = diesel::update(invites::table.find(id).filter(invites::used_by.is_null()))
            .set(invites::revoked.eq(true))
            .execute(conn)?;
        Ok(revoked == 1)
    }

    /// Who invited `user_id`, by name, if they registered with an invite.
    pub fn inviter_of(conn: &mut DbConnection, user_id: i32) -> QueryResult<Option<String>> {
        invites::table
            .inner_join(users::table.on(users::id.eq(invites::inviter_id)))
            .filter(invites::used_by.eq(user_id))
            .select(users::username)
            .first(conn)
            .optional()
    }

    /// One page of invites, newest first, with inviter and invitee names; and the total.
    pub fn page(
        conn: &mut DbConnection,
        page: i64,
        per_page: i64,
        now: NaiveDateTime,
    ) -> QueryResult<(Vec<InviteEntry>, i64)> {
        let total = invites::table.count().get_result(conn)?;
        let rows: Vec<Invite> = invites::table
            .select(Invite::as_select())
            .order((invites::created_time.desc(), invites::id.desc()))
            .offset((page - 1) * per_page)
            .limit(per_page)
            .load(conn)?;
        let mut ids: Vec<i32> = rows.iter().flat_map(|i| [Some(i.inviter_id), i.used_by]).flatten().collect();
        ids.sort_unstable();
        ids.dedup();
        let names: HashMap<i32, String> = users::table
            .filter(users::id.eq_any(&ids))
            .select((users::id, users::username))
            .load::<(i32, String)>(conn)?
            .into_iter()
            .collect();
        let entries = rows
            .into_iter()
            .map(|invite| InviteEntry {
                inviter_name: names.get(&invite.inviter_id).cloned().unwrap_or_default(),
                used_by_name: invite.used_by.and_then(|id| names.get(&id).cloned()),
                state: invite.state(now),
                invite,
            })
            .collect();
        Ok((entries, total))
    }
}
