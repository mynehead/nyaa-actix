//! Tera can't call Rust methods from templates, so the model helpers
//! templates need are exposed as filters instead.

use std::collections::HashMap;

use serde::de::DeserializeOwned;
use tera::{to_value, Result, Tera, Value};

use crate::models::{Torrent, User};

fn from_value<T: DeserializeOwned>(value: &Value, filter: &str) -> Result<T> {
    serde_json::from_value(value.clone()).map_err(|e| tera::Error::msg(format!("filter `{}`: {}", filter, e)))
}

fn torrent_filter(
    name: &'static str,
    f: fn(&Torrent) -> Value,
) -> impl Fn(&Value, &HashMap<String, Value>) -> Result<Value> + Send + Sync {
    move |value, _| Ok(f(&from_value::<Torrent>(value, name)?))
}

fn user_filter(
    name: &'static str,
    f: fn(&User) -> Value,
) -> impl Fn(&Value, &HashMap<String, Value>) -> Result<Value> + Send + Sync {
    move |value, _| Ok(f(&from_value::<User>(value, name)?))
}

pub fn register(tera: &mut Tera) {
    tera.register_filter("is_trusted", torrent_filter("is_trusted", |t| Value::Bool(t.is_trusted())));
    tera.register_filter("is_anonymous", torrent_filter("is_anonymous", |t| Value::Bool(t.is_anonymous())));
    tera.register_filter("is_remake", torrent_filter("is_remake", |t| Value::Bool(t.is_remake())));
    tera.register_filter("is_hidden", torrent_filter("is_hidden", |t| Value::Bool(t.is_hidden())));
    tera.register_filter("is_complete", torrent_filter("is_complete", |t| Value::Bool(t.is_complete())));
    tera.register_filter("is_deleted", torrent_filter("is_deleted", |t| Value::Bool(t.is_deleted() || t.is_banned())));
    tera.register_filter("row_class", torrent_filter("row_class", |t| to_value(t.row_class()).unwrap()));
    tera.register_filter(
        "is_comment_locked",
        torrent_filter("is_comment_locked", |t| Value::Bool(t.is_comment_locked())),
    );
    tera.register_filter("filesize", |value: &Value, _: &HashMap<String, Value>| {
        let bytes = value.as_i64().ok_or_else(|| tera::Error::msg("filter `filesize`: expected an integer"))?;
        Ok(to_value(crate::models::format_filesize(bytes)).unwrap())
    });
    tera.register_filter(
        "information_as_link",
        torrent_filter("information_as_link", |t| to_value(t.information_as_link()).unwrap()),
    );
    tera.register_filter("filesize_human", torrent_filter("filesize_human", |t| to_value(t.filesize_human()).unwrap()));
    tera.register_filter("level_str", user_filter("level_str", |u| to_value(u.level_str()).unwrap()));
    tera.register_filter("status_str", user_filter("status_str", |u| to_value(u.status_str()).unwrap()));
    tera.register_filter("level_color", user_filter("level_color", |u| to_value(u.level_color()).unwrap()));
}

#[cfg(test)]
mod tests {
    #[test]
    fn all_templates_parse() {
        let mut tera = tera::Tera::new("templates/**/*").expect("templates should parse");
        super::register(&mut tera);
        assert!(tera.get_template_names().count() > 10);
    }
}

#[cfg(test)]
mod render_tests {
    use crate::search::db::ListedTorrent;
    use crate::utils::context::{base_context, SearchState};
    use crate::utils::pagination::Pagination;
    use std::collections::HashMap;

    fn tera() -> tera::Tera {
        let mut tera = tera::Tera::new("templates/**/*").unwrap();
        super::register(&mut tera);
        tera
    }

    fn config() -> crate::config::Config {
        crate::config::Config {
            database_url: String::new(),
            secret_key: String::new(),
            site_name: "Nyaa".into(),
            site_flavor: "nyaa".into(),
            results_per_page: 75,
            max_pages: 0,
            torrent_storage_path: String::new(),
            avatar_storage_path: String::new(),
            enable_gravatar: false,
            maintenance: Default::default(),
            raid_mode: Default::default(),
            site_url: String::new(),
            tracker_urls: vec![],
            trusted_proxies: vec![],
            meili: None,
            tracker: None,
            ratelimit_account_age: 0,
            editing_time_limit: 0,
            upload_limit: Default::default(),
            mail: Default::default(),
            trusted: Default::default(),
            tickets: Default::default(),
            mfa: Default::default(),
        }
    }

    fn render(name: &str, ctx: &tera::Context) -> String {
        tera().render(name, ctx).unwrap_or_else(|e| {
            let mut msg = format!("{}", e);
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                msg += &format!("\n  caused by: {}", s);
                src = s.source();
            }
            panic!("{}", msg)
        })
    }

    #[test]
    fn listing_renders_rows_sort_links_and_pages() {
        let mut ctx = base_context(&config(), None);
        let torrent = crate::torrent::tests::sample_torrent();
        ctx.insert(
            "torrents",
            &vec![ListedTorrent {
                title: torrent.display_name.clone(),
                torrent,
                seed_count: 4,
                leech_count: 2,
                download_count: 9,
                group: None,
            }],
        );
        ctx.insert("pagination", &Pagination::new(2, 500, 75));
        ctx.insert(
            "search",
            &SearchState::new(
                &Some("a&b".into()),
                &Some("1_2".into()),
                &None,
                &Some("size".into()),
                &Some("desc".into()),
            ),
        );
        let html = render("home.html", &ctx);
        assert!(html.contains("<title>a&amp;b :: Nyaa</title>"), "{}", html);
        // Sort link flips the active column's order and keeps the search
        assert!(html.contains("href=\"?q=a%26b&amp;c=1_2&amp;s=size&amp;o=asc&amp;p=1\""), "{}", html);
        assert!(html.contains("href=\"?q=a%26b&amp;c=1_2&amp;s=size&amp;o=desc&amp;p=3\""), "{}", html);
        assert!(html.contains("<td class=\"text-center\">4</td>"));
        assert!(html.contains("/static/img/icons/nyaa/1_1.png"));
    }

    fn user(level: i32) -> crate::models::User {
        crate::models::User {
            id: 1,
            username: "alice".into(),
            email: Some("a@example.com".into()),
            password_hash: String::new(),
            status: 1,
            level,
            created_time: chrono::NaiveDateTime::default(),
            last_login_date: None,
            last_login_ip: None,
            registration_ip: None,
            avatar_time: None,
        }
    }

    #[test]
    fn view_renders_files_comments_and_escapes_description() {
        let mut torrent = crate::torrent::tests::sample_torrent();
        torrent.description = "<script>x</script>".into();
        torrent.information = "https://example.com".into();
        torrent.flags = crate::models::TorrentFlags::ANONYMOUS.bits();
        torrent.uploader_id = Some(1);
        let info = b"d5:filesld6:lengthi3e4:pathl3:sub5:a.txteee4:name4:root12:piece lengthi1e6:pieces0:e";
        let (files, count) = crate::torrent::file_tree(info).unwrap();

        let mut ctx = base_context(&config(), Some(&user(2)));
        ctx.insert("torrent", &torrent);
        ctx.insert("info_hash", "ab");
        ctx.insert("main_category", &serde_json::json!({ "id": 1, "name": "Anime" }));
        ctx.insert("sub_category", &serde_json::json!({ "id": 1, "main_category_id": 1, "name": "Anime Music Video" }));
        ctx.insert("stats", &Option::<crate::models::Statistic>::None);
        ctx.insert("files", &Some(files));
        ctx.insert("file_count", &count);
        ctx.insert("max_files_view", &1000);
        ctx.insert("comments", &vec![serde_json::json!({
            "comment": { "id": 3, "torrent_id": 7, "user_id": 1, "created_time": "2024-01-01T00:00:00", "edited_time": null, "text": "hi" },
            "user": user(0),
            "avatar_url": "/avatar/1?v=5",
        })]);
        ctx.insert("uploader", &Some(user(0)));
        ctx.insert("magnet", "magnet:?xt=urn:btih:ab");
        let html = render("view.html", &ctx);
        assert!(html.contains("&lt;script&gt;x&lt;&#x2F;script&gt;"), "{}", html);
        assert!(html.contains("Anonymous (<a class=\"text-default\" href=\"/user/alice\""), "{}", html);
        assert!(
            html.contains("<i class=\"fa fa-file\"></i>a.txt <span class=\"file-size\">(3 Bytes)</span>"),
            "{}",
            html
        );
        assert!(html.contains("href=\"https://example.com\""));
        // Anonymous upload: the commenter isn't marked as the uploader
        assert!(!html.contains("(uploader)"));
        assert!(html.contains("<img class=\"avatar\" src=\"&#x2F;avatar&#x2F;1?v=5\""), "{}", html);
        assert!(html.contains("<div class=\"collapse in\" id=\"collapse-comments\">"));
        // "Hide comments by default" starts the panel collapsed
        ctx.insert("hide_comments", &true);
        let html = render("view.html", &ctx);
        assert!(html.contains("<div class=\"collapse \" id=\"collapse-comments\">"), "{}", html);
        assert!(html.contains("aria-expanded=\"false\" aria-controls=\"collapse-comments\""));
    }

    #[test]
    fn simple_pages_render() {
        let cfg = config();
        for (page, logged_in) in [
            ("404.html", false),
            ("rules.html", false),
            ("help.html", true),
            ("profile.html", true),
            ("login.html", false),
        ] {
            let u = user(3);
            let mut ctx = base_context(&cfg, logged_in.then_some(&u));
            ctx.insert("error", &Option::<String>::None);
            // profile.html
            ctx.insert("avatar_url", crate::models::DEFAULT_AVATAR);
            ctx.insert("active_tab", "password");
            ctx.insert("hide_comments", &false);
            ctx.insert("email_value", "");
            for errors in ["password_errors", "email_errors"] {
                ctx.insert(errors, &HashMap::<String, Vec<String>>::new());
            }
            render(page, &ctx);
        }
        let mut ctx = base_context(&cfg, None);
        ctx.insert("errors", &vec!["Bad"]);
        assert!(render("register.html", &ctx).contains("<li>Bad</li>"));

        let mut ctx = base_context(&cfg, Some(&user(1)));
        ctx.insert("categories", &Vec::<(crate::models::MainCategory, Vec<crate::models::SubCategory>)>::new());
        ctx.insert("groups", &Vec::<crate::models::Group>::new());
        ctx.insert("form", &crate::handlers::torrents::EditForm::default());
        ctx.insert("group_id", &None::<i32>);
        ctx.insert("errors", &std::collections::HashMap::<&str, String>::new());
        assert!(render("upload.html", &ctx).contains("name=\"is_trusted\""));
    }
}
