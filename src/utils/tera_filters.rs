//! Tera can't call Rust methods from templates, so the model helpers
//! templates need are exposed as filters instead.

use std::collections::HashMap;

use serde::de::DeserializeOwned;
use tera::{to_value, Result, Tera, Value};

use crate::models::{Torrent, User};

fn from_value<T: DeserializeOwned>(value: &Value, filter: &str) -> Result<T> {
    serde_json::from_value(value.clone())
        .map_err(|e| tera::Error::msg(format!("filter `{}`: {}", filter, e)))
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
    tera.register_filter("is_remake", torrent_filter("is_remake", |t| Value::Bool(t.is_remake())));
    tera.register_filter("row_class", torrent_filter("row_class", |t| to_value(t.row_class()).unwrap()));
    tera.register_filter("filesize_human", torrent_filter("filesize_human", |t| to_value(t.filesize_human()).unwrap()));
    tera.register_filter("level_str", user_filter("level_str", |u| to_value(u.level_str()).unwrap()));
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
