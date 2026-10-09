//! Who may do what. [`Permission`] says what each user level grants, the extractors in
//! [`extract`] load the signed-in user and reject requests that lack a permission, and
//! [`policy`] holds the rules that also depend on the object (this user, this torrent).
//!
//! Only `get_current_user` knows about sessions, so the login part stays swappable.

pub mod extract;
pub mod mfa;
pub mod permission;
pub mod policy;

pub use extract::{CurrentUser, LoggedIn, Moderator};
pub use permission::{permission_map, Permission};
