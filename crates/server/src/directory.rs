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
    out.sort_by_key(|a| a.name.to_lowercase());
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

/// Work details an account shows in the organization directory. Everyone
/// edits their own in Settings; the admin center can edit anyone's.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Profile {
    pub title: String,
    pub department: String,
    pub office: String,
    pub phone: String,
    pub mobile: String,
    /// The manager's e-mail address.
    pub manager: String,
}

impl Profile {
    /// Trim every field and cap its length.
    pub fn cleaned(mut self) -> Profile {
        for f in [&mut self.title, &mut self.department, &mut self.office, &mut self.phone, &mut self.mobile, &mut self.manager] {
            *f = f.trim().chars().take(120).collect();
        }
        self.manager = self.manager.to_lowercase();
        self
    }
}

fn profile_from_row(r: &crate::db::Row) -> anyhow::Result<Profile> {
    Ok(Profile { title: r.get(0)?, department: r.get(1)?, office: r.get(2)?, phone: r.get(3)?, mobile: r.get(4)?, manager: r.get(5)? })
}

impl crate::store::Store {
    pub fn profile(&self, user_id: i64) -> anyhow::Result<Profile> {
        use crate::db::OptionalExtension;
        Ok(self
            .db()
            .query_row("SELECT title, department, office, phone, mobile, manager FROM user_profiles WHERE user_id = ?1", [user_id], profile_from_row)
            .optional()?
            .unwrap_or_default())
    }

    pub fn set_profile(&self, user_id: i64, p: &Profile) -> anyhow::Result<()> {
        self.db().execute(
            "INSERT INTO user_profiles (user_id, title, department, office, phone, mobile, manager) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT (user_id) DO UPDATE SET title = excluded.title, department = excluded.department, office = excluded.office, \
             phone = excluded.phone, mobile = excluded.mobile, manager = excluded.manager",
            crate::db::params![user_id, &p.title, &p.department, &p.office, &p.phone, &p.mobile, &p.manager],
        )?;
        Ok(())
    }

    /// Every stored profile, keyed by account id.
    pub fn profiles(&self) -> anyhow::Result<std::collections::HashMap<i64, Profile>> {
        let db = self.db();
        let st = db.prepare("SELECT title, department, office, phone, mobile, manager, user_id FROM user_profiles")?;
        let rows: Vec<(i64, Profile)> = st.query_map(crate::db::params![], |r| Ok((r.get(6)?, profile_from_row(r)?)))?;
        Ok(rows.into_iter().collect())
    }
}

/// The organization directory: every enabled account with its profile, sorted
/// by name. Rooms and shared mailboxes are included and marked by `kind`.
pub fn listing(app: &Shared) -> Vec<serde_json::Value> {
    let profiles = app.store.profiles().unwrap_or_default();
    let mut users: Vec<User> = app.store.users().unwrap_or_default().into_iter().filter(|u| u.enabled).collect();
    users.sort_by_key(|u| (u.name.to_lowercase(), u.email.clone()));
    users
        .iter()
        .map(|u| {
            let p = profiles.get(&u.id).cloned().unwrap_or_default();
            serde_json::json!({
                "id": u.id, "name": u.name, "email": u.email, "kind": u.kind,
                "avatar": app.store.avatar_updated(u.id).ok().flatten(),
                "title": p.title, "department": p.department, "office": p.office,
                "phone": p.phone, "mobile": p.mobile, "manager": p.manager,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    #[test]
    fn profiles_round_trip_and_fill_the_listing() {
        let (app, user) = App::test();
        assert_eq!(app.store.profile(user.id).unwrap(), Profile::default());
        let p = Profile { title: "  Engineer ".into(), department: "IT".into(), manager: "Boss@Example.com".into(), ..Default::default() }.cleaned();
        app.store.set_profile(user.id, &p).unwrap();
        let again = Profile { phone: "+49 1".into(), ..p.clone() };
        app.store.set_profile(user.id, &again).unwrap();
        assert_eq!(app.store.profile(user.id).unwrap(), again);
        assert_eq!(again.title, "Engineer");
        assert_eq!(again.manager, "boss@example.com");

        let off = app.store.create_user("off@example.com", "Off", "pw", "user").unwrap();
        app.store.update_user(off.id, None, None, None, Some(false)).unwrap();
        let list = listing(&app);
        assert!(list.iter().all(|v| v["email"] != "off@example.com"));
        let me = list.iter().find(|v| v["id"] == user.id).unwrap();
        assert_eq!(me["department"], "IT");
        assert_eq!(me["phone"], "+49 1");
    }
}
