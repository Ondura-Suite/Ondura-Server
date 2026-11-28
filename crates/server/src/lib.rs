//! Ondura Server: mail server with Exchange ActiveSync and Exchange Web
//! Services for mail, calendars, contacts and tasks.

pub mod activity;
pub mod admin;
pub mod api;
pub mod app;
pub mod collab;
pub mod auth;
pub mod autodiscover;
pub mod calendar;
pub mod cloud;
pub mod library;
pub mod config;
pub mod contacts;
pub mod db;
pub mod directory;
pub mod dns;
pub mod eas;
pub mod fetch;
pub mod ews;
pub mod http;

/// serde `default` helper for boolean fields that default to `true`.
pub(crate) fn serde_true() -> bool {
    true
}
pub mod ical;
pub mod imap;
pub mod mime;
pub mod qr;
pub mod model;
pub mod netlimit;
pub mod nextcloud;
pub mod setup;
pub mod smtp;
pub mod store;
pub mod tls;
pub mod totp;
pub mod tzinfo;
pub mod webmail;
pub mod xml;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use tokio::sync::Notify;
use tracing::{error, info, warn};

use app::App;
use config::Config;
use store::Store;

#[derive(Parser)]
#[command(name = "ondura-server", version, about = "Mail server with Exchange ActiveSync and EWS (mail, calendar, contacts, tasks)")]
struct Cli {
    /// Configuration file.
    #[arg(short, long, global = true, default_value = "/etc/ondura-server/config.toml", env = "ONDURA_CONFIG")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (default).
    Serve,
    /// Write an example configuration file.
    Init {
        #[arg(long)]
        hostname: String,
        #[arg(long)]
        force: bool,
    },
    /// Manage mail domains.
    Domain {
        #[command(subcommand)]
        cmd: DomainCmd,
    },
    /// Manage accounts and mailboxes.
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Manage additional addresses of mailboxes.
    Alias {
        #[command(subcommand)]
        cmd: AliasCmd,
    },
    /// DKIM keys.
    Dkim {
        #[command(subcommand)]
        cmd: DkimCmd,
    },
    /// Provision a local PostgreSQL database and switch the configuration to
    /// it. Run as root after installing the `postgresql` package.
    SetupPostgres {
        /// PostgreSQL role to own the data. The service connects as this role
        /// over the local socket using peer authentication, so it must match
        /// the system user the service runs as. Defaults to "ondura".
        #[arg(long, default_value = "ondura")]
        role: String,
        /// Database name. Defaults to "ondura".
        #[arg(long, default_value = "ondura")]
        dbname: String,
        /// Connection string to store. Defaults to a local peer-auth socket URL
        /// built from --role and --dbname.
        #[arg(long)]
        url: Option<String>,
        /// Print the steps without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// First-run setup: ensure a mail domain exists and create an admin
    /// mailbox. Safe to run again — existing domain and admin are kept.
    Bootstrap {
        /// Mail domain. Defaults to the server's host name with the first
        /// label removed (e.g. mail.example.com -> example.com).
        #[arg(long)]
        domain: Option<String>,
        /// Admin email address. Defaults to admin@<domain>.
        #[arg(long)]
        email: Option<String>,
        /// Admin password. A random one is generated and printed if omitted.
        #[arg(long)]
        password: Option<String>,
    },
    /// Show the DNS records a domain needs.
    Dns { domain: String },
    /// Inspect the outbound queue.
    Queue {
        #[command(subcommand)]
        cmd: QueueCmd,
    },
}

#[derive(Subcommand)]
enum DomainCmd {
    Add { name: String },
    Remove { name: String },
    List,
}

#[derive(Subcommand)]
enum UserCmd {
    /// Create an account. Without --password a random one is printed.
    Add {
        email: String,
        #[arg(long, default_value = "")]
        name: String,
        #[arg(long)]
        password: Option<String>,
        /// May use the Admin Center.
        #[arg(long)]
        admin: bool,
        /// user, shared or room
        #[arg(long, default_value = "user")]
        kind: String,
        /// Create the account without a mailbox (no mail; Files, SharePoint,
        /// Tasks and Notes still work). A mailbox can be added later.
        #[arg(long)]
        no_mailbox: bool,
    },
    Passwd {
        email: String,
        #[arg(long)]
        password: Option<String>,
    },
    Admin {
        email: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Turn an account's mailbox on or off.
    Mailbox {
        email: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Turn off two-step sign-in for an account (lost phone); the person can
    /// then sign in with the password and set it up again.
    TwoStepReset { email: String },
    Delete { email: String },
    List,
}

#[derive(Subcommand)]
enum AliasCmd {
    /// `@example.com` is a catch-all address.
    Add { address: String, mailbox: String },
    Remove { address: String },
    List,
}

#[derive(Subcommand)]
enum DkimCmd {
    Generate { domain: String },
    Show { domain: String },
}

#[derive(Subcommand)]
enum QueueCmd {
    List,
    Retry { id: i64 },
    Delete { id: i64 },
}

fn open_store(cfg: &Config) -> Result<Store> {
    use crate::config::DbBackend;
    match cfg.database.backend {
        DbBackend::Sqlite => Store::open(&cfg.sqlite_path()),
        DbBackend::Postgres => connect_postgres_with_retry(&cfg.database.url),
    }
}

/// Connect to PostgreSQL, retrying for a while if it is not ready yet. On boot
/// the database server may still be starting when Ondura launches, so a single
/// failed connection must not keep the service down.
fn connect_postgres_with_retry(url: &str) -> Result<Store> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut attempt = 0u32;
    loop {
        match Store::connect_postgres(url) {
            Ok(store) => return Ok(store),
            Err(e) => {
                attempt += 1;
                if Instant::now() >= deadline {
                    return Err(e.context("PostgreSQL did not become reachable within 60s"));
                }
                warn!("PostgreSQL not ready yet (attempt {attempt}): {e:#}; retrying in 2s");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

/// Administrative commands usually run as root while the service runs as
/// its own user: hand files created by root over to the owner of the data
/// directory.
fn fix_ownership(cfg: &Config) {
    use std::os::unix::fs::MetadataExt;
    // Only the embedded SQLite database lives in data_dir; PostgreSQL manages
    // its own files.
    if cfg.database.backend != crate::config::DbBackend::Sqlite {
        return;
    }
    let Ok(dir) = std::fs::metadata(&cfg.data_dir) else { return };
    if dir.uid() == 0 {
        return;
    }
    let db = cfg.sqlite_path();
    for suffix in ["", "-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
        if std::fs::metadata(&path).is_ok_and(|m| m.uid() == 0) {
            let _ = std::os::unix::fs::chown(&path, Some(dir.uid()), Some(dir.gid()));
        }
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let filter = tracing_subscriber::EnvFilter::try_from_env("ONDURA_LOG").unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
    let _ = rustls::crypto::ring::default_provider().install_default();

    if let Some(Command::Init { hostname, force }) = &cli.command {
        if cli.config.exists() && !force {
            bail!("{} already exists (use --force to overwrite)", cli.config.display());
        }
        if let Some(dir) = cli.config.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&cli.config, Config::example(hostname))?;
        println!("Wrote {}", cli.config.display());
        return Ok(());
    }
    let cfg = Config::load(&cli.config)?;
    model::set_default_tz(cfg.time_zone.parse().map_err(|_| anyhow!("invalid time zone"))?);

    let command = cli.command.unwrap_or(Command::Serve);
    if let Command::SetupPostgres { role, dbname, url, dry_run } = command {
        return setup::setup_postgres(&cli.config, &role, &dbname, url, dry_run);
    }
    if matches!(command, Command::Serve) {
        // Hand any root-created database files to the data-directory owner (in
        // case the server was ever started by hand as root), then fail early
        // with a clear message if the database still isn't writable — rather
        // than letting the first write log a cryptic "readonly database".
        fix_ownership(&cfg);
        preflight_storage(&cfg)?;
        return tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(serve(cfg));
    }
    // Admin commands run by a person: with a local peer-authenticated
    // PostgreSQL database, connecting only works as the service user, so when
    // run as root re-run the command as that user.
    if let Some(code) = reexec_as_service_user(&cfg)? {
        std::process::exit(code);
    }
    let result = run_command(&cfg, command);
    fix_ownership(&cfg);
    result
}

/// Whether the current process is running as root (real uid 0).
fn process_is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map(|m| m.uid() == 0).unwrap_or(false)
}

/// The `user=` value of a libpq connection string.
fn url_user(url: &str) -> Option<&str> {
    url.split_whitespace().find_map(|kv| kv.strip_prefix("user="))
}

/// True when the URL points at a local Unix-socket PostgreSQL with no password,
/// i.e. a connection that relies on peer authentication (the OS user must match
/// the role).
fn is_local_peer_auth(url: &str) -> bool {
    let socket_host = url.split_whitespace().any(|kv| kv.strip_prefix("host=").is_some_and(|h| h.starts_with('/')));
    let has_password = url.split_whitespace().any(|kv| kv.starts_with("password="));
    socket_host && !has_password
}

/// When an admin command is run as root against a local peer-authenticated
/// PostgreSQL database, re-run the same command as the service user (the `user=`
/// role in the URL, which matches the OS user), so the connection succeeds.
/// Returns the child's exit code when it re-ran, or `None` when no re-exec was
/// needed (so the caller continues normally).
fn reexec_as_service_user(cfg: &Config) -> Result<Option<i32>> {
    if cfg.database.backend != crate::config::DbBackend::Postgres || !process_is_root() || !is_local_peer_auth(&cfg.database.url) {
        return Ok(None);
    }
    let Some(user) = url_user(&cfg.database.url) else { return Ok(None) };
    let exe = std::env::current_exe().context("find own executable")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let status = std::process::Command::new("runuser")
        .arg("-u")
        .arg(user)
        .arg("--")
        .arg(&exe)
        .args(&args)
        .status()
        .with_context(|| format!("re-run the command as user '{user}'. Run it directly as that user instead: sudo -u {user} ondura-server ..."))?;
    Ok(Some(status.code().unwrap_or(1)))
}

/// Derive the mail domain from the server host name: drop the first label when
/// there are three or more (mail.example.com -> example.com), otherwise keep the
/// host name (example.com -> example.com).
fn mail_domain_from_host(hostname: &str) -> String {
    let h = hostname.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = h.split('.').filter(|s| !s.is_empty()).collect();
    if labels.len() >= 3 { labels[1..].join(".") } else { h }
}

/// Check that the SQLite database and its directory are writable by the current
/// user before serving, turning a later "attempt to write a readonly database"
/// into an actionable error at startup.
fn preflight_storage(cfg: &Config) -> Result<()> {
    if cfg.database.backend == crate::config::DbBackend::Postgres {
        // Serving by hand as root can't peer-authenticate as the service role;
        // the service is meant to run under systemd as that user.
        if process_is_root() && is_local_peer_auth(&cfg.database.url) && let Some(user) = url_user(&cfg.database.url) {
            bail!(
                "PostgreSQL uses peer authentication as the '{user}' role, but this process runs as root, so it cannot connect.\n  Start the server through systemd (it runs as '{user}'):\n    systemctl start ondura-server\n  Or, to run it by hand, run it as that user:\n    sudo -u {user} ondura-server serve"
            );
        }
        return Ok(());
    }
    if cfg.database.backend != crate::config::DbBackend::Sqlite {
        return Ok(());
    }
    let dir = &cfg.data_dir;
    // The directory must be writable: SQLite creates -wal/-shm/journal files there.
    let probe = dir.join(".ondura-write-test");
    if let Err(e) = std::fs::write(&probe, b"") {
        bail!(
            "data directory {} is not writable ({e}).\n  The service runs as the 'ondura' user. Fix ownership with:\n    sudo chown -R ondura:ondura {}",
            dir.display(),
            dir.display()
        );
    }
    let _ = std::fs::remove_file(&probe);

    let db = cfg.sqlite_path();
    if db.exists() && std::fs::OpenOptions::new().write(true).open(&db).is_err() {
        bail!(
            "database {} exists but is not writable by this user.\n  This usually means it was created while running as root. Fix ownership with:\n    sudo systemctl stop ondura-server\n    sudo chown -R ondura:ondura {}\n    sudo systemctl start ondura-server",
            db.display(),
            cfg.data_dir.display()
        );
    }
    Ok(())
}

fn run_command(cfg: &Config, command: Command) -> Result<()> {
    let cfg = cfg.clone();
    match command {
        Command::Serve | Command::SetupPostgres { .. } => unreachable!(),
        Command::Init { .. } => unreachable!(),
        Command::Domain { cmd } => {
            let s = open_store(&cfg)?;
            match cmd {
                DomainCmd::Add { name } => {
                    s.add_domain(&name)?;
                    if s.dkim(&name)?.is_none() {
                        smtp::dkim::generate(&s, &store::normalize_domain(&name)?)?;
                    }
                    println!("Domain {name} added. DNS records:\n");
                    print_dns(&s, &cfg, &store::normalize_domain(&name)?);
                }
                DomainCmd::Remove { name } => s.remove_domain(&name)?,
                DomainCmd::List => s.domains()?.iter().for_each(|d| println!("{d}")),
            }
            Ok(())
        }
        Command::User { cmd } => {
            let s = open_store(&cfg)?;
            let find = |email: &str| s.user_by_email(email)?.ok_or_else(|| anyhow!("no mailbox {email}"));
            match cmd {
                UserCmd::Add { email, name, password, admin, kind, no_mailbox } => {
                    if !matches!(kind.as_str(), "user" | "shared" | "room") {
                        bail!("kind must be user, shared or room");
                    }
                    let generated = password.is_none();
                    let password = password.unwrap_or_else(auth::random_password);
                    let u = s.create_user(&email, &name, &password, &kind)?;
                    if admin {
                        s.update_user(u.id, None, None, Some(true), None)?;
                    }
                    if no_mailbox {
                        s.set_mailbox(u.id, false)?;
                    }
                    println!("{} {} created.", if no_mailbox { "Account" } else { "Mailbox" }, u.email);
                    if generated && kind == "user" {
                        println!("Password: {password}");
                    }
                }
                UserCmd::Passwd { email, password } => {
                    let u = find(&email)?;
                    let generated = password.is_none();
                    let password = password.unwrap_or_else(auth::random_password);
                    s.set_password(u.id, &password)?;
                    if generated {
                        println!("New password: {password}");
                    }
                }
                UserCmd::Admin { email, state } => {
                    let u = find(&email)?;
                    s.update_user(u.id, None, None, Some(state == "on"), None)?;
                }
                UserCmd::Mailbox { email, state } => {
                    let u = find(&email)?;
                    s.set_mailbox(u.id, state == "on")?;
                    println!("Mailbox for {} turned {}.", u.email, state);
                }
                UserCmd::TwoStepReset { email } => {
                    let u = find(&email)?;
                    s.totp_disable(u.id)?;
                    println!("Two-step sign-in turned off for {}.", u.email);
                }
                UserCmd::Delete { email } => {
                    let u = find(&email)?;
                    s.delete_user(u.id)?;
                }
                UserCmd::List => {
                    let stats = s.user_stats()?;
                    for u in s.users()? {
                        let (items, bytes) = stats.get(&u.id).copied().unwrap_or((0, 0));
                        println!(
                            "{:<40} {:<6} {:<25} {:>7} items {:>9} KB{}{}",
                            u.email,
                            u.kind,
                            u.name,
                            items,
                            bytes / 1024,
                            if u.is_admin { "  admin" } else { "" },
                            if u.enabled { "" } else { "  disabled" }
                        );
                    }
                }
            }
            Ok(())
        }
        Command::Alias { cmd } => {
            let s = open_store(&cfg)?;
            match cmd {
                AliasCmd::Add { address, mailbox } => {
                    let u = s.user_by_email(&mailbox)?.ok_or_else(|| anyhow!("no mailbox {mailbox}"))?;
                    s.add_alias(&address, u.id)?;
                }
                AliasCmd::Remove { address } => s.remove_alias(&address)?,
                AliasCmd::List => {
                    for (a, id) in s.aliases()? {
                        let target = s.user(id)?.map(|u| u.email).unwrap_or_default();
                        println!("{a:<40} -> {target}");
                    }
                }
            }
            Ok(())
        }
        Command::Dkim { cmd } => {
            let s = open_store(&cfg)?;
            let key = match cmd {
                DkimCmd::Generate { domain } => smtp::dkim::generate(&s, &store::normalize_domain(&domain)?)?,
                DkimCmd::Show { domain } => s.dkim(&domain)?.ok_or_else(|| anyhow!("no DKIM key for {domain}"))?,
            };
            println!("{}._domainkey.{}. TXT \"{}\"", key.selector, key.domain, smtp::dkim::dns_record(&key));
            Ok(())
        }
        Command::Dns { domain } => {
            let s = open_store(&cfg)?;
            print_dns(&s, &cfg, &domain.to_ascii_lowercase());
            Ok(())
        }
        Command::Bootstrap { domain, email, password } => {
            let s = open_store(&cfg)?;
            let domain = domain
                .map(|d| d.trim().to_ascii_lowercase())
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| mail_domain_from_host(&cfg.hostname));
            let norm = store::normalize_domain(&domain)?;

            // Ensure the domain exists (idempotent) with a DKIM key.
            s.add_domain(&domain)?;
            if s.dkim(&domain)?.is_none() {
                smtp::dkim::generate(&s, &norm)?;
            }

            // Ensure an admin mailbox exists.
            let email = email.map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).unwrap_or_else(|| format!("admin@{norm}"));
            if let Some(existing) = s.user_by_email(&email)? {
                println!("Admin mailbox {} already exists; leaving it unchanged.", existing.email);
            } else {
                let generated = password.is_none();
                let password = password.unwrap_or_else(auth::random_password);
                let u = s.create_user(&email, "Administrator", &password, "user")?;
                s.update_user(u.id, None, None, Some(true), None)?;
                println!("Admin mailbox {} created.", u.email);
                if generated {
                    println!("Password: {password}");
                    println!("Write this down now — it is not stored anywhere and not shown again.");
                }
            }
            println!("\nDNS records for {norm}:\n");
            print_dns(&s, &cfg, &norm);
            Ok(())
        }
        Command::Queue { cmd } => {
            let s = open_store(&cfg)?;
            match cmd {
                QueueCmd::List => {
                    for m in s.queue()? {
                        let when = chrono::DateTime::from_timestamp(m.next_attempt, 0).unwrap_or_default();
                        println!("#{:<6} {:<35} -> {}  attempts {}  next {}  {}", m.id, m.sender, m.recipients.join(", "), m.attempts, when.format("%Y-%m-%d %H:%M"), m.last_error);
                    }
                }
                QueueCmd::Retry { id } => s.retry_now(id)?,
                QueueCmd::Delete { id } => s.dequeue(id)?,
            }
            Ok(())
        }
    }
}

fn print_dns(s: &Store, cfg: &Config, domain: &str) {
    for r in dns::records(s, &cfg.hostname, domain) {
        println!("{:<45} {:<6} {}\n    ; {}", r.name, r.kind, r.value, r.purpose);
    }
    println!("\n; {}", dns::host_hint(&cfg.hostname));
}

async fn serve(cfg: Config) -> Result<()> {
    let _ = admin::started();
    let store = open_store(&cfg)?;
    let mut reloaders = Vec::new();
    let (smtp_tls, https_tls) = match (&cfg.tls.cert, &cfg.tls.key) {
        (Some(cert), Some(key)) => {
            // Generate a self-signed certificate if none exists yet, so the
            // server always starts; a real certificate replaces it later.
            tls::ensure_self_signed(cert, key, &cfg.hostname).context("ensure a TLS certificate exists")?;
            let (smtp, r1) = tls::server_config(cert, key, &[]).context("load TLS certificate")?;
            let (https, r2) = tls::server_config(cert, key, &[b"h2", b"http/1.1"])?;
            reloaders.push(r1);
            reloaders.push(r2);
            (Some(smtp), Some(https))
        }
        _ => {
            warn!("no TLS certificate configured: HTTPS, STARTTLS and port 465 are disabled");
            (None, None)
        }
    };
    tls::spawn_reloader(reloaders);
    let dns = mail_auth::MessageAuthenticator::new_system_conf().or_else(|_| mail_auth::MessageAuthenticator::new_cloudflare()).ok();
    if dns.is_none() {
        warn!("no DNS resolver available: SPF/DKIM/DMARC checks and MX delivery will fail");
    }
    let app = Arc::new(App { cfg: cfg.clone(), store, settings: std::sync::RwLock::new(crate::app::SettingsOverrides::default()), smtp_tls, https_tls, dns, queue_wake: Notify::new() });
    app.load_settings();
    if app.store.users()?.is_empty() {
        warn!("no mailboxes yet: run `ondura-server bootstrap` to create your mail domain and an admin mailbox in one step");
    }

    let mut tasks = tokio::task::JoinSet::new();
    for addr in cfg.http.https.clone() {
        tasks.spawn(http::serve_https(app.clone(), addr));
    }
    for addr in cfg.http.http.clone() {
        tasks.spawn(http::serve_http(app.clone(), addr));
    }
    for (list, mode) in [(&cfg.smtp.inbound, smtp::Mode::Inbound), (&cfg.smtp.submission, smtp::Mode::Submission), (&cfg.smtp.submissions, smtp::Mode::Submissions)] {
        for addr in list.clone() {
            if mode == smtp::Mode::Submissions && app.smtp_tls.is_none() {
                continue;
            }
            tasks.spawn(smtp::serve(app.clone(), addr, mode));
        }
    }
    // IMAP (143 STARTTLS, 993 implicit TLS). POP3 listeners are added alongside.
    for addr in cfg.imap.imap.clone() {
        tasks.spawn(imap::serve(app.clone(), addr, false));
    }
    if app.smtp_tls.is_some() {
        for addr in cfg.imap.imaps.clone() {
            tasks.spawn(imap::serve(app.clone(), addr, true));
        }
    }
    for addr in cfg.imap.pop3.clone() {
        tasks.spawn(imap::pop3::serve(app.clone(), addr, false));
    }
    if app.smtp_tls.is_some() {
        for addr in cfg.imap.pop3s.clone() {
            tasks.spawn(imap::pop3::serve(app.clone(), addr, true));
        }
    }
    smtp::outbound::spawn(app.clone());
    let feeds = app.clone();
    tokio::spawn(async move {
        // Subscribed calendars due for a refresh (network fetches, so apart
        // from the jobs above).
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            tick.tick().await;
            webmail::calendar_api::refresh_subscriptions(&feeds).await;
        }
    });
    let later = app.clone();
    tokio::spawn(async move {
        // Messages scheduled with "send later", and snoozed messages coming back.
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            webmail::mail_api::send_due(&later).await;
            webmail::mail_api::wake_snoozed(&later);
        }
    });
    let maintenance = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
        loop {
            tick.tick().await;
            if let Err(e) = maintenance.store.prune(60) {
                warn!("maintenance: {e:#}");
            }
        }
    });
    info!(hostname = %cfg.hostname, version = env!("CARGO_PKG_VERSION"), "Ondura Server started");

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        r = tasks.join_next() => {
            if let Some(Ok(Err(e))) = r {
                error!("{e:#}");
                return Err(e);
            }
        }
        _ = tokio::signal::ctrl_c() => info!("shutting down"),
        _ = term.recv() => info!("shutting down"),
    }
    Ok(())
}

#[cfg(test)]
mod bootstrap_tests {
    use super::{is_local_peer_auth, mail_domain_from_host, url_user};

    #[test]
    fn domain_derived_from_host() {
        assert_eq!(mail_domain_from_host("mail.mirranet.de"), "mirranet.de");
        assert_eq!(mail_domain_from_host("mx1.sub.example.com"), "sub.example.com");
        assert_eq!(mail_domain_from_host("example.com"), "example.com");
        assert_eq!(mail_domain_from_host("MAIL.Example.COM"), "example.com");
    }

    #[test]
    fn peer_auth_detection() {
        assert!(is_local_peer_auth("host=/var/run/postgresql user=ondura dbname=ondura"));
        // A password means password auth, which works for any OS user.
        assert!(!is_local_peer_auth("host=/var/run/postgresql user=ondura password=x dbname=ondura"));
        // A TCP host is not peer auth.
        assert!(!is_local_peer_auth("host=localhost user=ondura dbname=ondura"));
        assert_eq!(url_user("host=/var/run/postgresql user=ondura dbname=ondura"), Some("ondura"));
    }
}
