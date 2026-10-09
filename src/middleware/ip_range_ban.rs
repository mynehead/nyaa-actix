//! IP range bans: nobody connecting from a banned network (CIDR, IPv4 or IPv6) can use the
//! site at all; every request gets a plain 403. The list lives in memory, so the check
//! costs no database query. It is reloaded right after a change on /admin/bans and once a
//! minute, which also picks up `unban-ip-range` from the command line.

use std::net::IpAddr;
use std::sync::RwLock;
use std::time::Duration;

use actix_web::body::{BoxBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::middleware::Next;
use actix_web::{web, Error, HttpResponse};
use chrono::NaiveDateTime;

use crate::db::{DbConnection, DbPool};
use crate::models::IpRangeBan;
use crate::utils::client_addr;
use crate::utils::proxy::IpNet;

/// How often the list is re-read from the database even without a change here.
const RELOAD_EVERY: Duration = Duration::from_secs(60);

/// The banned networks, shared by all workers as `web::Data<IpRangeBans>`.
#[derive(Default)]
pub struct IpRangeBans {
    ranges: RwLock<Vec<(IpNet, Option<NaiveDateTime>)>>,
}

impl IpRangeBans {
    pub fn load(conn: &mut DbConnection) -> diesel::QueryResult<IpRangeBans> {
        let bans = IpRangeBans::default();
        bans.reload(conn)?;
        Ok(bans)
    }

    pub fn reload(&self, conn: &mut DbConnection) -> diesel::QueryResult<()> {
        let ranges = IpRangeBan::all(conn)?
            .iter()
            .filter_map(|ban| match ban.net() {
                Some(net) => Some((net, ban.expires_time)),
                None => {
                    log::warn!("Skipping IP range ban #{} with an unreadable range `{}`", ban.id, ban.cidr);
                    None
                }
            })
            .collect();
        *self.ranges.write().unwrap_or_else(|e| e.into_inner()) = ranges;
        Ok(())
    }

    /// The ban covering `ip`, if any is in force at `now`: its range and expiry.
    pub fn find(&self, ip: IpAddr, now: NaiveDateTime) -> Option<(IpNet, Option<NaiveDateTime>)> {
        let ranges = self.ranges.read().unwrap_or_else(|e| e.into_inner());
        ranges.iter().find(|(net, expires)| expires.is_none_or(|t| t > now) && net.contains(ip)).copied()
    }
}

/// Re-reads the list every `RELOAD_EVERY` in a background thread.
pub fn spawn_reload(pool: DbPool, bans: web::Data<IpRangeBans>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(RELOAD_EVERY);
        let result =
            pool.get().map_err(|e| e.to_string()).and_then(|mut c| bans.reload(&mut c).map_err(|e| e.to_string()));
        if let Err(e) = result {
            log::warn!("Could not reload the IP range bans: {e}");
        }
    });
}

pub async fn reject_banned_range(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<BoxBody>, Error> {
    let bans = req.app_data::<web::Data<IpRangeBans>>();
    if let (Some(bans), Some(ip)) = (bans, client_addr(req.request())) {
        if let Some((_, expires)) = bans.find(ip, chrono::Utc::now().naive_utc()) {
            let until = match expires {
                Some(t) => format!(" until {} UTC", t.format("%Y-%m-%d %H:%M")),
                None => String::new(),
            };
            let response = HttpResponse::Forbidden()
                .content_type("text/plain; charset=utf-8")
                .body(format!("Your network is banned from this site{until}."));
            return Ok(req.into_response(response));
        }
    }
    next.call(req).await.map(ServiceResponse::map_into_boxed_body)
}
