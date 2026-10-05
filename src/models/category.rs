use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use crate::db::schema::{nyaa_main_categories, nyaa_sub_categories};

#[derive(Debug, Clone, Queryable, Selectable, Serialize, Deserialize)]
#[diesel(table_name = nyaa_main_categories)]
pub struct MainCategory {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize, Deserialize)]
#[diesel(table_name = nyaa_sub_categories)]
pub struct SubCategory {
    pub id: i32,
    pub main_category_id: i32,
    pub name: String,
}

impl SubCategory {
    /// The "main_sub" form used in the `c=` search param and the upload form (upstream `id_as_string`).
    #[allow(dead_code)]
    pub fn id_str(&self) -> String {
        format!("{}_{}", self.main_category_id, self.id)
    }
}

pub fn get_all_categories(conn: &mut SqliteConnection) -> QueryResult<Vec<(MainCategory, Vec<SubCategory>)>> {
    let mains = nyaa_main_categories::table
        .order(nyaa_main_categories::id.asc())
        .load::<MainCategory>(conn)?;
    let subs = nyaa_sub_categories::table
        .order(nyaa_sub_categories::main_category_id.asc())
        .then_order_by(nyaa_sub_categories::id.asc())
        .load::<SubCategory>(conn)?;

    Ok(mains.into_iter().map(|main| {
        let my_subs: Vec<SubCategory> = subs.iter()
            .filter(|s| s.main_category_id == main.id)
            .cloned()
            .collect();
        (main, my_subs)
    }).collect())
}

/// For checking an uploaded category exists.
pub fn get_sub_category(conn: &mut SqliteConnection, main_id: i32, sub_id: i32) -> QueryResult<Option<SubCategory>> {
    nyaa_sub_categories::table
        .filter(nyaa_sub_categories::main_category_id.eq(main_id))
        .filter(nyaa_sub_categories::id.eq(sub_id))
        .first(conn)
        .optional()
}

/// "Main - Sub" name for a torrent's category ids. For the listing and view page, which
/// still print the raw "1_2" ids.
#[allow(dead_code)]
pub fn category_display(conn: &mut SqliteConnection, main_id: i32, sub_id: i32) -> String {
    let main = nyaa_main_categories::table.find(main_id).first::<MainCategory>(conn).ok();
    let sub = nyaa_sub_categories::table
        .filter(nyaa_sub_categories::main_category_id.eq(main_id))
        .filter(nyaa_sub_categories::id.eq(sub_id))
        .first::<SubCategory>(conn)
        .ok();

    match (main, sub) {
        (Some(m), Some(s)) => {
            if sub_id == 0 { m.name } else { format!("{} - {}", m.name, s.name) }
        }
        (Some(m), None) => m.name,
        _ => "Unknown".to_string(),
    }
}
