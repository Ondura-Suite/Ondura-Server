//! Delivery into a local mailbox: forwarding, meeting messages, junk
//! placement and automatic replies (out of office).

use anyhow::{Result, anyhow};
use tracing::{debug, warn};

use crate::app::Shared;
use crate::calendar;
use crate::mime::{self, Outgoing};
use crate::model::Person;
use crate::store::{NewMail, now};

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub junk: bool,
    /// Envelope sender ("" for bounces).
    pub envelope_from: String,
}

/// Deliver `raw` into the mailbox `user_id` (boxed: delivery, meeting
/// processing and sending call each other).
pub fn deliver<'a>(app: &'a Shared, user_id: i64, raw: &'a [u8], opts: Options) -> futures::future::BoxFuture<'a, Result<()>> {
    Box::pin(deliver_inner(app, user_id, raw, opts))
}

async fn deliver_inner(app: &Shared, user_id: i64, raw: &[u8], opts: Options) -> Result<()> {
    let user = app.store.user(user_id)?.ok_or_else(|| anyhow!("mailbox {user_id} not found"))?;
    let mut settings = app.store.settings(user_id)?;
    let meta = mime::parse_meta(raw);

    // Forwarding.
    if let Some(target) = settings.forward_to.clone().filter(|t| t.contains('@')) {
        let loop_marker = format!("X-Ondura-Forwarded: {}", user.email);
        if !String::from_utf8_lossy(&raw[..mime::find_header_end(raw)]).contains(&loop_marker) {
            let fwd = mime::prepend_headers(raw, &[("X-Ondura-Forwarded".into(), user.email.clone())]);
            let domain = target.rsplit_once('@').map(|(_, d)| d).unwrap_or_default();
            if app.store.is_local_domain(domain) {
                if let Some((uid, _)) = app.store.resolve_address(&target)?
                    && uid != user_id
                {
                    let app2 = app.clone();
                    let opts2 = opts.clone();
                    tokio::spawn(async move {
                        if let Err(e) = deliver(&app2, uid, &fwd, opts2).await {
                            warn!("forwarding failed: {e:#}");
                        }
                    });
                }
            } else {
                app.store.enqueue(&user.email, std::slice::from_ref(&target), &fwd)?;
                app.queue_wake.notify_one();
            }
            if !settings.keep_forwarded_copy {
                return Ok(());
            }
        }
    }

    // Meeting requests, replies and cancellations update the calendar.
    if let Some(ics) = &meta.calendar
        && !opts.junk
        && let Err(e) = calendar::process_incoming(app, &user, ics, meta.from.as_ref()).await
    {
        warn!(user = %user.email, "meeting message not processed: {e:#}");
    }

    let folder = app.store.ensure_folder(user_id, if opts.junk { "junkemail" } else { "inbox" })?;
    let item_id = app.store.insert_mail(NewMail { user_id, folder_id: folder.id, mime: raw, flags: 0, received: now(), writer: "smtp" })?;
    debug!(user = %user.email, folder = %folder.name, "delivered");

    // Inbox rules run on freshly delivered mail (not junk).
    if !opts.junk
        && let Err(e) = app.store.apply_rules(user_id, item_id)
    {
        warn!(user = %user.email, "inbox rules failed: {e:#}");
    }

    // Out of office.
    if !opts.junk && settings.oof.active(now()) && should_auto_reply(raw, &opts.envelope_from) {
        let sender = opts.envelope_from.to_ascii_lowercase();
        let recently = settings.oof_sent.get(&sender).is_some_and(|t| now() - t < 4 * 86_400);
        let sender_domain = sender.rsplit_once('@').map(|(_, d)| d.to_string()).unwrap_or_default();
        let internal = app.store.is_local_domain(&sender_domain);
        let allowed = internal
            || match settings.oof.external_audience.as_str() {
                "None" => false,
                "Known" => is_known_contact(app, user_id, &sender),
                _ => true,
            };
        let text = if internal { settings.oof.internal_reply.clone() } else { settings.oof.external_reply.clone() };
        if allowed && !recently && !text.trim().is_empty() {
            settings.oof_sent.insert(sender.clone(), now());
            settings.oof_sent.retain(|_, t| now() - *t < 14 * 86_400);
            app.store.set_settings(user_id, &settings)?;
            let is_html = text.contains('<') && text.contains('>');
            let out = Outgoing {
                from: user.person(),
                to: vec![Person::new("", sender.clone())],
                subject: format!("Automatic reply: {}", meta.subject),
                text: if is_html { mime::html_to_text(&text) } else { text.clone() },
                html: Some(if is_html { text } else { mime::text_to_html(&text) }),
                headers: vec![("Auto-Submitted".into(), "auto-replied".into()), ("X-Auto-Response-Suppress".into(), "All".into())],
                in_reply_to: meta.message_id.clone(),
                ..Default::default()
            };
            let app2 = app.clone();
            tokio::spawn(async move {
                if let Err(e) = super::submit::send_generated(&app2, &user, &out, false).await {
                    warn!("automatic reply failed: {e:#}");
                }
            });
        }
    }
    Ok(())
}

/// RFC 3834: no automatic replies to bounces, lists or other automatic mail.
fn should_auto_reply(raw: &[u8], envelope_from: &str) -> bool {
    if envelope_from.is_empty() || !envelope_from.contains('@') {
        return false;
    }
    let local = envelope_from.split('@').next().unwrap_or_default().to_ascii_lowercase();
    if local.starts_with("mailer-daemon") || local.starts_with("postmaster") || local.contains("noreply") || local.contains("no-reply") || local.starts_with("owner-") || local.ends_with("-request") {
        return false;
    }
    let head = String::from_utf8_lossy(&raw[..mime::find_header_end(raw)]).to_ascii_lowercase();
    for line in head.lines() {
        if let Some(v) = line.strip_prefix("auto-submitted:")
            && v.trim() != "no"
        {
            return false;
        }
        if let Some(v) = line.strip_prefix("precedence:")
            && matches!(v.trim(), "bulk" | "list" | "junk")
        {
            return false;
        }
        if line.starts_with("list-id:") || line.starts_with("list-unsubscribe:") || line.starts_with("x-auto-response-suppress: all") || line.starts_with("x-auto-response-suppress: oof") {
            return false;
        }
    }
    true
}

fn is_known_contact(app: &Shared, user_id: i64, address: &str) -> bool {
    let folders: Vec<i64> = app.store.folders(user_id).unwrap_or_default().into_iter().filter(|f| f.class == crate::model::FolderClass::Contacts).map(|f| f.id).collect();
    if folders.is_empty() {
        return false;
    }
    app.store.search(user_id, &folders, address, 1).map(|r| !r.is_empty()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{OofSettings, UserSettings};

    #[tokio::test]
    async fn delivers_and_replies() {
        let (app, user) = crate::app::App::test();
        let settings = UserSettings {
            oof: OofSettings { state: "Enabled".into(), external_reply: "Bin im Urlaub".into(), internal_reply: "Urlaub".into(), external_audience: "All".into(), ..Default::default() },
            ..Default::default()
        };
        app.store.set_settings(user.id, &settings).unwrap();
        let raw = b"From: max@other.org\r\nTo: niklas@example.com\r\nSubject: Frage\r\nMessage-ID: <q@other.org>\r\n\r\nHallo";
        deliver(&app, user.id, raw, Options { junk: false, envelope_from: "max@other.org".into() }).await.unwrap();
        let inbox = app.store.folder_by_role(user.id, "inbox").unwrap().unwrap();
        assert_eq!(app.store.items(inbox.id, 0, 10).unwrap().len(), 1);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let q = app.store.queue().unwrap();
        assert_eq!(q.len(), 1);
        assert!(String::from_utf8_lossy(&q[0].mime).contains("Automatic reply: Frage"));
        // Second mail from the same sender: no second reply.
        deliver(&app, user.id, raw, Options { junk: false, envelope_from: "max@other.org".into() }).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(app.store.queue().unwrap().len(), 1);
        assert!(!should_auto_reply(b"Precedence: bulk\r\n\r\n", "a@b.de"));
        assert!(!should_auto_reply(b"\r\n", ""));
    }
}
