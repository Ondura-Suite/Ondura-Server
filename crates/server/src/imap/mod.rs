//! IMAP4rev1 server over the shared mailbox store, so standard mail clients
//! (Thunderbird, Apple Mail, K-9, mutt) can use an Ondura account.
//!
//! The implementation targets broad client compatibility rather than every
//! optional extension: CAPABILITY, STARTTLS, LOGIN/AUTHENTICATE PLAIN, LIST,
//! LSUB, SELECT/EXAMINE, STATUS, CREATE/DELETE/RENAME, SUBSCRIBE, APPEND,
//! CHECK/NOOP, FETCH (+UID), STORE (+UID), SEARCH (+UID), COPY/MOVE (+UID),
//! EXPUNGE, CLOSE, IDLE and LOGOUT.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine as _;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::app::Shared;
use crate::auth;
use crate::model::{FolderClass, flags};
use crate::store::{Folder, Item, NewMail, now};

pub mod pop3;
mod tests;

const MAX_LINE: usize = 64 * 1024;
const MAX_LITERAL: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------- TLS-able stream

enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
    Gone,
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Gone => Poll::Ready(Err(io::ErrorKind::NotConnected.into())),
        }
    }
}
impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Gone => Poll::Ready(Err(io::ErrorKind::NotConnected.into())),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s).poll_flush(cx),
            Stream::Gone => Poll::Ready(Ok(())),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Gone => Poll::Ready(Ok(())),
        }
    }
}

struct Conn {
    stream: Stream,
    rbuf: Vec<u8>,
    rpos: usize,
}

impl Conn {
    fn new(stream: Stream) -> Conn {
        Conn { stream, rbuf: Vec::with_capacity(8192), rpos: 0 }
    }
    fn is_tls(&self) -> bool {
        matches!(self.stream, Stream::Tls(_))
    }
    async fn fill(&mut self) -> io::Result<usize> {
        let mut tmp = [0u8; 8192];
        let n = self.stream.read(&mut tmp).await?;
        if n > 0 {
            self.rbuf.extend_from_slice(&tmp[..n]);
        }
        Ok(n)
    }
    fn compact(&mut self) {
        if self.rpos > 8192 {
            self.rbuf.drain(..self.rpos);
            self.rpos = 0;
        }
    }
    async fn read_line(&mut self) -> io::Result<Option<String>> {
        loop {
            if let Some(off) = self.rbuf[self.rpos..].iter().position(|&b| b == b'\n') {
                let end = self.rpos + off;
                let mut line = self.rbuf[self.rpos..end].to_vec();
                self.rpos = end + 1;
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.compact();
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            if self.rbuf.len() - self.rpos > MAX_LINE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
            }
            if self.fill().await? == 0 {
                return Ok(None);
            }
        }
    }
    async fn read_n(&mut self, n: usize) -> io::Result<Vec<u8>> {
        while self.rbuf.len() - self.rpos < n {
            if self.fill().await? == 0 {
                break;
            }
        }
        let take = (self.rbuf.len() - self.rpos).min(n);
        let data = self.rbuf[self.rpos..self.rpos + take].to_vec();
        self.rpos += take;
        self.compact();
        Ok(data)
    }
    async fn write(&mut self, s: &[u8]) -> io::Result<()> {
        self.stream.write_all(s).await?;
        self.stream.flush().await
    }
    async fn start_tls(&mut self, acceptor: TlsAcceptor) -> io::Result<()> {
        let stream = std::mem::replace(&mut self.stream, Stream::Gone);
        let Stream::Plain(tcp) = stream else {
            self.stream = stream;
            return Ok(());
        };
        let tls = acceptor.accept(tcp).await?;
        self.stream = Stream::Tls(Box::new(tls));
        self.rbuf.clear();
        self.rpos = 0;
        Ok(())
    }
}

// ---------------------------------------------------------------- tokenizer

#[derive(Debug, Clone)]
enum Tok {
    Atom(String),
    Str(String),
    Literal(Vec<u8>),
    LParen,
    RParen,
}

impl Tok {
    fn text(&self) -> String {
        match self {
            Tok::Atom(s) | Tok::Str(s) => s.clone(),
            Tok::Literal(b) => String::from_utf8_lossy(b).into_owned(),
            Tok::LParen => "(".into(),
            Tok::RParen => ")".into(),
        }
    }
}

/// Tokenize one command line. Returns the tokens and, if the line ends in a
/// literal introducer `{n}` / `{n+}`, the (length, synchronizing?) of it.
fn tokenize_line(line: &str, out: &mut Vec<Tok>) -> Option<(usize, bool)> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' => i += 1,
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b'"' => {
                let mut s = String::new();
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' && i + 1 < b.len() {
                        i += 1;
                    }
                    s.push(b[i] as char);
                    i += 1;
                }
                i += 1; // closing quote
                out.push(Tok::Str(s));
            }
            b'{' => {
                // Literal introducer, must be at end of line.
                let mut j = i + 1;
                let mut n = 0usize;
                while j < b.len() && b[j].is_ascii_digit() {
                    n = n * 10 + (b[j] - b'0') as usize;
                    j += 1;
                }
                let sync = !(j < b.len() && b[j] == b'+');
                return Some((n, sync));
            }
            _ => {
                let start = i;
                while i < b.len() && !matches!(b[i], b' ' | b'\t' | b'(' | b')') {
                    i += 1;
                }
                out.push(Tok::Atom(line[start..i].to_string()));
            }
        }
    }
    None
}

/// Read a full command (tag + tokens), expanding literals.
async fn read_command(conn: &mut Conn) -> io::Result<Option<(String, Vec<Tok>)>> {
    let Some(mut line) = conn.read_line().await? else { return Ok(None) };
    let mut toks = Vec::new();
    while let Some((n, sync)) = tokenize_line(&line, &mut toks) {
        if n > MAX_LITERAL {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "literal too large"));
        }
        if sync {
            conn.write(b"+ OK\r\n").await?;
        }
        let data = conn.read_n(n).await?;
        toks.push(Tok::Literal(data));
        let Some(next) = conn.read_line().await? else { return Ok(None) };
        line = next;
    }
    if toks.is_empty() {
        // Blank line; treat as an empty command with no tag.
        return Ok(Some((String::new(), Vec::new())));
    }
    let tag = toks.remove(0).text();
    Ok(Some((tag, toks)))
}

// ---------------------------------------------------------------- server

/// Run an IMAP listener. `implicit_tls` is true for port 993 (IMAPS).
pub async fn serve(app: Shared, addr: SocketAddr, implicit_tls: bool) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await.map_err(|e| anyhow::anyhow!("bind IMAP {addr}: {e}"))?;
    info!(%addr, tls = implicit_tls, "IMAP listening");
    let limiter = crate::netlimit::ConnLimiter::new(512, 32);
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("IMAP accept: {e}");
                continue;
            }
        };
        let Some(guard) = limiter.admit(peer.ip()) else { continue };
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = session(app, tcp, implicit_tls, peer.ip()).await {
                debug!(%peer, "IMAP session ended: {e}");
            }
            drop(guard);
        });
    }
}

struct Mailbox {
    folder: Folder,
    /// Messages in UID order: (uid, item id, flags).
    msgs: Vec<(i64, i64, i64)>,
    read_only: bool,
}

struct Session {
    app: Shared,
    user: Option<crate::store::User>,
    mailbox: Option<Mailbox>,
    /// Message ids marked \Deleted this session.
    deleted: std::collections::HashSet<i64>,
    ip: std::net::IpAddr,
}

async fn session(app: Shared, tcp: TcpStream, implicit_tls: bool, ip: std::net::IpAddr) -> io::Result<()> {
    let mut conn = Conn::new(Stream::Plain(tcp));
    if implicit_tls {
        let Some(tls) = app.imap_tls() else { return Ok(()) };
        conn.start_tls(TlsAcceptor::from(tls)).await?;
    }
    let mut s = Session { app, user: None, mailbox: None, deleted: std::collections::HashSet::new(), ip };
    conn.write(format!("* OK [CAPABILITY {}] Ondura IMAP ready\r\n", s.capability(&conn)).as_bytes()).await?;
    loop {
        let Some((tag, args)) = read_command(&mut conn).await? else { return Ok(()) };
        if tag.is_empty() {
            continue;
        }
        let cmd = args.first().map(|t| t.text().to_ascii_uppercase()).unwrap_or_default();
        let rest = &args[args.len().min(1)..];
        match cmd.as_str() {
            "LOGOUT" => {
                conn.write(b"* BYE Ondura logging out\r\n").await?;
                conn.write(format!("{tag} OK LOGOUT completed\r\n").as_bytes()).await?;
                return Ok(());
            }
            "CAPABILITY" => {
                conn.write(format!("* CAPABILITY {}\r\n{tag} OK CAPABILITY completed\r\n", s.capability(&conn)).as_bytes()).await?;
            }
            "NOOP" | "CHECK" => {
                conn.write(format!("{tag} OK {cmd} completed\r\n").as_bytes()).await?;
            }
            "STARTTLS" => {
                if conn.is_tls() {
                    conn.write(format!("{tag} NO already using TLS\r\n").as_bytes()).await?;
                } else if let Some(tls) = s.app.imap_tls() {
                    conn.write(format!("{tag} OK begin TLS negotiation\r\n").as_bytes()).await?;
                    conn.start_tls(TlsAcceptor::from(tls)).await?;
                } else {
                    conn.write(format!("{tag} NO TLS not available\r\n").as_bytes()).await?;
                }
            }
            "LOGIN" => s.cmd_login(&mut conn, &tag, rest).await?,
            "AUTHENTICATE" => s.cmd_authenticate(&mut conn, &tag, rest).await?,
            _ if s.user.is_none() => {
                conn.write(format!("{tag} NO Please authenticate first\r\n").as_bytes()).await?;
            }
            "LIST" | "LSUB" => s.cmd_list(&mut conn, &tag, &cmd, rest).await?,
            "NAMESPACE" => {
                conn.write(format!("* NAMESPACE ((\"\" \"/\")) NIL NIL\r\n{tag} OK NAMESPACE completed\r\n").as_bytes()).await?;
            }
            "SELECT" | "EXAMINE" => s.cmd_select(&mut conn, &tag, &cmd, rest).await?,
            "STATUS" => s.cmd_status(&mut conn, &tag, rest).await?,
            "CREATE" => s.cmd_create(&mut conn, &tag, rest).await?,
            "DELETE" => s.cmd_delete(&mut conn, &tag, rest).await?,
            "RENAME" => s.cmd_rename(&mut conn, &tag, rest).await?,
            "SUBSCRIBE" | "UNSUBSCRIBE" => {
                conn.write(format!("{tag} OK {cmd} completed\r\n").as_bytes()).await?;
            }
            "APPEND" => s.cmd_append(&mut conn, &tag, rest).await?,
            "FETCH" => s.cmd_fetch(&mut conn, &tag, rest, false).await?,
            "STORE" => s.cmd_store(&mut conn, &tag, rest, false).await?,
            "SEARCH" => s.cmd_search(&mut conn, &tag, rest, false).await?,
            "COPY" => s.cmd_copy(&mut conn, &tag, rest, false).await?,
            "MOVE" => s.cmd_move(&mut conn, &tag, rest, false).await?,
            "EXPUNGE" => s.cmd_expunge(&mut conn, &tag).await?,
            "CLOSE" => {
                s.expunge(&mut conn, false).await?;
                s.mailbox = None;
                conn.write(format!("{tag} OK CLOSE completed\r\n").as_bytes()).await?;
            }
            "UID" => s.cmd_uid(&mut conn, &tag, rest).await?,
            "IDLE" => s.cmd_idle(&mut conn, &tag).await?,
            _ => {
                conn.write(format!("{tag} BAD Command not recognized\r\n").as_bytes()).await?;
            }
        }
    }
}

impl Session {
    fn capability(&self, conn: &Conn) -> String {
        let mut caps = vec!["IMAP4rev1", "IDLE", "UIDPLUS", "MOVE", "NAMESPACE", "LITERAL+"];
        if conn.is_tls() {
            caps.push("AUTH=PLAIN");
        } else if self.app.imap_tls().is_some() {
            caps.push("STARTTLS");
            caps.push("LOGINDISABLED");
        } else {
            caps.push("AUTH=PLAIN");
        }
        caps.join(" ")
    }

    fn uid(&self) -> i64 {
        self.user.as_ref().map(|u| u.id).unwrap_or(0)
    }

    async fn authed(&mut self, conn: &mut Conn, login: &str, password: &str) -> io::Result<bool> {
        let store = self.app.store.clone();
        let (l, p, ip) = (login.to_string(), password.to_string(), self.ip);
        let res = tokio::task::spawn_blocking(move || auth::login(&store, &l, &p, Some(ip))).await;
        match res {
            // A valid account with no mailbox cannot use IMAP.
            Ok(Ok(user)) if user.mailbox => {
                self.user = Some(user);
                Ok(true)
            }
            _ => {
                // Slow down credential guessing a little.
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = conn;
                Ok(false)
            }
        }
    }

    async fn cmd_login(&mut self, conn: &mut Conn, tag: &str, args: &[Tok]) -> io::Result<()> {
        if !conn.is_tls() && self.app.imap_tls().is_some() {
            conn.write(format!("{tag} NO [PRIVACYREQUIRED] Use STARTTLS first\r\n").as_bytes()).await?;
            return Ok(());
        }
        if args.len() < 2 {
            conn.write(format!("{tag} BAD LOGIN needs a user and password\r\n").as_bytes()).await?;
            return Ok(());
        }
        let user = args[0].text();
        let pass = args[1].text();
        if self.authed(conn, &user, &pass).await? {
            conn.write(format!("{tag} OK [CAPABILITY {}] LOGIN completed\r\n", self.capability(conn)).as_bytes()).await?;
        } else {
            conn.write(format!("{tag} NO LOGIN failed\r\n").as_bytes()).await?;
        }
        Ok(())
    }

    async fn cmd_authenticate(&mut self, conn: &mut Conn, tag: &str, args: &[Tok]) -> io::Result<()> {
        let mech = args.first().map(|t| t.text().to_ascii_uppercase()).unwrap_or_default();
        if mech != "PLAIN" {
            conn.write(format!("{tag} NO Unsupported authentication mechanism\r\n").as_bytes()).await?;
            return Ok(());
        }
        // Initial response may be inline (SASL-IR) or requested with a challenge.
        let b64 = if args.len() >= 2 {
            args[1].text()
        } else {
            conn.write(b"+ \r\n").await?;
            conn.read_line().await?.unwrap_or_default()
        };
        if b64.trim() == "*" {
            conn.write(format!("{tag} BAD authentication cancelled\r\n").as_bytes()).await?;
            return Ok(());
        }
        let decoded = base64::engine::general_purpose::STANDARD.decode(b64.trim()).unwrap_or_default();
        // authzid \0 authcid \0 passwd
        let parts: Vec<&[u8]> = decoded.split(|&b| b == 0).collect();
        if parts.len() == 3 {
            let user = String::from_utf8_lossy(parts[1]).into_owned();
            let pass = String::from_utf8_lossy(parts[2]).into_owned();
            if self.authed(conn, &user, &pass).await? {
                conn.write(format!("{tag} OK [CAPABILITY {}] AUTHENTICATE completed\r\n", self.capability(conn)).as_bytes()).await?;
                return Ok(());
            }
        }
        conn.write(format!("{tag} NO AUTHENTICATE failed\r\n").as_bytes()).await?;
        Ok(())
    }

    // --------------------------------------------------------- folder mapping

    fn folders(&self) -> Vec<Folder> {
        self.app.store.folders(self.uid()).unwrap_or_default().into_iter().filter(|f| f.role.as_deref() != Some("root") && !f.deleted).collect()
    }

    fn mailbox_name(&self, folders: &[Folder], f: &Folder) -> String {
        if f.role.as_deref() == Some("inbox") {
            return "INBOX".to_string();
        }
        let mut parts = vec![f.name.clone()];
        let mut cur = f.parent_id;
        while let Some(pid) = cur {
            match folders.iter().find(|x| x.id == pid) {
                Some(p) if p.role.as_deref() != Some("root") => {
                    parts.push(p.name.clone());
                    cur = p.parent_id;
                }
                _ => break,
            }
        }
        parts.reverse();
        parts.join("/")
    }

    fn resolve(&self, folders: &[Folder], name: &str) -> Option<Folder> {
        if name.eq_ignore_ascii_case("INBOX") {
            return folders.iter().find(|f| f.role.as_deref() == Some("inbox")).cloned();
        }
        folders.iter().find(|f| self.mailbox_name(folders, f).eq_ignore_ascii_case(name)).cloned()
    }

    fn special_use(f: &Folder) -> &'static str {
        match f.role.as_deref() {
            Some("sentitems") => " \\Sent",
            Some("drafts") => " \\Drafts",
            Some("deleteditems") => " \\Trash",
            Some("junkemail") => " \\Junk",
            Some("archive") => " \\Archive",
            _ => "",
        }
    }

    async fn cmd_list(&mut self, conn: &mut Conn, tag: &str, cmd: &str, args: &[Tok]) -> io::Result<()> {
        let folders = self.folders();
        let pattern = args.get(1).map(|t| t.text()).unwrap_or_default();
        for f in &folders {
            let name = self.mailbox_name(&folders, f);
            if list_match(&pattern, &name) {
                conn.write(format!("* {cmd} (\\HasNoChildren{}) \"/\" \"{}\"\r\n", Self::special_use(f), name).as_bytes()).await?;
            }
        }
        conn.write(format!("{tag} OK {cmd} completed\r\n").as_bytes()).await?;
        Ok(())
    }

    fn load_mailbox(&self, folder: Folder, read_only: bool) -> Mailbox {
        let items = self.app.store.mailbox_items(folder.id).unwrap_or_default();
        let msgs = items.iter().filter_map(|it| it.imap_uid.map(|u| (u, it.id, it.flags))).collect();
        Mailbox { folder, msgs, read_only }
    }

    async fn cmd_select(&mut self, conn: &mut Conn, tag: &str, cmd: &str, args: &[Tok]) -> io::Result<()> {
        let folders = self.folders();
        let Some(name) = args.first().map(|t| t.text()) else {
            conn.write(format!("{tag} BAD {cmd} needs a mailbox\r\n").as_bytes()).await?;
            return Ok(());
        };
        let Some(folder) = self.resolve(&folders, &name) else {
            conn.write(format!("{tag} NO [NONEXISTENT] Mailbox does not exist\r\n").as_bytes()).await?;
            return Ok(());
        };
        let read_only = cmd == "EXAMINE";
        let mb = self.load_mailbox(folder.clone(), read_only);
        let exists = mb.msgs.len();
        let unseen = mb.msgs.iter().position(|(_, _, fl)| fl & flags::READ == 0).map(|p| p + 1);
        let uidnext = self.app.store.imap_uidnext(self.uid()).unwrap_or(1);
        let uidvalidity = folder.id as u32;
        self.mailbox = Some(mb);
        conn.write(b"* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n").await?;
        conn.write(format!("* {exists} EXISTS\r\n").as_bytes()).await?;
        conn.write(b"* 0 RECENT\r\n").await?;
        if let Some(u) = unseen {
            conn.write(format!("* OK [UNSEEN {u}] First unseen\r\n").as_bytes()).await?;
        }
        conn.write(format!("* OK [UIDVALIDITY {uidvalidity}] UIDs valid\r\n").as_bytes()).await?;
        conn.write(format!("* OK [UIDNEXT {uidnext}] Predicted next UID\r\n").as_bytes()).await?;
        let perm = if read_only { "* OK [PERMANENTFLAGS ()] Read-only\r\n" } else { "* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)] Limited\r\n" };
        conn.write(perm.as_bytes()).await?;
        let resp = if read_only { "READ-ONLY" } else { "READ-WRITE" };
        conn.write(format!("{tag} OK [{resp}] {cmd} completed\r\n").as_bytes()).await?;
        Ok(())
    }

    async fn cmd_status(&mut self, conn: &mut Conn, tag: &str, args: &[Tok]) -> io::Result<()> {
        let folders = self.folders();
        let Some(name) = args.first().map(|t| t.text()) else {
            conn.write(format!("{tag} BAD STATUS needs a mailbox\r\n").as_bytes()).await?;
            return Ok(());
        };
        let Some(folder) = self.resolve(&folders, &name) else {
            conn.write(format!("{tag} NO Mailbox does not exist\r\n").as_bytes()).await?;
            return Ok(());
        };
        let items = self.app.store.mailbox_items(folder.id).unwrap_or_default();
        let messages = items.len();
        let unseen = items.iter().filter(|it| it.flags & flags::READ == 0).count();
        let uidnext = self.app.store.imap_uidnext(self.uid()).unwrap_or(1);
        let uidvalidity = folder.id as u32;
        let wants: Vec<String> = args.iter().skip(1).map(|t| t.text().to_ascii_uppercase()).collect();
        let mut parts = Vec::new();
        for w in &wants {
            match w.as_str() {
                "MESSAGES" => parts.push(format!("MESSAGES {messages}")),
                "RECENT" => parts.push("RECENT 0".to_string()),
                "UIDNEXT" => parts.push(format!("UIDNEXT {uidnext}")),
                "UIDVALIDITY" => parts.push(format!("UIDVALIDITY {uidvalidity}")),
                "UNSEEN" => parts.push(format!("UNSEEN {unseen}")),
                _ => {}
            }
        }
        conn.write(format!("* STATUS \"{name}\" ({})\r\n{tag} OK STATUS completed\r\n", parts.join(" ")).as_bytes()).await?;
        Ok(())
    }

    async fn cmd_create(&mut self, conn: &mut Conn, tag: &str, args: &[Tok]) -> io::Result<()> {
        let Some(name) = args.first().map(|t| t.text()) else {
            conn.write(format!("{tag} BAD CREATE needs a name\r\n").as_bytes()).await?;
            return Ok(());
        };
        let leaf = name.trim_end_matches('/').rsplit('/').next().unwrap_or(&name).to_string();
        match self.app.store.create_folder(self.uid(), None, &leaf, FolderClass::Mail, "imap") {
            Ok(_) => conn.write(format!("{tag} OK CREATE completed\r\n").as_bytes()).await?,
            Err(_) => conn.write(format!("{tag} NO CREATE failed\r\n").as_bytes()).await?,
        }
        Ok(())
    }

    async fn cmd_delete(&mut self, conn: &mut Conn, tag: &str, args: &[Tok]) -> io::Result<()> {
        let folders = self.folders();
        let Some(folder) = args.first().map(|t| t.text()).and_then(|n| self.resolve(&folders, &n)) else {
            conn.write(format!("{tag} NO Mailbox does not exist\r\n").as_bytes()).await?;
            return Ok(());
        };
        if folder.role.is_some() {
            conn.write(format!("{tag} NO Cannot delete a system mailbox\r\n").as_bytes()).await?;
            return Ok(());
        }
        match self.app.store.delete_folder(folder.id, "imap") {
            Ok(_) => conn.write(format!("{tag} OK DELETE completed\r\n").as_bytes()).await?,
            Err(_) => conn.write(format!("{tag} NO DELETE failed\r\n").as_bytes()).await?,
        }
        Ok(())
    }

    async fn cmd_rename(&mut self, conn: &mut Conn, tag: &str, args: &[Tok]) -> io::Result<()> {
        let folders = self.folders();
        let (Some(from), Some(to)) = (args.first().map(|t| t.text()), args.get(1).map(|t| t.text())) else {
            conn.write(format!("{tag} BAD RENAME needs two names\r\n").as_bytes()).await?;
            return Ok(());
        };
        let Some(folder) = self.resolve(&folders, &from) else {
            conn.write(format!("{tag} NO Mailbox does not exist\r\n").as_bytes()).await?;
            return Ok(());
        };
        let leaf = to.trim_end_matches('/').rsplit('/').next().unwrap_or(&to).to_string();
        match self.app.store.update_folder(folder.id, Some(&leaf), None) {
            Ok(_) => conn.write(format!("{tag} OK RENAME completed\r\n").as_bytes()).await?,
            Err(_) => conn.write(format!("{tag} NO RENAME failed\r\n").as_bytes()).await?,
        }
        Ok(())
    }

    async fn cmd_append(&mut self, conn: &mut Conn, tag: &str, args: &[Tok]) -> io::Result<()> {
        let folders = self.folders();
        let Some(name) = args.first().map(|t| t.text()) else {
            conn.write(format!("{tag} BAD APPEND needs a mailbox\r\n").as_bytes()).await?;
            return Ok(());
        };
        let Some(folder) = self.resolve(&folders, &name) else {
            conn.write(format!("{tag} NO [TRYCREATE] Mailbox does not exist\r\n").as_bytes()).await?;
            return Ok(());
        };
        // The message is the last literal; optional flag list / date precede it.
        let Some(Tok::Literal(raw)) = args.iter().rev().find(|t| matches!(t, Tok::Literal(_))) else {
            conn.write(format!("{tag} BAD APPEND needs a message literal\r\n").as_bytes()).await?;
            return Ok(());
        };
        let mut set = 0i64;
        let mut in_flags = false;
        for t in args {
            match t {
                Tok::LParen => in_flags = true,
                Tok::RParen => in_flags = false,
                Tok::Atom(a) if in_flags => set |= flag_bit(a),
                _ => {}
            }
        }
        match self.app.store.insert_mail(NewMail { user_id: self.uid(), folder_id: folder.id, mime: raw, flags: set, received: now(), writer: "imap" }) {
            Ok(id) => {
                let uid = self.app.store.item(id).ok().flatten().and_then(|i| i.imap_uid).unwrap_or(0);
                conn.write(format!("{tag} OK [APPENDUID {} {uid}] APPEND completed\r\n", folder.id as u32).as_bytes()).await?;
            }
            Err(_) => conn.write(format!("{tag} NO APPEND failed\r\n").as_bytes()).await?,
        }
        Ok(())
    }

    // --------------------------------------------------------- fetch/store/search

    fn require_mailbox(&self) -> Option<&Mailbox> {
        self.mailbox.as_ref()
    }

    /// Resolve a sequence/UID set against the current mailbox to a list of
    /// (seqno, uid, id). `by_uid` selects UID vs sequence numbering.
    fn resolve_set(&self, set: &str, by_uid: bool) -> Vec<(usize, i64, i64)> {
        let Some(mb) = &self.mailbox else { return Vec::new() };
        let n = mb.msgs.len();
        if n == 0 {
            return Vec::new();
        }
        let max_uid = mb.msgs.last().map(|(u, _, _)| *u).unwrap_or(0);
        let mut out = Vec::new();
        for part in set.split(',') {
            let (lo, hi) = match part.split_once(':') {
                Some((a, b)) => (parse_point(a, if by_uid { max_uid } else { n as i64 }), parse_point(b, if by_uid { max_uid } else { n as i64 })),
                None => {
                    let v = parse_point(part, if by_uid { max_uid } else { n as i64 });
                    (v, v)
                }
            };
            let (lo, hi) = (lo.min(hi), lo.max(hi));
            for (idx, (uid, id, _)) in mb.msgs.iter().enumerate() {
                let key = if by_uid { *uid } else { (idx + 1) as i64 };
                if key >= lo && key <= hi {
                    out.push((idx + 1, *uid, *id));
                }
            }
        }
        out.sort_unstable_by_key(|(seq, _, _)| *seq);
        out.dedup_by_key(|(seq, _, _)| *seq);
        out
    }

    async fn cmd_fetch(&mut self, conn: &mut Conn, tag: &str, args: &[Tok], by_uid: bool) -> io::Result<()> {
        if self.require_mailbox().is_none() {
            conn.write(format!("{tag} NO No mailbox selected\r\n").as_bytes()).await?;
            return Ok(());
        }
        let Some(set) = args.first().map(|t| t.text()) else {
            conn.write(format!("{tag} BAD FETCH needs a set\r\n").as_bytes()).await?;
            return Ok(());
        };
        let want = fetch_items(&args[1..]);
        let targets = self.resolve_set(&set, by_uid);
        for (seq, uid, id) in targets {
            if let Some(line) = self.fetch_one(seq, uid, id, &want, by_uid) {
                conn.write(line.as_bytes()).await?;
            }
        }
        conn.write(format!("{tag} OK FETCH completed\r\n").as_bytes()).await?;
        Ok(())
    }

    fn fetch_one(&self, seq: usize, uid: i64, id: i64, want: &FetchWant, by_uid: bool) -> Option<String> {
        let item = self.app.store.item(id).ok().flatten()?;
        let raw = if want.needs_body { self.app.store.mime(id).ok().flatten().unwrap_or_default() } else { Vec::new() };
        let mut parts: Vec<String> = Vec::new();
        // UID is always returned for UID FETCH.
        if want.uid || by_uid {
            parts.push(format!("UID {uid}"));
        }
        if want.flags {
            parts.push(format!("FLAGS ({})", self.flags_string(id, item.flags)));
        }
        if want.internaldate {
            parts.push(format!("INTERNALDATE \"{}\"", internal_date(item.received)));
        }
        if want.size {
            parts.push(format!("RFC822.SIZE {}", item.size.max(raw.len() as i64)));
        }
        if want.envelope {
            parts.push(format!("ENVELOPE {}", envelope(&item)));
        }
        if want.bodystructure {
            parts.push(format!("BODYSTRUCTURE {}", bodystructure(&raw)));
        }
        for sec in &want.sections {
            let (data, label) = section_data(&raw, sec);
            let shown = apply_partial(&data, sec.partial);
            let off = sec.partial.map(|(o, _)| format!("<{o}>")).unwrap_or_default();
            parts.push(format!("BODY[{label}]{off} {{{}}}\r\n{}", shown.len(), String::from_utf8_lossy(&shown)));
        }
        for r in &want.rfc822 {
            let (data, _) = section_data(&raw, &Section { part: r.clone(), partial: None });
            let key = match r.as_str() {
                "" => "RFC822",
                "HEADER" => "RFC822.HEADER",
                "TEXT" => "RFC822.TEXT",
                _ => "RFC822",
            };
            parts.push(format!("{key} {{{}}}\r\n{}", data.len(), String::from_utf8_lossy(&data)));
        }
        // \Seen is set implicitly by a non-peek BODY[...] fetch.
        if want.sets_seen && !self.mailbox.as_ref().map(|m| m.read_only).unwrap_or(true) && item.flags & flags::READ == 0 {
            let _ = self.app.store.update_flags(id, flags::READ, 0, None, "imap");
            // Reflect the change in the returned FLAGS if present.
        }
        Some(format!("* {seq} FETCH ({})\r\n", parts.join(" ")))
    }

    fn flags_string(&self, id: i64, bits: i64) -> String {
        let mut out = Vec::new();
        if bits & flags::READ != 0 {
            out.push("\\Seen");
        }
        if bits & flags::FLAGGED != 0 {
            out.push("\\Flagged");
        }
        if bits & flags::ANSWERED != 0 {
            out.push("\\Answered");
        }
        if bits & flags::DRAFT != 0 {
            out.push("\\Draft");
        }
        if self.deleted.contains(&id) {
            out.push("\\Deleted");
        }
        out.join(" ")
    }

    async fn cmd_store(&mut self, conn: &mut Conn, tag: &str, args: &[Tok], by_uid: bool) -> io::Result<()> {
        if self.mailbox.as_ref().map(|m| m.read_only).unwrap_or(true) {
            conn.write(format!("{tag} NO Mailbox is read-only\r\n").as_bytes()).await?;
            return Ok(());
        }
        let Some(set) = args.first().map(|t| t.text()) else {
            conn.write(format!("{tag} BAD STORE needs a set\r\n").as_bytes()).await?;
            return Ok(());
        };
        let op = args.get(1).map(|t| t.text().to_ascii_uppercase()).unwrap_or_default();
        let silent = op.ends_with(".SILENT");
        let mode = if op.starts_with('+') { 1 } else if op.starts_with('-') { -1 } else { 0 };
        let mut new_bits = 0i64;
        let mut set_deleted = false;
        let mut in_list = false;
        for t in &args[2..] {
            match t {
                Tok::LParen => in_list = true,
                Tok::RParen => in_list = false,
                Tok::Atom(a) | Tok::Str(a) => {
                    let _ = in_list;
                    if a.eq_ignore_ascii_case("\\Deleted") {
                        set_deleted = true;
                    } else {
                        new_bits |= flag_bit(a);
                    }
                }
                _ => {}
            }
        }
        let targets = self.resolve_set(&set, by_uid);
        for (seq, uid, id) in targets {
            let cur = self.app.store.item(id).ok().flatten().map(|i| i.flags).unwrap_or(0);
            let (set_b, clear_b) = match mode {
                1 => (new_bits, 0),
                -1 => (0, new_bits),
                _ => (new_bits, !new_bits & (flags::READ | flags::FLAGGED | flags::ANSWERED | flags::DRAFT)),
            };
            if set_b != 0 || clear_b != 0 {
                let _ = self.app.store.update_flags(id, set_b, clear_b, None, "imap");
            }
            match mode {
                1 if set_deleted => {
                    self.deleted.insert(id);
                }
                -1 if set_deleted => {
                    self.deleted.remove(&id);
                }
                0 => {
                    if set_deleted {
                        self.deleted.insert(id);
                    } else {
                        self.deleted.remove(&id);
                    }
                }
                _ => {}
            }
            let _ = cur;
            if !silent {
                let bits = self.app.store.item(id).ok().flatten().map(|i| i.flags).unwrap_or(0);
                let uidpart = if by_uid { format!(" UID {uid}") } else { String::new() };
                conn.write(format!("* {seq} FETCH (FLAGS ({}){uidpart})\r\n", self.flags_string(id, bits)).as_bytes()).await?;
            }
        }
        // Refresh the cached flags snapshot.
        self.reload_flags();
        conn.write(format!("{tag} OK STORE completed\r\n").as_bytes()).await?;
        Ok(())
    }

    fn reload_flags(&mut self) {
        if let Some(mb) = &mut self.mailbox {
            for m in &mut mb.msgs {
                if let Ok(Some(it)) = self.app.store.item(m.1) {
                    m.2 = it.flags;
                }
            }
        }
    }

    async fn cmd_search(&mut self, conn: &mut Conn, tag: &str, args: &[Tok], by_uid: bool) -> io::Result<()> {
        let Some(mb) = self.require_mailbox() else {
            conn.write(format!("{tag} NO No mailbox selected\r\n").as_bytes()).await?;
            return Ok(());
        };
        let toks: Vec<String> = args.iter().map(|t| t.text()).collect();
        let mut results = Vec::new();
        for (idx, (uid, id, fl)) in mb.msgs.iter().enumerate() {
            if self.search_match(&toks, idx + 1, *uid, *id, *fl, by_uid) {
                results.push(if by_uid { *uid } else { (idx + 1) as i64 });
            }
        }
        let list = results.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(" ");
        conn.write(format!("* SEARCH {list}\r\n{tag} OK SEARCH completed\r\n").as_bytes()).await?;
        Ok(())
    }

    fn search_match(&self, crit: &[String], seq: usize, uid: i64, id: i64, bits: i64, by_uid: bool) -> bool {
        // Evaluate a flat list of criteria, all ANDed. A small, common subset.
        let mut i = 0;
        let lazy_item = || self.app.store.item(id).ok().flatten();
        while i < crit.len() {
            let k = crit[i].to_ascii_uppercase();
            match k.as_str() {
                "ALL" => {}
                "SEEN" => {
                    if bits & flags::READ == 0 {
                        return false;
                    }
                }
                "UNSEEN" => {
                    if bits & flags::READ != 0 {
                        return false;
                    }
                }
                "FLAGGED" => {
                    if bits & flags::FLAGGED == 0 {
                        return false;
                    }
                }
                "UNFLAGGED" => {
                    if bits & flags::FLAGGED != 0 {
                        return false;
                    }
                }
                "ANSWERED" => {
                    if bits & flags::ANSWERED == 0 {
                        return false;
                    }
                }
                "UNANSWERED" => {
                    if bits & flags::ANSWERED != 0 {
                        return false;
                    }
                }
                "DRAFT" => {
                    if bits & flags::DRAFT == 0 {
                        return false;
                    }
                }
                "DELETED" => {
                    if !self.deleted.contains(&id) {
                        return false;
                    }
                }
                "UNDELETED" => {
                    if self.deleted.contains(&id) {
                        return false;
                    }
                }
                "NEW" | "RECENT" => return false,
                "UID" => {
                    i += 1;
                    if let Some(set) = crit.get(i) {
                        let hit = self.resolve_set(set, true).iter().any(|(_, u, _)| *u == uid);
                        if !hit {
                            return false;
                        }
                    }
                }
                "FROM" | "TO" | "CC" | "SUBJECT" | "BODY" | "TEXT" => {
                    i += 1;
                    let needle = crit.get(i).cloned().unwrap_or_default().to_lowercase();
                    if let Some(it) = lazy_item() {
                        let hay = match k.as_str() {
                            "SUBJECT" => it.subject.to_lowercase(),
                            "FROM" => it.sender.as_ref().map(|p| format!("{} {}", p.name, p.email)).unwrap_or_default().to_lowercase(),
                            "TO" | "CC" => it.recipients.to.iter().chain(it.recipients.cc.iter()).map(|p| format!("{} {}", p.name, p.email)).collect::<Vec<_>>().join(" ").to_lowercase(),
                            _ => String::from_utf8_lossy(&self.app.store.mime(id).ok().flatten().unwrap_or_default()).to_lowercase(),
                        };
                        if !hay.contains(&needle) {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                "LARGER" | "SMALLER" => {
                    i += 1;
                    let n: i64 = crit.get(i).and_then(|v| v.parse().ok()).unwrap_or(0);
                    let size = lazy_item().map(|it| it.size).unwrap_or(0);
                    if (k == "LARGER" && size <= n) || (k == "SMALLER" && size >= n) {
                        return false;
                    }
                }
                "SINCE" | "BEFORE" | "ON" => {
                    i += 1;
                    if let Some(day) = crit.get(i).and_then(|v| parse_imap_date(v)) {
                        let recv = lazy_item().map(|it| it.received).unwrap_or(0);
                        let ok = match k.as_str() {
                            "SINCE" => recv >= day,
                            "BEFORE" => recv < day,
                            _ => recv >= day && recv < day + 86_400,
                        };
                        if !ok {
                            return false;
                        }
                    }
                }
                "NOT" | "OR" => { /* not supported; ignore operand loosely */ }
                other => {
                    // A bare number or set in non-UID search = sequence set.
                    if !by_uid && other.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                        let hit = self.resolve_set(other, false).iter().any(|(s, _, _)| *s == seq);
                        if !hit {
                            return false;
                        }
                    }
                }
            }
            i += 1;
        }
        true
    }

    async fn cmd_copy(&mut self, conn: &mut Conn, tag: &str, args: &[Tok], by_uid: bool) -> io::Result<()> {
        let folders = self.folders();
        let (Some(set), Some(name)) = (args.first().map(|t| t.text()), args.get(1).map(|t| t.text())) else {
            conn.write(format!("{tag} BAD COPY needs a set and mailbox\r\n").as_bytes()).await?;
            return Ok(());
        };
        let Some(target) = self.resolve(&folders, &name) else {
            conn.write(format!("{tag} NO [TRYCREATE] Mailbox does not exist\r\n").as_bytes()).await?;
            return Ok(());
        };
        for (_, _, id) in self.resolve_set(&set, by_uid) {
            let _ = self.app.store.copy_item(id, target.id, "imap");
        }
        conn.write(format!("{tag} OK COPY completed\r\n").as_bytes()).await?;
        Ok(())
    }

    async fn cmd_move(&mut self, conn: &mut Conn, tag: &str, args: &[Tok], by_uid: bool) -> io::Result<()> {
        let folders = self.folders();
        let (Some(set), Some(name)) = (args.first().map(|t| t.text()), args.get(1).map(|t| t.text())) else {
            conn.write(format!("{tag} BAD MOVE needs a set and mailbox\r\n").as_bytes()).await?;
            return Ok(());
        };
        let Some(target) = self.resolve(&folders, &name) else {
            conn.write(format!("{tag} NO [TRYCREATE] Mailbox does not exist\r\n").as_bytes()).await?;
            return Ok(());
        };
        let targets = self.resolve_set(&set, by_uid);
        // Report EXPUNGE for the moved messages, highest sequence first.
        let mut seqs: Vec<usize> = targets.iter().map(|(s, _, _)| *s).collect();
        for (_, _, id) in &targets {
            let _ = self.app.store.move_item(*id, target.id, "imap");
        }
        seqs.sort_unstable_by_key(|s| std::cmp::Reverse(*s));
        for s in seqs {
            conn.write(format!("* {s} EXPUNGE\r\n").as_bytes()).await?;
        }
        self.refresh_mailbox();
        conn.write(format!("{tag} OK MOVE completed\r\n").as_bytes()).await?;
        Ok(())
    }

    fn refresh_mailbox(&mut self) {
        if let Some(mb) = &self.mailbox {
            let folder = mb.folder.clone();
            let read_only = mb.read_only;
            self.mailbox = Some(self.load_mailbox(folder, read_only));
        }
    }

    async fn expunge(&mut self, conn: &mut Conn, report: bool) -> io::Result<()> {
        let Some(mb) = &self.mailbox else { return Ok(()) };
        if mb.read_only {
            return Ok(());
        }
        // Collect (seq, id) of \Deleted messages, highest seq first.
        let mut to_remove: Vec<(usize, i64)> = mb.msgs.iter().enumerate().filter(|(_, (_, id, _))| self.deleted.contains(id)).map(|(idx, (_, id, _))| (idx + 1, *id)).collect();
        to_remove.sort_unstable_by_key(|(seq, _)| std::cmp::Reverse(*seq));
        let trash = self.app.store.folder_by_role(self.uid(), "deleteditems").ok().flatten();
        for (seq, id) in &to_remove {
            // In Trash already (or no Trash): hard-delete; otherwise move to Trash.
            match &trash {
                Some(tr) if mb.folder.id != tr.id => {
                    let _ = self.app.store.move_item(*id, tr.id, "imap");
                }
                _ => {
                    let _ = self.app.store.delete_item(*id, "imap");
                }
            }
            self.deleted.remove(id);
            if report {
                conn.write(format!("* {seq} EXPUNGE\r\n").as_bytes()).await?;
            }
        }
        self.refresh_mailbox();
        Ok(())
    }

    async fn cmd_expunge(&mut self, conn: &mut Conn, tag: &str) -> io::Result<()> {
        if self.mailbox.as_ref().map(|m| m.read_only).unwrap_or(true) {
            conn.write(format!("{tag} NO Mailbox is read-only\r\n").as_bytes()).await?;
            return Ok(());
        }
        self.expunge(conn, true).await?;
        conn.write(format!("{tag} OK EXPUNGE completed\r\n").as_bytes()).await?;
        Ok(())
    }

    async fn cmd_uid(&mut self, conn: &mut Conn, tag: &str, args: &[Tok]) -> io::Result<()> {
        let sub = args.first().map(|t| t.text().to_ascii_uppercase()).unwrap_or_default();
        let rest = &args[args.len().min(1)..];
        match sub.as_str() {
            "FETCH" => self.cmd_fetch(conn, tag, rest, true).await,
            "STORE" => self.cmd_store(conn, tag, rest, true).await,
            "SEARCH" => self.cmd_search(conn, tag, rest, true).await,
            "COPY" => self.cmd_copy(conn, tag, rest, true).await,
            "MOVE" => self.cmd_move(conn, tag, rest, true).await,
            "EXPUNGE" => self.cmd_expunge(conn, tag).await,
            _ => {
                conn.write(format!("{tag} BAD Unknown UID command\r\n").as_bytes()).await?;
                Ok(())
            }
        }
    }

    async fn cmd_idle(&mut self, conn: &mut Conn, tag: &str) -> io::Result<()> {
        conn.write(b"+ idling\r\n").await?;
        let folder_id = self.mailbox.as_ref().map(|m| m.folder.id);
        let mut last_len = self.mailbox.as_ref().map(|m| m.msgs.len()).unwrap_or(0);
        let mut poll = tokio::time::interval(Duration::from_secs(2));
        poll.tick().await;
        loop {
            tokio::select! {
                line = conn.read_line() => {
                    match line? {
                        Some(l) if l.trim().eq_ignore_ascii_case("DONE") => break,
                        Some(_) => {}
                        None => return Ok(()),
                    }
                }
                _ = poll.tick() => {
                    if let Some(fid) = folder_id {
                        let items = self.app.store.mailbox_items(fid).unwrap_or_default();
                        if items.len() != last_len {
                            last_len = items.len();
                            self.refresh_mailbox();
                            conn.write(format!("* {} EXISTS\r\n", last_len).as_bytes()).await?;
                        }
                    }
                }
            }
        }
        conn.write(format!("{tag} OK IDLE terminated\r\n").as_bytes()).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------- helpers

fn flag_bit(name: &str) -> i64 {
    match name.to_ascii_lowercase().as_str() {
        "\\seen" => flags::READ,
        "\\flagged" => flags::FLAGGED,
        "\\answered" => flags::ANSWERED,
        "\\draft" => flags::DRAFT,
        _ => 0,
    }
}

fn parse_point(s: &str, max: i64) -> i64 {
    let s = s.trim();
    if s == "*" {
        max
    } else {
        s.parse().unwrap_or(0)
    }
}

/// IMAP LIST wildcard match: `*` matches anything, `%` matches within a level.
fn list_match(pattern: &str, name: &str) -> bool {
    if pattern.is_empty() {
        return name.is_empty();
    }
    fn m(p: &[u8], n: &[u8]) -> bool {
        if p.is_empty() {
            return n.is_empty();
        }
        match p[0] {
            b'*' => m(&p[1..], n) || (!n.is_empty() && m(p, &n[1..])),
            b'%' => m(&p[1..], n) || (!n.is_empty() && n[0] != b'/' && m(p, &n[1..])),
            c => !n.is_empty() && (n[0] == c || n[0].eq_ignore_ascii_case(&c)) && m(&p[1..], &n[1..]),
        }
    }
    m(pattern.as_bytes(), name.as_bytes())
}

fn internal_date(ts: i64) -> String {
    use chrono::{Datelike, Timelike};
    let dt = chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default();
    const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!(
        "{:02}-{}-{:04} {:02}:{:02}:{:02} +0000",
        dt.day(),
        MON[(dt.month() as usize - 1).min(11)],
        dt.year(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

fn parse_imap_date(s: &str) -> Option<i64> {
    // dd-Mon-yyyy
    let s = s.trim_matches('"');
    let mut it = s.split('-');
    let d: u32 = it.next()?.parse().ok()?;
    let mon = it.next()?;
    let y: i32 = it.next()?.parse().ok()?;
    const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let m = MON.iter().position(|x| x.eq_ignore_ascii_case(mon))? as u32 + 1;
    let date = chrono::NaiveDate::from_ymd_opt(y, m, d)?.and_hms_opt(0, 0, 0)?;
    Some(date.and_utc().timestamp())
}

struct Section {
    part: String,
    partial: Option<(usize, usize)>,
}

#[derive(Default)]
struct FetchWant {
    uid: bool,
    flags: bool,
    internaldate: bool,
    size: bool,
    envelope: bool,
    bodystructure: bool,
    needs_body: bool,
    sets_seen: bool,
    sections: Vec<Section>,
    rfc822: Vec<String>,
}

/// Parse FETCH data items from the token stream (flattened, parentheses ignored).
fn fetch_items(toks: &[Tok]) -> FetchWant {
    let mut w = FetchWant::default();
    let flat: Vec<String> = toks.iter().filter(|t| !matches!(t, Tok::LParen | Tok::RParen)).map(|t| t.text()).collect();
    // Expand macros.
    let mut items: Vec<String> = Vec::new();
    for it in &flat {
        match it.to_ascii_uppercase().as_str() {
            "ALL" => items.extend(["FLAGS", "INTERNALDATE", "RFC822.SIZE", "ENVELOPE"].map(String::from)),
            "FAST" => items.extend(["FLAGS", "INTERNALDATE", "RFC822.SIZE"].map(String::from)),
            "FULL" => items.extend(["FLAGS", "INTERNALDATE", "RFC822.SIZE", "ENVELOPE", "BODY"].map(String::from)),
            _ => items.push(it.clone()),
        }
    }
    for it in items {
        let up = it.to_ascii_uppercase();
        if up == "UID" {
            w.uid = true;
        } else if up == "FLAGS" {
            w.flags = true;
        } else if up == "INTERNALDATE" {
            w.internaldate = true;
        } else if up == "RFC822.SIZE" {
            w.size = true;
        } else if up == "ENVELOPE" {
            w.envelope = true;
        } else if up == "BODYSTRUCTURE" || up == "BODY" {
            w.bodystructure = true;
            w.needs_body = true;
        } else if up == "RFC822" {
            w.rfc822.push(String::new());
            w.needs_body = true;
            w.sets_seen = true;
        } else if up == "RFC822.HEADER" {
            w.rfc822.push("HEADER".into());
            w.needs_body = true;
        } else if up == "RFC822.TEXT" {
            w.rfc822.push("TEXT".into());
            w.needs_body = true;
            w.sets_seen = true;
        } else if up.starts_with("BODY.PEEK[") || up.starts_with("BODY[") {
            let peek = up.starts_with("BODY.PEEK[");
            if !peek {
                w.sets_seen = true;
            }
            // Recover original-case section between [ and ].
            let open = it.find('[').unwrap_or(0);
            let close = it.find(']').unwrap_or(it.len());
            let part = it.get(open + 1..close).unwrap_or("").to_string();
            let partial = it[close.min(it.len())..].strip_prefix(']').and_then(parse_partial);
            w.sections.push(Section { part, partial });
            w.needs_body = true;
        }
    }
    w
}

fn parse_partial(s: &str) -> Option<(usize, usize)> {
    let s = s.strip_prefix('<')?.strip_suffix('>')?;
    let (a, b) = s.split_once('.')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

fn apply_partial(data: &[u8], partial: Option<(usize, usize)>) -> Vec<u8> {
    match partial {
        Some((off, len)) => data.iter().skip(off).take(len).copied().collect(),
        None => data.to_vec(),
    }
}

/// Return the bytes for a BODY[...] section label.
fn section_data(raw: &[u8], sec: &Section) -> (Vec<u8>, String) {
    let label = sec.part.clone();
    let up = sec.part.to_ascii_uppercase();
    let data = if up.is_empty() {
        raw.to_vec()
    } else if up == "HEADER" {
        header_bytes(raw)
    } else if up == "TEXT" {
        text_bytes(raw)
    } else if let Some(fields) = up.strip_prefix("HEADER.FIELDS") {
        let names: Vec<String> = fields.trim().trim_start_matches('(').trim_end_matches(')').split_whitespace().map(|s| s.to_ascii_lowercase()).collect();
        header_fields(raw, &names, false)
    } else if let Some(fields) = up.strip_prefix("HEADER.FIELDS.NOT") {
        let names: Vec<String> = fields.trim().trim_start_matches('(').trim_end_matches(')').split_whitespace().map(|s| s.to_ascii_lowercase()).collect();
        header_fields(raw, &names, true)
    } else {
        raw.to_vec()
    };
    (data, label)
}

fn split_headers(raw: &[u8]) -> (&[u8], &[u8]) {
    // Find the blank line that ends the header block.
    let mut i = 0;
    while i + 1 < raw.len() {
        if raw[i] == b'\n' && (raw[i + 1] == b'\n' || (raw[i + 1] == b'\r' && i + 2 < raw.len() && raw[i + 2] == b'\n')) {
            let body_start = if raw[i + 1] == b'\r' { i + 3 } else { i + 2 };
            return (&raw[..body_start], &raw[body_start..]);
        }
        i += 1;
    }
    (raw, &[])
}

fn header_bytes(raw: &[u8]) -> Vec<u8> {
    split_headers(raw).0.to_vec()
}

fn text_bytes(raw: &[u8]) -> Vec<u8> {
    split_headers(raw).1.to_vec()
}

fn header_fields(raw: &[u8], names: &[String], exclude: bool) -> Vec<u8> {
    let head = split_headers(raw).0;
    let text = String::from_utf8_lossy(head);
    let mut out = String::new();
    let mut keep = false;
    for line in text.split_inclusive('\n') {
        let continuation = line.starts_with(' ') || line.starts_with('\t');
        if !continuation {
            let name = line.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
            let listed = names.iter().any(|n| n == &name);
            keep = if exclude { !listed } else { listed };
        }
        if keep && !line.trim().is_empty() {
            out.push_str(line);
        }
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// IMAP ENVELOPE from a stored item: date, subject, from, sender, reply-to, to,
/// cc, bcc, in-reply-to, message-id.
fn envelope(item: &Item) -> String {
    let nstr = |s: &str| if s.is_empty() { "NIL".to_string() } else { format!("\"{}\"", imap_escape(s)) };
    let addr = |p: &crate::model::Person| {
        let (user, host) = p.email.split_once('@').unwrap_or((p.email.as_str(), ""));
        format!("({} NIL {} {})", nstr(&p.name), nstr(user), nstr(host))
    };
    let addr_list = |ps: &[crate::model::Person]| {
        if ps.is_empty() {
            "NIL".to_string()
        } else {
            format!("({})", ps.iter().map(addr).collect::<Vec<_>>().join(""))
        }
    };
    let from = item.sender.as_slice();
    format!(
        "({} {} {} {} {} {} {} NIL {} {})",
        nstr(&internal_date(item.received)),
        nstr(&item.subject),
        addr_list(from),
        addr_list(from),
        addr_list(from),
        addr_list(&item.recipients.to),
        addr_list(&item.recipients.cc),
        nstr(item.in_reply_to.as_deref().unwrap_or("")),
        nstr(item.message_id.as_deref().unwrap_or("")),
    )
}

fn imap_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A simplified but valid single-part BODYSTRUCTURE. Clients fetch BODY[] for
/// the full message; attachment part structure is not broken out.
fn bodystructure(raw: &[u8]) -> String {
    let body = text_bytes(raw);
    let lines = body.iter().filter(|&&b| b == b'\n').count();
    format!("(\"text\" \"plain\" (\"charset\" \"utf-8\") NIL NIL \"8bit\" {} {})", body.len(), lines)
}
