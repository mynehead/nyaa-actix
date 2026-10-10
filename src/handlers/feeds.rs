//! The RSS feed (upstream's `rss.xml`, with its `nyaa:` elements) and the page its
//! namespace URL points to.

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Result};
use serde::Serialize;
use tera::Tera;

use crate::config::Config;
use crate::db::DbConnection;
use crate::models::{format_filesize, get_all_categories};
use crate::search::db::ListedTorrent;
use crate::utils::context::base_context;
use crate::utils::internal_error;

/// One `<item>`, with everything already formatted as upstream prints it and escaped for
/// XML. Tera's own escaping would also turn every `/` in the URLs into `&#x2F;`.
#[derive(Serialize)]
struct FeedItem {
    id: i32,
    title: String,
    link: String,
    view_url: String,
    pub_date: String,
    seeders: i32,
    leechers: i32,
    downloads: i32,
    info_hash: String,
    category_id: String,
    category: String,
    size: String,
    comments: i32,
    trusted: &'static str,
    remake: &'static str,
    best: &'static str,
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "Yes"
    } else {
        "No"
    }
}

/// The feed for `torrents`; `label` is "Home" or the quoted search term, and
/// `magnets` links items to magnets instead of .torrent files.
pub fn render_rss(
    conn: &mut DbConnection,
    tmpl: &Tera,
    cfg: &Config,
    label: &str,
    torrents: &[ListedTorrent],
    magnets: bool,
) -> Result<String> {
    let site = &cfg.site_url;
    // "Anime - English-translated", as upstream's category_name
    let categories: HashMap<String, String> = get_all_categories(conn)
        .map_err(internal_error)?
        .into_iter()
        .flat_map(|(main, subs)| {
            subs.into_iter()
                .map(move |sub| (format!("{}_{}", main.id, sub.id), format!("{} - {}", main.name, sub.name)))
        })
        .collect();
    let trackers = cfg.trackers();
    let items: Vec<FeedItem> = torrents
        .iter()
        .map(|listed| {
            let t = &listed.torrent;
            let category_id = format!("{}_{}", t.main_category_id, t.sub_category_id);
            FeedItem {
                id: t.id,
                title: xml(&t.display_name),
                link: xml(&if magnets {
                    t.magnet_uri(&t.display_name, &trackers)
                } else {
                    format!("{site}/download/{}.torrent", t.id)
                }),
                view_url: xml(&format!("{site}/view/{}", t.id)),
                pub_date: t.created_time.format("%a, %d %b %Y %H:%M:%S -0000").to_string(),
                seeders: listed.seed_count,
                leechers: listed.leech_count,
                downloads: listed.download_count,
                info_hash: t.info_hash_hex(),
                category: xml(categories.get(&category_id).map_or("", String::as_str)),
                category_id,
                size: format_filesize(t.filesize),
                comments: t.comment_count,
                trusted: yes_no(t.is_trusted()),
                remake: yes_no(t.is_remake()),
                best: yes_no(t.is_best()),
            }
        })
        .collect();

    let mut ctx = tera::Context::new();
    ctx.insert("site_name", &xml(&cfg.site_name));
    ctx.insert("site_url", &xml(site));
    ctx.insert("term", &xml(label));
    ctx.insert("magnet_links", &magnets);
    ctx.insert("items", &items);
    tmpl.render("rss.xml", &ctx).map_err(internal_error)
}

/// `/xmlns/nyaa`: what the feed's `nyaa:` elements mean.
pub async fn xmlns_nyaa(tmpl: web::Data<Tera>, cfg: web::Data<Config>) -> Result<HttpResponse> {
    let html = tmpl.render("xmlns.html", &base_context(&cfg, None)).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_escaped_for_xml() {
        assert_eq!(xml(r#"a<b> & "c" 'd' ]]>"#), "a&lt;b&gt; &amp; &quot;c&quot; &apos;d&apos; ]]&gt;");
        assert_eq!(xml("https://x/view/1"), "https://x/view/1");
    }
}
