use crate::db::schema::{nyaa_main_categories, nyaa_sub_categories};
use crate::db::DbConnection;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

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

pub fn get_all_categories(conn: &mut DbConnection) -> QueryResult<Vec<(MainCategory, Vec<SubCategory>)>> {
    let mains = nyaa_main_categories::table.order(nyaa_main_categories::id.asc()).load::<MainCategory>(conn)?;
    let subs = nyaa_sub_categories::table
        .order(nyaa_sub_categories::main_category_id.asc())
        .then_order_by(nyaa_sub_categories::id.asc())
        .load::<SubCategory>(conn)?;

    Ok(mains
        .into_iter()
        .map(|main| {
            let my_subs: Vec<SubCategory> = subs.iter().filter(|s| s.main_category_id == main.id).cloned().collect();
            (main, my_subs)
        })
        .collect())
}

/// Looks up a subcategory, for the view page (and later for validating uploads).
pub fn get_sub_category(conn: &mut DbConnection, main_id: i32, sub_id: i32) -> QueryResult<Option<SubCategory>> {
    nyaa_sub_categories::table
        .filter(nyaa_sub_categories::main_category_id.eq(main_id))
        .filter(nyaa_sub_categories::id.eq(sub_id))
        .first(conn)
        .optional()
}
