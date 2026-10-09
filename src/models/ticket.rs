use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::{support_ticket_messages, support_tickets, users};
use crate::db::DbConnection;

/// `status` of a support ticket.
pub const TICKET_OPEN: i32 = 0;
pub const TICKET_CLOSED: i32 = 1;

pub const TICKET_SUBJECT_MIN: usize = 3;
pub const TICKET_SUBJECT_MAX: usize = 200;
pub const TICKET_MESSAGE_MIN: usize = 3;
pub const TICKET_MESSAGE_MAX: usize = 10_000;

pub const TICKETS_PER_PAGE: i64 = 25;

/// The trimmed subject, or why it can't be used.
pub fn validate_subject(subject: &str) -> Result<String, &'static str> {
    let subject = crate::utils::sanitize_string(subject).trim().to_string();
    if (TICKET_SUBJECT_MIN..=TICKET_SUBJECT_MAX).contains(&subject.chars().count()) {
        Ok(subject)
    } else {
        Err("Subject must be at least 3 characters long and 200 at most.")
    }
}

/// The trimmed message, line breaks kept, or why it can't be used.
pub fn validate_message(message: &str) -> Result<String, &'static str> {
    let message = crate::utils::sanitize_text(message).trim().to_string();
    if (TICKET_MESSAGE_MIN..=TICKET_MESSAGE_MAX).contains(&message.chars().count()) {
        Ok(message)
    } else {
        Err("Message must be at least 3 characters long and 10000 at most.")
    }
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = support_tickets)]
pub struct Ticket {
    pub id: i32,
    pub created_time: NaiveDateTime,
    pub updated_time: NaiveDateTime,
    pub subject: String,
    pub status: i32,
    /// Whether the last message is from staff.
    pub staff_replied: bool,
    pub user_id: i32,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = support_ticket_messages)]
pub struct TicketMessage {
    pub id: i32,
    pub created_time: NaiveDateTime,
    pub body: String,
    pub from_staff: bool,
    pub ticket_id: i32,
    pub user_id: Option<i32>,
}

/// A ticket with its opener's name, as the lists show it.
#[derive(Debug, Serialize)]
pub struct TicketRow {
    #[serde(flatten)]
    pub ticket: Ticket,
    pub username: String,
}

/// A message with its author's name, `None` when the account is gone.
#[derive(Debug, Serialize)]
pub struct TicketMessageRow {
    #[serde(flatten)]
    pub message: TicketMessage,
    pub author: Option<String>,
}

/// Which tickets the staff queue lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketFilter {
    Open,
    Closed,
    All,
}

impl TicketFilter {
    pub fn parse(s: Option<&str>) -> Option<TicketFilter> {
        match s.unwrap_or("open") {
            "open" | "" => Some(TicketFilter::Open),
            "closed" => Some(TicketFilter::Closed),
            "all" => Some(TicketFilter::All),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            TicketFilter::Open => "open",
            TicketFilter::Closed => "closed",
            TicketFilter::All => "all",
        }
    }
}

fn now() -> NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

impl Ticket {
    pub fn is_open(&self) -> bool {
        self.status == TICKET_OPEN
    }

    /// Opens a ticket with its first message; returns the new ticket's id.
    pub fn create(conn: &mut DbConnection, user_id: i32, subject: &str, body: &str) -> QueryResult<i32> {
        conn.transaction(|conn| {
            let now = now();
            diesel::insert_into(support_tickets::table)
                .values((
                    support_tickets::created_time.eq(now),
                    support_tickets::updated_time.eq(now),
                    support_tickets::subject.eq(subject),
                    support_tickets::status.eq(TICKET_OPEN),
                    support_tickets::staff_replied.eq(false),
                    support_tickets::user_id.eq(user_id),
                ))
                .execute(conn)?;
            let id = support_tickets::table
                .filter(support_tickets::user_id.eq(user_id))
                .order(support_tickets::id.desc())
                .select(support_tickets::id)
                .first(conn)?;
            insert_message(conn, id, user_id, body, false, now)?;
            Ok(id)
        })
    }

    pub fn by_id(conn: &mut DbConnection, id: i32) -> QueryResult<Option<Ticket>> {
        support_tickets::table.find(id).select(Ticket::as_select()).first(conn).optional()
    }

    /// The user's tickets, open ones first, then by last activity.
    pub fn of_user(conn: &mut DbConnection, user_id: i32) -> QueryResult<Vec<Ticket>> {
        support_tickets::table
            .filter(support_tickets::user_id.eq(user_id))
            .order((support_tickets::status.asc(), support_tickets::updated_time.desc(), support_tickets::id.desc()))
            .select(Ticket::as_select())
            .load(conn)
    }

    /// One page of the staff queue and the number of matching tickets. Open tickets come
    /// longest-waiting first; closed and all, most recently active first.
    pub fn queue(conn: &mut DbConnection, filter: TicketFilter, page: i64) -> QueryResult<(Vec<TicketRow>, i64)> {
        let filtered = || {
            let q = support_tickets::table.into_boxed();
            match filter {
                TicketFilter::Open => q.filter(support_tickets::status.eq(TICKET_OPEN)),
                TicketFilter::Closed => q.filter(support_tickets::status.eq(TICKET_CLOSED)),
                TicketFilter::All => q,
            }
        };
        let total = filtered().count().get_result(conn)?;
        let q = filtered().inner_join(users::table);
        let q = if filter == TicketFilter::Open {
            q.order((
                support_tickets::staff_replied.asc(),
                support_tickets::updated_time.asc(),
                support_tickets::id.asc(),
            ))
        } else {
            q.order((support_tickets::updated_time.desc(), support_tickets::id.desc()))
        };
        let rows: Vec<(Ticket, String)> = q
            .select((Ticket::as_select(), users::username))
            .limit(TICKETS_PER_PAGE)
            .offset((page.max(1) - 1) * TICKETS_PER_PAGE)
            .load(conn)?;
        Ok((rows.into_iter().map(|(ticket, username)| TicketRow { ticket, username }).collect(), total))
    }

    pub fn messages(&self, conn: &mut DbConnection) -> QueryResult<Vec<TicketMessageRow>> {
        let rows: Vec<(TicketMessage, Option<String>)> = support_ticket_messages::table
            .left_join(users::table)
            .filter(support_ticket_messages::ticket_id.eq(self.id))
            .order(support_ticket_messages::id.asc())
            .select((TicketMessage::as_select(), users::username.nullable()))
            .load(conn)?;
        Ok(rows.into_iter().map(|(message, author)| TicketMessageRow { message, author }).collect())
    }

    /// Adds a reply and marks who answered last.
    pub fn reply(&self, conn: &mut DbConnection, user_id: i32, body: &str, from_staff: bool) -> QueryResult<()> {
        conn.transaction(|conn| {
            let now = now();
            insert_message(conn, self.id, user_id, body, from_staff, now)?;
            diesel::update(support_tickets::table.find(self.id))
                .set((support_tickets::updated_time.eq(now), support_tickets::staff_replied.eq(from_staff)))
                .execute(conn)
                .map(|_| ())
        })
    }

    pub fn set_status(&self, conn: &mut DbConnection, status: i32) -> QueryResult<()> {
        diesel::update(support_tickets::table.find(self.id))
            .set((support_tickets::status.eq(status), support_tickets::updated_time.eq(now())))
            .execute(conn)
            .map(|_| ())
    }

    /// Tickets the user opened since `since`, for the rate limit.
    pub fn opened_since(conn: &mut DbConnection, user_id: i32, since: NaiveDateTime) -> QueryResult<i64> {
        support_tickets::table
            .filter(support_tickets::user_id.eq(user_id))
            .filter(support_tickets::created_time.gt(since))
            .count()
            .get_result(conn)
    }

    /// Replies the user wrote since `since` (not counting the messages that opened tickets),
    /// for the rate limit.
    pub fn replies_since(conn: &mut DbConnection, user_id: i32, since: NaiveDateTime) -> QueryResult<i64> {
        let messages: i64 = support_ticket_messages::table
            .filter(support_ticket_messages::user_id.eq(user_id))
            .filter(support_ticket_messages::created_time.gt(since))
            .count()
            .get_result(conn)?;
        Ok((messages - Self::opened_since(conn, user_id, since)?).max(0))
    }
}

fn insert_message(
    conn: &mut DbConnection,
    ticket_id: i32,
    user_id: i32,
    body: &str,
    from_staff: bool,
    at: NaiveDateTime,
) -> QueryResult<()> {
    diesel::insert_into(support_ticket_messages::table)
        .values((
            support_ticket_messages::created_time.eq(at),
            support_ticket_messages::body.eq(body),
            support_ticket_messages::from_staff.eq(from_staff),
            support_ticket_messages::ticket_id.eq(ticket_id),
            support_ticket_messages::user_id.eq(Some(user_id)),
        ))
        .execute(conn)
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_and_message_lengths() {
        assert!(validate_subject(" ab ").is_err());
        assert_eq!(validate_subject(" Help\u{0} me "), Ok("Help me".into()));
        assert!(validate_subject(&"é".repeat(201)).is_err());
        assert_eq!(validate_message(" line one\r\nline two "), Ok("line one\nline two".into()));
        assert!(validate_message("hi").is_err());
        assert!(validate_message(&"x".repeat(10_001)).is_err());
    }

    #[test]
    fn filter_parses_the_tabs() {
        assert_eq!(TicketFilter::parse(None), Some(TicketFilter::Open));
        assert_eq!(TicketFilter::parse(Some("closed")), Some(TicketFilter::Closed));
        assert_eq!(TicketFilter::parse(Some("all")).map(TicketFilter::name), Some("all"));
        assert_eq!(TicketFilter::parse(Some("nope")), None);
    }
}
