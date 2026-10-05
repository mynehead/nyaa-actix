//! Context every page gets, so `layout.html` can rely on it.

use serde::Serialize;

use crate::config::Config;
use crate::models::User;

/// A category as the navbar search and listings show it (upstream's `nyaa_cats` / `suke_cats`).
#[derive(Debug, Serialize)]
pub struct NavCategory {
    /// "main_sub", as used in the `c=` search param.
    pub id: &'static str,
    /// Label in the navbar select; subcategories start with "- ".
    pub name: &'static str,
    /// Full "Main - Sub" name, used for titles and alt text.
    pub title: &'static str,
}

macro_rules! cats {
    ($(($id:expr, $name:expr, $title:expr)),* $(,)?) => {
        &[$(NavCategory { id: $id, name: $name, title: $title }),*]
    };
}

const NYAA_CATS: &[NavCategory] = cats![
    ("1_0", "Anime", "Anime"),
    ("1_1", "- Anime Music Video", "Anime - AMV"),
    ("1_2", "- English-translated", "Anime - English"),
    ("1_3", "- Non-English-translated", "Anime - Non-English"),
    ("1_4", "- Raw", "Anime - Raw"),
    ("2_0", "Audio", "Audio"),
    ("2_1", "- Lossless", "Audio - Lossless"),
    ("2_2", "- Lossy", "Audio - Lossy"),
    ("3_0", "Literature", "Literature"),
    ("3_1", "- English-translated", "Literature - English"),
    ("3_2", "- Non-English-translated", "Literature - Non-English"),
    ("3_3", "- Raw", "Literature - Raw"),
    ("4_0", "Live Action", "Live Action"),
    ("4_1", "- English-translated", "Live Action - English"),
    ("4_2", "- Idol/Promotional Video", "Live Action - Idol/PV"),
    ("4_3", "- Non-English-translated", "Live Action - Non-English"),
    ("4_4", "- Raw", "Live Action - Raw"),
    ("5_0", "Pictures", "Pictures"),
    ("5_1", "- Graphics", "Pictures - Graphics"),
    ("5_2", "- Photos", "Pictures - Photos"),
    ("6_0", "Software", "Software"),
    ("6_1", "- Applications", "Software - Apps"),
    ("6_2", "- Games", "Software - Games"),
];

const SUKEBEI_CATS: &[NavCategory] = cats![
    ("1_0", "Art", "Art"),
    ("1_1", "- Anime", "Art - Anime"),
    ("1_2", "- Doujinshi", "Art - Doujinshi"),
    ("1_3", "- Games", "Art - Games"),
    ("1_4", "- Manga", "Art - Manga"),
    ("1_5", "- Pictures", "Art - Pictures"),
    ("2_0", "Real Life", "Real Life"),
    ("2_1", "- Photobooks and Pictures", "Real Life - Pictures"),
    ("2_2", "- Videos", "Real Life - Videos"),
];

pub fn nav_categories(flavor: &str) -> &'static [NavCategory] {
    if flavor == "sukebei" { SUKEBEI_CATS } else { NYAA_CATS }
}

/// Current search, for refilling the navbar form and building sort and page links.
#[derive(Debug, Default, Serialize)]
pub struct SearchState {
    pub term: String,
    pub category: String,
    pub quality_filter: String,
    pub sort: String,
    pub order: String,
}

impl SearchState {
    pub fn new(q: &Option<String>, c: &Option<String>, f: &Option<String>, s: &Option<String>, o: &Option<String>) -> Self {
        let get = |v: &Option<String>, default: &str| v.clone().unwrap_or_else(|| default.to_string());
        SearchState {
            term: get(q, ""),
            category: get(c, "0_0"),
            quality_filter: get(f, "0"),
            sort: get(s, "id"),
            order: get(o, "desc"),
        }
    }
}

/// Starts a page context with the logged-in user, site config and navbar data.
pub fn base_context(cfg: &Config, current_user: Option<&User>) -> tera::Context {
    let mut ctx = tera::Context::new();
    ctx.insert("current_user", &current_user);
    ctx.insert("config", &serde_json::json!({
        "site_name": cfg.site_name,
        "site_flavor": cfg.site_flavor,
    }));
    let cats = nav_categories(&cfg.site_flavor);
    let names: std::collections::HashMap<&str, &str> = cats.iter().map(|c| (c.id, c.title)).collect();
    ctx.insert("nav_categories", cats);
    ctx.insert("category_names", &names);
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nyaa_categories_match_seeded_ids() {
        let ids: Vec<&str> = nav_categories("nyaa").iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), 23);
        assert!(ids.contains(&"4_2") && ids.contains(&"6_0"));
        assert_eq!(nav_categories("sukebei")[0].name, "Art");
    }

    #[test]
    fn search_state_defaults() {
        let s = SearchState::new(&None, &None, &None, &None, &None);
        assert_eq!((s.category.as_str(), s.quality_filter.as_str(), s.sort.as_str(), s.order.as_str()), ("0_0", "0", "id", "desc"));
    }
}
