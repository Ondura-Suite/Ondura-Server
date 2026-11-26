//! IMAP integration tests driving the session over a real loopback socket.
#![cfg(test)]

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::app::App;
use crate::store::{NewMail, now};

/// Start a session bound to a loopback socket and return the client end.
async fn connect() -> (TcpStream, crate::store::User, crate::app::Shared) {
    let (app, user) = App::test();
    // Seed two messages into the inbox.
    let inbox = app.store.folder_by_role(user.id, "inbox").unwrap().unwrap();
    for n in 0..2 {
        let raw = format!("From: Max <max@other.org>\r\nTo: niklas@example.com\r\nSubject: Angebot {n}\r\nMessage-ID: <m{n}@x>\r\n\r\nHallo Niklas, Angebot Nummer {n}.");
        app.store.insert_mail(NewMail { user_id: user.id, folder_id: inbox.id, mime: raw.as_bytes(), flags: 0, received: now() + n, writer: "smtp" }).unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app2 = app.clone();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let _ = super::session(app2, tcp, false, "127.0.0.1".parse().unwrap()).await;
    });
    let client = TcpStream::connect(addr).await.unwrap();
    (client, user, app)
}

/// Send a line and read until a line beginning with `tag `.
async fn cmd(client: &mut TcpStream, line: &str) -> String {
    client.write_all(line.as_bytes()).await.unwrap();
    client.write_all(b"\r\n").await.unwrap();
    let tag = line.split(' ').next().unwrap().to_string();
    read_until_tag(client, &tag).await
}

async fn read_until_tag(client: &mut TcpStream, tag: &str) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = client.read(&mut tmp).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        let text = String::from_utf8_lossy(&buf);
        if text.lines().any(|l| l.starts_with(&format!("{tag} "))) {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

async fn read_greeting(client: &mut TcpStream) {
    let mut tmp = [0u8; 1024];
    let n = client.read(&mut tmp).await.unwrap();
    let s = String::from_utf8_lossy(&tmp[..n]);
    assert!(s.contains("* OK"), "{s}");
}

#[tokio::test]
async fn login_select_fetch_flow() {
    let (mut c, _user, _app) = connect().await;
    read_greeting(&mut c).await;

    let r = cmd(&mut c, "a1 LOGIN niklas@example.com secret").await;
    assert!(r.contains("a1 OK"), "{r}");

    let r = cmd(&mut c, "a2 LIST \"\" \"*\"").await;
    assert!(r.contains("INBOX"), "{r}");

    let r = cmd(&mut c, "a3 SELECT INBOX").await;
    assert!(r.contains("2 EXISTS"), "{r}");
    assert!(r.contains("[UIDVALIDITY"), "{r}");
    assert!(r.contains("a3 OK"), "{r}");

    // FETCH flags + envelope by sequence.
    let r = cmd(&mut c, "a4 FETCH 1:2 (UID FLAGS ENVELOPE RFC822.SIZE)").await;
    assert!(r.contains("* 1 FETCH"), "{r}");
    assert!(r.contains("* 2 FETCH"), "{r}");
    assert!(r.contains("Angebot 0"), "{r}");
    assert!(r.contains("a4 OK"), "{r}");

    // UID FETCH body, which also marks \Seen.
    let r = cmd(&mut c, "a5 UID FETCH 1 (BODY[])").await;
    assert!(r.contains("Hallo Niklas"), "{r}");
    assert!(r.contains("a5 OK"), "{r}");

    // STORE a flag.
    let r = cmd(&mut c, "a6 STORE 2 +FLAGS (\\Flagged)").await;
    assert!(r.contains("\\Flagged"), "{r}");

    // SEARCH by body text.
    let r = cmd(&mut c, "a7 SEARCH BODY \"Nummer 1\"").await;
    assert!(r.contains("* SEARCH 2"), "{r}");

    // UID SEARCH ALL returns uids.
    let r = cmd(&mut c, "a8 UID SEARCH ALL").await;
    assert!(r.contains("* SEARCH"), "{r}");

    let r = cmd(&mut c, "a9 LOGOUT").await;
    assert!(r.contains("a9 OK") || r.contains("BYE"), "{r}");
}

#[tokio::test]
async fn append_and_expunge() {
    let (mut c, user, app) = connect().await;
    read_greeting(&mut c).await;
    cmd(&mut c, "b1 LOGIN niklas@example.com secret").await;
    cmd(&mut c, "b2 SELECT INBOX").await;

    // APPEND a new message via literal.
    let msg = "From: a@b.com\r\nSubject: Appended\r\n\r\nbody";
    let r = cmd(&mut c, &format!("b3 APPEND INBOX {{{}}}\r\n{msg}", msg.len())).await;
    assert!(r.contains("b3 OK"), "{r}");
    let inbox = app.store.folder_by_role(user.id, "inbox").unwrap().unwrap();
    assert_eq!(app.store.mailbox_items(inbox.id).unwrap().len(), 3);

    // Mark the first message deleted and expunge it (moves to Trash).
    cmd(&mut c, "b4 STORE 1 +FLAGS (\\Deleted)").await;
    let r = cmd(&mut c, "b5 EXPUNGE").await;
    assert!(r.contains("1 EXPUNGE"), "{r}");
    assert_eq!(app.store.mailbox_items(inbox.id).unwrap().len(), 2);
}
