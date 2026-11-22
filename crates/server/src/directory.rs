//! Global address list: all mailboxes of the server.

use crate::app::Shared;
use crate::store::User;

/// Mailboxes whose name or address contains every word of `query`.
pub fn search(app: &Shared, query: &str, limit: usize) -> Vec<User> {
    let words: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
    let mut out: Vec<User> = app
        .store
        .users()
        .unwrap_or_default()
        .into_iter()
        .filter(|u| u.enabled)
        .filter(|u| {
            let hay = format!("{} {}", u.name.to_lowercase(), u.email);
            !words.is_empty() && words.iter().all(|w| hay.contains(w.as_str()))
        })
        .collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out.truncate(limit);
    out
}

/// Exact match on an address (mailbox or alias).
pub fn resolve(app: &Shared, address: &str) -> Option<User> {
    let (id, _) = app.store.resolve_address(address).ok().flatten()?;
    app.store.user(id).ok().flatten()
}

/// First and last name guessed from a display name.
pub fn split_name(name: &str) -> (String, String) {
    match name.trim().rsplit_once(' ') {
        Some((f, l)) => (f.to_string(), l.to_string()),
        None => (String::new(), name.trim().to_string()),
    }
}
