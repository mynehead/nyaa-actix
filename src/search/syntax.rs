//! Search operators in the text a visitor types into the search box.
//!
//! Every part must match (AND):
//! - `word` matches names containing it; `"two words"` matches that exact phrase.
//! - `-word`, `!word`, `-"two words"` leave out names containing it.
//! - `user:name` and `group:slug` keep only that uploader's or group's torrents;
//!   `-user:name` / `!group:slug` leave them out. Values may be quoted.
//!
//! A whole term that is an info hash (40 hex or 32 base32 characters) jumps straight to the
//! torrent instead; see `info_hash`.

use diesel::prelude::*;

use crate::db::schema::{groups, users};
use crate::db::DbConnection;

/// Free text: a word, or a quoted phrase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    pub text: String,
    pub phrase: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Text(Text),
    User(String),
    Group(String),
}

/// One part of a search, possibly negated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub token: Token,
    pub negated: bool,
}

/// Splits a search into its parts. Never fails: anything that isn't an operator is text.
pub fn parse(input: &str) -> Vec<Part> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut parts = Vec::new();

    // Reads a quoted string (i at the opening quote) or a bare word up to whitespace
    let read = |i: &mut usize| -> (String, bool) {
        if chars.get(*i) == Some(&'"') {
            let start = *i + 1;
            let end = (start..chars.len()).find(|&j| chars[j] == '"').unwrap_or(chars.len());
            *i = end + 1;
            (chars[start..end].iter().collect(), true)
        } else {
            let start = *i;
            while *i < chars.len() && !chars[*i].is_whitespace() {
                *i += 1;
            }
            (chars[start..*i].iter().collect(), false)
        }
    };

    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        // A lone "-" (as in "Show - 01") is text, not a negation of nothing
        let negated = matches!(chars[i], '-' | '!')
            && chars.get(i + 1).is_some_and(|c| !c.is_whitespace() && !matches!(c, '-' | '!'));
        if negated {
            i += 1;
        }
        let (word, phrase) = read(&mut i);
        let token = if phrase { None } else { operator(&word, &chars, &mut i, &read) };
        let token = match token {
            Some(t) => t,
            None if word.trim().is_empty() => continue,
            None => Token::Text(Text { text: word, phrase }),
        };
        parts.push(Part { token, negated });
    }
    parts
}

/// `user:name` or `group:slug`, with the value right after the colon or, when the word
/// ends at the colon and a quote follows, the quoted value.
fn operator(word: &str, chars: &[char], i: &mut usize, read: &dyn Fn(&mut usize) -> (String, bool)) -> Option<Token> {
    let (key, value) = word.split_once(':')?;
    let make: fn(String) -> Token = match key.to_ascii_lowercase().as_str() {
        "user" | "u" => Token::User,
        "group" | "g" => Token::Group,
        _ => return None,
    };
    let value = if let Some(quoted) = value.strip_prefix('"') {
        // `user:"a b"`: the word stopped at the space; read on to the closing quote
        if let Some(v) = quoted.strip_suffix('"') {
            v.to_string()
        } else {
            let mut j = *i - quoted.chars().count() - 1;
            let (v, _) = read(&mut j);
            *i = j;
            v
        }
    } else if value.is_empty() && chars.get(*i) == Some(&'"') {
        read(i).0
    } else {
        value.to_string()
    };
    let value = value.trim().to_string();
    (!value.is_empty()).then(|| make(value))
}

/// The text of a search with its operators resolved against the database.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub include: Vec<Text>,
    pub exclude: Vec<Text>,
    /// Uploader ids every result must have, and whether their anonymous uploads count
    /// as theirs (only for moderators and the uploader themselves).
    pub users: Vec<(i32, bool)>,
    pub not_users: Vec<(i32, bool)>,
    pub groups: Vec<i32>,
    pub not_groups: Vec<i32>,
    /// A `user:` or `group:` that names nobody: nothing can match.
    pub matches_nothing: bool,
}

impl Resolved {
    /// Resolves `user:` and `group:` names (ignoring case). Anonymous uploads count as a
    /// user's only when `reveals(id)`, so neither `user:` nor `-user:` can tie them to
    /// their uploader. An unknown name to exclude is ignored.
    pub fn new(conn: &mut DbConnection, term: &str, reveals: impl Fn(i32) -> bool) -> QueryResult<Self> {
        use super::db::lower;
        let mut r = Resolved::default();
        for part in parse(term) {
            match part.token {
                Token::Text(t) if part.negated => r.exclude.push(t),
                Token::Text(t) => r.include.push(t),
                Token::User(name) => {
                    let id: Option<i32> = users::table
                        .filter(lower(users::username).eq(name.to_lowercase()))
                        .select(users::id)
                        .first(conn)
                        .optional()?;
                    match (id, part.negated) {
                        (Some(id), false) => r.users.push((id, reveals(id))),
                        (Some(id), true) => r.not_users.push((id, reveals(id))),
                        (None, false) => r.matches_nothing = true,
                        (None, true) => {}
                    }
                }
                Token::Group(slug) => {
                    let id: Option<i32> = groups::table
                        .filter(lower(groups::slug).eq(slug.to_lowercase()))
                        .select(groups::id)
                        .first(conn)
                        .optional()?;
                    match (id, part.negated) {
                        (Some(id), false) => r.groups.push(id),
                        (Some(id), true) => r.not_groups.push(id),
                        (None, false) => r.matches_nothing = true,
                        (None, true) => {}
                    }
                }
            }
        }
        Ok(r)
    }
}

/// The info hash a whole search term spells, as 40 hex or 32 base32 characters (the two
/// forms magnet links use), as upstream's search jumps straight to that torrent.
pub fn info_hash(term: &str) -> Option<Vec<u8>> {
    let term = term.trim();
    match term.len() {
        40 => hex::decode(term).ok(),
        32 => base32_decode(term),
        _ => None,
    }
}

/// RFC 4648 base32 without padding, either case.
fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let (mut buf, mut bits) = (0u64, 0u32);
    for c in s.chars() {
        let v = match c.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u64 - 'A' as u64,
            c @ '2'..='7' => c as u64 - '2' as u64 + 26,
            _ => return None,
        };
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str, phrase: bool, negated: bool) -> Part {
        Part { token: Token::Text(Text { text: t.into(), phrase }), negated }
    }

    #[test]
    fn parses_words_phrases_and_negations() {
        assert_eq!(
            parse(r#"dragon "show - 01" -720p !"sword tale" Show - 01"#),
            vec![
                text("dragon", false, false),
                text("show - 01", true, false),
                text("720p", false, true),
                text("sword tale", true, true),
                text("Show", false, false),
                text("-", false, false),
                text("01", false, false),
            ]
        );
        // An unclosed quote runs to the end; empty phrases and bare quotes vanish
        assert_eq!(parse(r#"a "b c"#), vec![text("a", false, false), text("b c", true, false)]);
        assert_eq!(parse(r#""" -"" "#), vec![]);
        // Doubled signs are text, so "--" or "!!" searches still work
        assert_eq!(parse("-- !!x"), vec![text("--", false, false), text("!!x", false, false)]);
    }

    #[test]
    fn parses_user_and_group_operators() {
        let p = |token, negated| Part { token, negated };
        assert_eq!(
            parse(r#"user:alice -group:subs G:Other !u:bob group:"a b" user:"c d" dragon"#),
            vec![
                p(Token::User("alice".into()), false),
                p(Token::Group("subs".into()), true),
                p(Token::Group("Other".into()), false),
                p(Token::User("bob".into()), true),
                p(Token::Group("a b".into()), false),
                p(Token::User("c d".into()), false),
                text("dragon", false, false),
            ]
        );
        // Unknown keys, empty values and quoted words with colons stay text
        assert_eq!(
            parse(r#"re:zero user: "user:x""#),
            vec![text("re:zero", false, false), text("user:", false, false), text("user:x", true, false)]
        );
    }

    #[test]
    fn reads_info_hashes() {
        let hex = "0123456789abcdef0123456789ABCDEF01234567";
        assert_eq!(info_hash(hex), hex::decode(hex).ok());
        // The same hash in base32
        let b32 = "AERUKZ4JVPG66AJDIVTYTK6N54ASGRLH";
        assert_eq!(info_hash(b32), hex::decode(hex).ok());
        assert_eq!(info_hash(&b32.to_lowercase()), hex::decode(hex).ok());
        assert_eq!(info_hash("dragon"), None);
        assert_eq!(info_hash("0123456789abcdef0123456789abcdef0123456z"), None);
        assert_eq!(info_hash("AERUKZ4JVPG66AJDIVTYTK6N54ASGRL1"), None);
    }
}
