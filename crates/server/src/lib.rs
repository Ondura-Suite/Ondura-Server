//! Ondura Server: mail server with Exchange ActiveSync and Exchange Web
//! Services for mail, calendars, contacts and tasks.

pub mod admin;
pub mod app;
pub mod collab;
pub mod auth;
pub mod autodiscover;
pub mod calendar;
pub mod config;
pub mod db;
pub mod directory;
pub mod dns;
pub mod eas;
pub mod ews;
pub mod http;
pub mod ical;
pub mod mime;
pub mod model;
pub mod netlimit;
pub mod setup;
pub mod smtp;
pub mod store;
pub mod tls;
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
    /// Manage mailboxes.
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
    /// Create a mailbox. Without --password a random one is printed.
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
        DbBackend::Postgres => Store::connect_postgres(&cfg.database.url),
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
    let result = run_command(&cfg, command);
    fix_ownership(&cfg);
    result
}

/// Check that the SQLite database and its directory are writable by the current
/// user before serving, turning a later "attempt to write a readonly database"
/// into an actionable error at startup.
fn preflight_storage(cfg: &Config) -> Result<()> {
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
                UserCmd::Add { email, name, password, admin, kind } => {
                    if !matches!(kind.as_str(), "user" | "shared" | "room") {
                        bail!("kind must be user, shared or room");
                    }
                    let generated = password.is_none();
                    let password = password.unwrap_or_else(auth::random_password);
                    let u = s.create_user(&email, &name, &password, &kind)?;
                    if admin {
                        s.update_user(u.id, None, None, Some(true), None)?;
                    }
                    println!("Mailbox {} created.", u.email);
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
    let app = Arc::new(App { cfg: cfg.clone(), store, smtp_tls, https_tls, dns, queue_wake: Notify::new() });
    if app.store.users()?.is_empty() {
        warn!("no mailboxes yet: create one with `ondura-server domain add <domain>` and `ondura-server user add <address> --admin`");
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
    smtp::outbound::spawn(app.clone());
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
