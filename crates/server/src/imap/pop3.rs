//! POP3 server over the shared store, serving the inbox. Reuses the IMAP
//! connection types for TLS and line handling.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use super::{Conn, Stream};
use crate::app::Shared;
use crate::auth;

/// Run a POP3 listener. `implicit_tls` is true for port 995 (POP3S).
pub async fn serve(app: Shared, addr: SocketAddr, implicit_tls: bool) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await.map_err(|e| anyhow::anyhow!("bind POP3 {addr}: {e}"))?;
    info!(%addr, tls = implicit_tls, "POP3 listening");
    let limiter = crate::netlimit::ConnLimiter::new(256, 16);
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("POP3 accept: {e}");
                continue;
            }
        };
        let Some(guard) = limiter.admit(peer.ip()) else { continue };
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = session(app, tcp, implicit_tls).await {
                debug!(%peer, "POP3 session ended: {e}");
            }
            drop(guard);
        });
    }
}

async fn session(app: Shared, tcp: TcpStream, implicit_tls: bool) -> io::Result<()> {
    let mut conn = Conn::new(Stream::Plain(tcp));
    if implicit_tls {
        let Some(tls) = app.imap_tls() else { return Ok(()) };
        conn.start_tls(TlsAcceptor::from(tls)).await?;
    }
    conn.write(b"+OK Ondura POP3 ready\r\n").await?;

    let mut pending_login: Option<String> = None;
    let mut user: Option<crate::store::User> = None;
    // (message number, item id, size) and the numbers marked for deletion.
    let mut msgs: Vec<(usize, i64, i64)> = Vec::new();
    let mut deleted: std::collections::HashSet<usize> = std::collections::HashSet::new();

    loop {
        let Some(line) = conn.read_line().await? else { return Ok(()) };
        let mut it = line.trim_end().splitn(2, ' ');
        let cmd = it.next().unwrap_or("").to_ascii_uppercase();
        let arg = it.next().unwrap_or("").trim().to_string();

        match cmd.as_str() {
            "CAPA" => conn.write(b"+OK\r\nUSER\r\nUIDL\r\nTOP\r\nSTLS\r\n.\r\n").await?,
            "STLS" => {
                if conn.is_tls() {
                    conn.write(b"-ERR already using TLS\r\n").await?;
                } else if let Some(tls) = app.imap_tls() {
                    conn.write(b"+OK begin TLS\r\n").await?;
                    conn.start_tls(TlsAcceptor::from(tls)).await?;
                } else {
                    conn.write(b"-ERR TLS unavailable\r\n").await?;
                }
            }
            "USER" => {
                if !conn.is_tls() && app.imap_tls().is_some() {
                    conn.write(b"-ERR [AUTH] STLS required first\r\n").await?;
                } else {
                    pending_login = Some(arg);
                    conn.write(b"+OK\r\n").await?;
                }
            }
            "PASS" => {
                let Some(login) = pending_login.take() else {
                    conn.write(b"-ERR USER first\r\n").await?;
                    continue;
                };
                let store = app.store.clone();
                let (l, p) = (login, arg.clone());
                let res = tokio::task::spawn_blocking(move || auth::login(&store, &l, &p, None)).await;
                match res {
                    Ok(Ok(u)) => {
                        if let Some(inbox) = app.store.folder_by_role(u.id, "inbox").ok().flatten() {
                            let items = app.store.mailbox_items(inbox.id).unwrap_or_default();
                            msgs = items.iter().enumerate().map(|(i, it)| (i + 1, it.id, it.size.max(0))).collect();
                        }
                        user = Some(u);
                        conn.write(b"+OK mailbox ready\r\n").await?;
                    }
                    _ => {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        conn.write(b"-ERR authentication failed\r\n").await?;
                    }
                }
            }
            "QUIT" => {
                // UPDATE state: apply deletions, then close.
                if user.is_some() {
                    apply_deletions(&app, &msgs, &deleted, user.as_ref().map(|u| u.id).unwrap_or(0));
                }
                conn.write(b"+OK Ondura POP3 signing off\r\n").await?;
                return Ok(());
            }
            _ if user.is_none() => conn.write(b"-ERR authenticate first\r\n").await?,
            "STAT" => {
                let (n, total) = msgs.iter().filter(|(num, _, _)| !deleted.contains(num)).fold((0usize, 0i64), |(n, t), (_, _, s)| (n + 1, t + s));
                conn.write(format!("+OK {n} {total}\r\n").as_bytes()).await?;
            }
            "LIST" | "UIDL" => {
                let uidl = cmd == "UIDL";
                if arg.is_empty() {
                    conn.write(b"+OK\r\n").await?;
                    for (num, id, size) in &msgs {
                        if !deleted.contains(num) {
                            let v = if uidl { *id } else { *size };
                            conn.write(format!("{num} {v}\r\n").as_bytes()).await?;
                        }
                    }
                    conn.write(b".\r\n").await?;
                } else if let Ok(n) = arg.parse::<usize>() {
                    match msgs.iter().find(|(num, _, _)| *num == n) {
                        Some((_, id, size)) if !deleted.contains(&n) => {
                            let v = if uidl { *id } else { *size };
                            conn.write(format!("+OK {n} {v}\r\n").as_bytes()).await?;
                        }
                        _ => conn.write(b"-ERR no such message\r\n").await?,
                    }
                } else {
                    conn.write(b"-ERR bad argument\r\n").await?;
                }
            }
            "RETR" => {
                let found = arg.parse::<usize>().ok().and_then(|n| msgs.iter().find(|(num, _, _)| *num == n && !deleted.contains(num)).map(|(_, id, _)| *id));
                if let Some(id) = found {
                    let raw = app.store.mime(id).ok().flatten().unwrap_or_default();
                    conn.write(format!("+OK {} octets\r\n", raw.len()).as_bytes()).await?;
                    write_message(&mut conn, &raw, None).await?;
                } else {
                    conn.write(b"-ERR no such message\r\n").await?;
                }
            }
            "TOP" => {
                let mut parts = arg.split_whitespace();
                let n = parts.next().and_then(|v| v.parse::<usize>().ok());
                let lines = parts.next().and_then(|v| v.parse::<usize>().ok());
                let found = n.and_then(|n| msgs.iter().find(|(num, _, _)| *num == n && !deleted.contains(num)).map(|(_, id, _)| *id));
                if let (Some(id), Some(lines)) = (found, lines) {
                    let raw = app.store.mime(id).ok().flatten().unwrap_or_default();
                    conn.write(b"+OK\r\n").await?;
                    write_message(&mut conn, &raw, Some(lines)).await?;
                } else {
                    conn.write(b"-ERR no such message\r\n").await?;
                }
            }
            "DELE" => match arg.parse::<usize>() {
                Ok(n) if msgs.iter().any(|(num, _, _)| *num == n) => {
                    deleted.insert(n);
                    conn.write(format!("+OK message {n} deleted\r\n").as_bytes()).await?;
                }
                _ => conn.write(b"-ERR no such message\r\n").await?,
            },
            "RSET" => {
                deleted.clear();
                conn.write(b"+OK\r\n").await?;
            }
            "NOOP" => conn.write(b"+OK\r\n").await?,
            _ => conn.write(b"-ERR unknown command\r\n").await?,
        }
    }
}

/// Write a message body (optionally only its headers plus `top_lines` body
/// lines), CRLF-normalized and dot-stuffed, terminated by the `.` line.
async fn write_message(conn: &mut Conn, raw: &[u8], top_lines: Option<usize>) -> io::Result<()> {
    let text = String::from_utf8_lossy(raw);
    let mut out = String::new();
    let mut in_body = false;
    let mut body_count = 0usize;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if !in_body && line.is_empty() {
            in_body = true;
            out.push_str("\r\n");
            continue;
        }
        if in_body {
            if top_lines.is_some_and(|limit| body_count >= limit) {
                break;
            }
            body_count += 1;
        }
        // Dot-stuffing: a line starting with '.' gets an extra '.'.
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    conn.write(out.as_bytes()).await
}

/// Move deleted messages to Deleted Items (or remove them if already there).
fn apply_deletions(app: &Shared, msgs: &[(usize, i64, i64)], deleted: &std::collections::HashSet<usize>, user_id: i64) {
    if deleted.is_empty() {
        return;
    }
    let trash = app.store.folder_by_role(user_id, "deleteditems").ok().flatten();
    let inbox_is_trash = trash.as_ref().map(|t| Some(t.id) == app.store.folder_by_role(user_id, "inbox").ok().flatten().map(|f| f.id)).unwrap_or(false);
    for (num, id, _) in msgs {
        if !deleted.contains(num) {
            continue;
        }
        match &trash {
            Some(tr) if !inbox_is_trash => {
                let _ = app.store.move_item(*id, tr.id, "pop3");
            }
            _ => {
                let _ = app.store.delete_item(*id, "pop3");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::store::{NewMail, now};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn line(c: &mut TcpStream) -> String {
        let mut tmp = [0u8; 4096];
        let n = c.read(&mut tmp).await.unwrap();
        String::from_utf8_lossy(&tmp[..n]).into_owned()
    }
    async fn multiline(c: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = c.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if String::from_utf8_lossy(&buf).contains("\r\n.\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[tokio::test]
    async fn pop3_retrieve_and_delete() {
        let (app, user) = App::test();
        let inbox = app.store.folder_by_role(user.id, "inbox").unwrap().unwrap();
        for n in 0..2 {
            let raw = format!("From: a@b.com\r\nSubject: Post {n}\r\n\r\nBody line {n}.\r\n.hidden dot line");
            app.store.insert_mail(NewMail { user_id: user.id, folder_id: inbox.id, mime: raw.as_bytes(), flags: 0, received: now() + n, writer: "smtp" }).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app2 = app.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let a = app2.clone();
                tokio::spawn(async move {
                    let _ = session(a, tcp, false).await;
                });
            }
        });

        let mut c = TcpStream::connect(addr).await.unwrap();
        assert!(line(&mut c).await.contains("+OK"));
        c.write_all(b"USER niklas@example.com\r\n").await.unwrap();
        assert!(line(&mut c).await.contains("+OK"));
        c.write_all(b"PASS secret\r\n").await.unwrap();
        assert!(line(&mut c).await.contains("+OK"));
        c.write_all(b"STAT\r\n").await.unwrap();
        assert!(line(&mut c).await.contains("+OK 2"));
        c.write_all(b"RETR 1\r\n").await.unwrap();
        let body = multiline(&mut c).await;
        assert!(body.contains("Body line 0"), "{body}");
        assert!(body.contains("..hidden dot line"), "dot-stuffing: {body}");
        c.write_all(b"DELE 1\r\n").await.unwrap();
        assert!(line(&mut c).await.contains("+OK"));
        c.write_all(b"QUIT\r\n").await.unwrap();
        assert!(line(&mut c).await.contains("+OK"));

        // The inbox now has one message (the other moved to Deleted Items).
        assert_eq!(app.store.mailbox_items(inbox.id).unwrap().len(), 1);
    }
}
