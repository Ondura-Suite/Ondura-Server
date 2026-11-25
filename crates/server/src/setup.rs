//! Provisioning helpers for the `setup-postgres` command: create a local
//! PostgreSQL role and database, then point the configuration at it.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::store::Store;

/// Validate a PostgreSQL identifier that will be interpolated into SQL (role or
/// database name) — these can't be bound as parameters.
fn valid_ident(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The default peer-authentication connection string for a local database.
pub fn default_url(role: &str, dbname: &str) -> String {
    format!("host=/var/run/postgresql user={role} dbname={dbname}")
}

/// Idempotent SQL that creates the role (if missing) and the database (if
/// missing), run as the PostgreSQL superuser.
fn provision_sql(role: &str, dbname: &str) -> String {
    format!(
        "DO $$ BEGIN\n  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{role}') THEN\n    CREATE ROLE {role} LOGIN;\n  END IF;\nEND $$;\n\
         SELECT 'CREATE DATABASE {dbname} OWNER {role}'\n  WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = '{dbname}')\\gexec\n"
    )
}

/// Run `main()`'s `setup-postgres` subcommand.
pub fn setup_postgres(config_path: &Path, role: &str, dbname: &str, url_override: Option<String>, dry_run: bool) -> Result<()> {
    if !valid_ident(role) {
        bail!("invalid role {role:?}: use lowercase letters, digits and underscores (start with a letter)");
    }
    if !valid_ident(dbname) {
        bail!("invalid database name {dbname:?}: use lowercase letters, digits and underscores (start with a letter)");
    }
    let url = url_override.unwrap_or_else(|| default_url(role, dbname));
    let sql = provision_sql(role, dbname);

    if dry_run {
        println!("Would run as the postgres superuser:\n\n{sql}");
        println!("Would set {} to use PostgreSQL:\n  [database]\n  backend = \"postgres\"\n  url = \"{url}\"", config_path.display());
        return Ok(());
    }

    if process_uid() != 0 {
        bail!("setup-postgres must run as root (it uses the postgres superuser and edits the config). Try: sudo ondura-server setup-postgres");
    }

    println!("Creating PostgreSQL role {role:?} and database {dbname:?} ...");
    run_superuser_sql(&sql).context("provision the database (is the 'postgresql' package installed and running?)")?;

    println!("Building the schema ...");
    Store::connect_postgres(&url).context("connect to the new PostgreSQL database")?;

    let text = std::fs::read_to_string(config_path).with_context(|| format!("read {}", config_path.display()))?;
    let updated = rewrite_backend(&text, &url);
    std::fs::write(config_path, updated).with_context(|| format!("write {}", config_path.display()))?;

    println!("Done. {} now uses PostgreSQL.", config_path.display());
    println!("Restart the service to apply it: systemctl restart ondura-server");
    Ok(())
}

fn process_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0)
}

/// Pipe `sql` into `psql` running as the postgres superuser.
fn run_superuser_sql(sql: &str) -> Result<()> {
    let mut child = Command::new("runuser")
        .args(["-u", "postgres", "--", "psql", "-v", "ON_ERROR_STOP=1", "-X", "-q", "-f", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .context("run psql as the postgres user (requires the postgresql package and 'runuser')")?;
    child.stdin.take().expect("piped stdin").write_all(sql.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        bail!("psql exited with status {status}");
    }
    Ok(())
}

/// Return `text` with the `[database]` section set to use PostgreSQL at `url`.
/// Preserves the rest of the file (comments and other sections) untouched.
fn rewrite_backend(text: &str, url: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_db = false;
    let mut wrote_backend = false;
    let mut wrote_url = false;
    let mut db_section_seen = false;

    for line in text.lines() {
        let trimmed = line.trim_start();
        let is_header = trimmed.starts_with('[');
        if is_header {
            // Leaving the [database] section: make sure both keys were written.
            if in_db {
                if !wrote_backend {
                    out.push("backend = \"postgres\"".into());
                }
                if !wrote_url {
                    out.push(format!("url = \"{url}\""));
                }
            }
            in_db = trimmed.starts_with("[database]");
            if in_db {
                db_section_seen = true;
                wrote_backend = false;
                wrote_url = false;
            }
            out.push(line.to_string());
            continue;
        }
        if in_db {
            let key = trimmed.trim_start_matches('#').trim_start();
            if key.starts_with("backend") {
                out.push("backend = \"postgres\"".into());
                wrote_backend = true;
                continue;
            }
            if key.starts_with("url") {
                out.push(format!("url = \"{url}\""));
                wrote_url = true;
                continue;
            }
        }
        out.push(line.to_string());
    }
    // File ended while still in the [database] section.
    if in_db {
        if !wrote_backend {
            out.push("backend = \"postgres\"".into());
        }
        if !wrote_url {
            out.push(format!("url = \"{url}\""));
        }
    }
    // No [database] section at all: append one.
    if !db_section_seen {
        out.push("".into());
        out.push("[database]".into());
        out.push("backend = \"postgres\"".into());
        out.push(format!("url = \"{url}\""));
    }

    let mut joined = out.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_validated() {
        assert!(valid_ident("ondura"));
        assert!(valid_ident("ondura_mail2"));
        assert!(!valid_ident("Ondura")); // uppercase
        assert!(!valid_ident("2db")); // leading digit
        assert!(!valid_ident("ondura; DROP")); // injection attempt
        assert!(!valid_ident(""));
    }

    #[test]
    fn rewrite_switches_the_template() {
        let cfg = "hostname = \"mail.example.com\"\n\n[database]\nbackend = \"sqlite\"\n# url = \"host=/var/run/postgresql user=ondura dbname=ondura\"\n\n[tls]\ncert = \"x\"\n";
        let out = rewrite_backend(cfg, "host=/var/run/postgresql user=ondura dbname=ondura");
        assert!(out.contains("backend = \"postgres\""));
        assert!(out.contains("url = \"host=/var/run/postgresql user=ondura dbname=ondura\""));
        assert!(!out.contains("backend = \"sqlite\""));
        // Other sections survive.
        assert!(out.contains("[tls]"));
        assert!(out.contains("cert = \"x\""));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn rewrite_appends_when_no_section() {
        let out = rewrite_backend("hostname = \"m.example.com\"\n", "host=/x user=ondura dbname=ondura");
        assert!(out.contains("[database]"));
        assert!(out.contains("backend = \"postgres\""));
    }

    #[test]
    fn provision_sql_is_idempotent_shaped() {
        let sql = provision_sql("ondura", "ondura");
        assert!(sql.contains("IF NOT EXISTS"));
        assert!(sql.contains("CREATE ROLE ondura LOGIN"));
        assert!(sql.contains("CREATE DATABASE ondura OWNER ondura"));
    }
}
