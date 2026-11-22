//! Sending a message on behalf of a mailbox (SMTP submission, ActiveSync
//! SendMail, EWS CreateItem with SendAndSaveCopy, server-generated mails).

use anyhow::{Result, bail};
use tracing::info;

use crate::app::Shared;
use crate::mime;
use crate::model::flags;
use crate::store::{NewMail, User, now};

use super::{delivery, dkim};

#[derive(Debug, Default, Clone)]
pub struct Options {
    /// Envelope sender (default: the From address).
    pub envelope_from: Option<String>,
    /// Envelope recipients (default: To, Cc and Bcc of the message).
    pub recipients: Option<Vec<String>>,
    /// Keep a copy in Sent Items.
    pub save_copy: bool,
}

/// Send `raw` as `user`. Local recipients get the message immediately,
/// remote ones through the outbound queue (DKIM-signed).
pub async fn send(app: &Shared, user: &User, raw: Vec<u8>, opts: Options) -> Result<()> {
    let mut raw = mime::crlf(&mime::complete_headers(&raw, app.hostname()));
    let meta = mime::parse_meta(&raw);
    let allowed = app.store.send_as_addresses(user.id)?;
    let from = match &meta.from {
        Some(f) => {
            if !allowed.iter().any(|a| a.eq_ignore_ascii_case(&f.email)) {
                bail!("You are not allowed to send as {}", f.email);
            }
            f.email.to_ascii_lowercase()
        }
        None => {
            raw = mime::prepend_headers(&raw, &[("From".into(), mime::format_person(&user.person()))]);
            user.email.clone()
        }
    };
    let recipients: Vec<String> = match opts.recipients {
        Some(r) => r.into_iter().map(|a| a.to_ascii_lowercase()).collect(),
        None => mime::recipients(&raw),
    };
    if recipients.is_empty() {
        bail!("The message has no recipients");
    }
    if raw.len() > app.cfg.smtp.max_message_size {
        bail!("The message is larger than the allowed {} MB", app.cfg.smtp.max_message_size / 1024 / 1024);
    }
    let envelope_from = opts.envelope_from.unwrap_or_else(|| from.clone());
    let transmit = mime::strip_bcc(&raw);

    let mut remote = Vec::new();
    let mut local_done = std::collections::HashSet::new();
    for r in &recipients {
        let domain = r.rsplit_once('@').map(|(_, d)| d).unwrap_or_default();
        if app.store.is_local_domain(domain) {
            match app.store.resolve_address(r)? {
                Some((uid, _)) => {
                    if local_done.insert(uid) {
                        delivery::deliver(app, uid, &transmit, delivery::Options { junk: false, envelope_from: envelope_from.clone() }).await?;
                    }
                }
                None => {
                    // Unknown local address: bounce right away.
                    super::outbound::bounce(app, &envelope_from, &transmit, &[(r.clone(), "550 5.1.1 Mailbox unavailable".into())]).await;
                }
            }
        } else {
            remote.push(r.clone());
        }
    }
    if !remote.is_empty() {
        let domain = from.rsplit_once('@').map(|(_, d)| d).unwrap_or_default();
        let signed = match dkim::sign(&app.store, domain, &transmit)? {
            Some(header) => {
                let mut v = header.into_bytes();
                v.extend_from_slice(&transmit);
                v
            }
            None => transmit.clone(),
        };
        app.store.enqueue(&envelope_from, &remote, &signed)?;
        app.queue_wake.notify_one();
    }
    if opts.save_copy {
        let sent = app.store.ensure_folder(user.id, "sentitems")?;
        app.store.insert_mail(NewMail { user_id: user.id, folder_id: sent.id, mime: &raw, flags: flags::READ, received: now(), writer: "submit" })?;
    }
    info!(user = %user.email, rcpts = recipients.len(), remote = remote.len(), "message sent");
    Ok(())
}

/// Send a message built by the server (invitations, replies) as `user`.
pub async fn send_generated(app: &Shared, user: &User, out: &mime::Outgoing, save_copy: bool) -> Result<()> {
    let raw = out.build(app.hostname());
    send(app, user, raw, Options { save_copy, ..Default::default() }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_and_remote() {
        let (app, user) = crate::app::App::test();
        let anna = app.store.create_user("anna@example.com", "Anna", "pw", "user").unwrap();
        let raw = b"From: Niklas <niklas@example.com>\r\nTo: anna@example.com, max@other.org\r\nBcc: hidden@other.org\r\nSubject: Test\r\n\r\nHallo".to_vec();
        send(&app, &user, raw, Options { save_copy: true, ..Default::default() }).await.unwrap();
        let inbox = app.store.folder_by_role(anna.id, "inbox").unwrap().unwrap();
        let items = app.store.items(inbox.id, 0, 10).unwrap();
        assert_eq!(items.len(), 1);
        let delivered = app.store.mime(items[0].id).unwrap().unwrap();
        assert!(!String::from_utf8_lossy(&delivered).contains("hidden@"));
        let q = app.store.queue().unwrap();
        assert_eq!(q[0].recipients, vec!["max@other.org".to_string(), "hidden@other.org".to_string()]);
        let sent = app.store.folder_by_role(user.id, "sentitems").unwrap().unwrap();
        assert_eq!(app.store.items(sent.id, 0, 10).unwrap().len(), 1);

        let spoof = b"From: boss@example.com\r\nTo: anna@example.com\r\n\r\nx".to_vec();
        assert!(send(&app, &user, spoof, Options::default()).await.is_err());
    }
}
