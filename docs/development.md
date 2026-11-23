# Ondura Server – development

## Layout

```
crates/
  wbxml/            WBXML codec with the ActiveSync code pages (MS-ASWBXML)
  server/
    src/
      main.rs       command line, `serve`
      config.rs     config.toml
      app.rs        shared state (store, TLS, DNS)
      store.rs      SQLite: domains, mailboxes, folders, items, sync state, queue
      model.rs      calendar/contact/task/note model, expansion of recurring series
      mime.rs       reading and building MIME messages
      ical.rs       iCalendar (iTIP) reading and writing
      tzinfo.rs     Windows time zone rules from IANA zones (ActiveSync, EWS, VTIMEZONE)
      calendar.rs   meeting requests/replies/cancellations, rooms, free/busy
      smtp/         SMTP server, submission, local delivery, outbound queue, DKIM, rspamd
      http.rs       HTTPS front end and routing
      eas/          Exchange ActiveSync
      ews/          Exchange Web Services
      autodiscover.rs
      admin/        Admin Center (JSON API + single-page UI in index.html)
packaging/          systemd unit, Debian maintainer scripts, certbot hook
scripts/            release and third-party notices
```

## Data model and synchronisation

Everything lives in one SQLite database (WAL mode). Each mailbox has a **modseq** counter; every change to a folder or item stores the next value, and deleted items leave a **tombstone** with the modseq of the deletion. A client's sync state is therefore just a number:

- ActiveSync `SyncKey`s map to modseqs per device and collection (`sync_keys` table). Changes made by a device are tagged with its writer id (`eas:<device>`) and not echoed back to it. Windowed responses cut only between different modseqs, so bulk operations are never split.
- EWS `SyncState` is `S<folder>:<modseq>` for items and `H<modseq>` for the hierarchy.
- ActiveSync Ping, Sync with `Wait` and EWS streaming notifications wait on an in-process broadcast channel that the store fires on every change.

Mail is stored as raw MIME plus index columns (subject, sender, recipients, preview, flags). Calendar items, contacts, tasks and notes are JSON documents of the types in `model.rs`; both protocols convert from and to them. Recurring series are expanded on the server (`Event::occurrences`) in the event's IANA time zone, so local times survive DST changes. Changed and deleted occurrences are exceptions keyed by their original start.

## Protocols

All protocol work follows Microsoft's Open Specifications:

| Area | Specifications |
| --- | --- |
| ActiveSync | MS-ASHTTP, MS-ASCMD, MS-ASWBXML, MS-ASAIRS, MS-ASEMAIL, MS-ASCAL, MS-ASCNTC, MS-ASTASK, MS-ASNOTE, MS-ASPROV, MS-ASDTYPE |
| EWS | MS-OXWSCORE, MS-OXWSFOLD, MS-OXWSSYNC, MS-OXWSMSG, MS-OXWSCONT, MS-OXWSTASK, MS-OXWSATT, MS-OXWSRSLNM, MS-OXWSOOF, MS-OXWSAVLS, MS-OXWSNTIF, MS-OXWSMTGS |
| Autodiscover | MS-OXDISCO, MS-OXDSCLI, MS-ASCMD (Autodiscover) |
| Mail | RFC 5321, 5322, 2045–2049, 3207, 4954, 6376 (DKIM), 7208 (SPF), 7489 (DMARC), 3834, 3464 |
| Calendar | RFC 5545, 5546 (iTIP), 6047 (iMIP) |

The EWS implementation is tested with the exact requests Ondura Mail (Ondura-Core, `crates/mail-ews`) sends, see `src/ews/tests.rs`.

## Server identification

So the Ondura Mail app can tell it is talking to an Ondura Server (rather than
Exchange), the server identifies itself two ways:

- **Response header** — every HTTP response carries
  `X-Ondura-Server: Ondura-Server/<version>` (e.g. `Ondura-Server/0.1.0`). It is
  present on any request, including unauthenticated ones such as the `401` from
  `EWS/Exchange.asmx`, so the app can detect the server on first contact without
  signing in. The value is `http::SERVER_ID`.
- **Discovery document** — `GET /.well-known/ondura-server` (also
  `GET /ondura-server`), unauthenticated, returns JSON:

  ```json
  {
    "product": "Ondura Server",
    "vendor": "Ondura",
    "identifier": "ondura-server",
    "version": "0.1.0",
    "hostname": "mail.example.com",
    "protocols": ["EWS", "ActiveSync", "Autodiscover", "SMTP"],
    "endpoints": {
      "ews": "https://mail.example.com/EWS/Exchange.asmx",
      "activesync": "https://mail.example.com/Microsoft-Server-ActiveSync",
      "autodiscover": "https://mail.example.com/autodiscover/autodiscover.xml",
      "webmail": "https://mail.example.com/mail"
    }
  }
  ```

  The stable field to match on is `identifier: "ondura-server"`. For a quick
  check the `X-Ondura-Server` header is enough; the document is there when the
  app wants the version or the endpoint URLs.

## Building and testing

```sh
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

A local instance without TLS, for clients on the same machine:

```toml
# dev.toml
hostname = "mail.example.com"
data_dir = "./data"
[http]
https = []
http = ["127.0.0.1:8080"]
behind_proxy = true
[smtp]
inbound = ["127.0.0.1:2525"]
submission = ["127.0.0.1:2587"]
submissions = []
verify_incoming = false
```

```sh
cargo run -- -c dev.toml domain add example.com
cargo run -- -c dev.toml user add you@example.com --password secret --admin
ONDURA_LOG=debug cargo run -- -c dev.toml serve
# Admin Center: http://127.0.0.1:8080/admin
# EWS:          http://127.0.0.1:8080/EWS/Exchange.asmx
```

`ONDURA_LOG=debug` logs ActiveSync requests and responses as readable XML.

## Continuous integration

- **GitHub Actions** (`.github/workflows/build.yml`): clippy, tests and the third-party notices check on every push; Debian packages and tarballs for x86_64 and ARM64 (built on Ubuntu 22.04 for a low glibc requirement). Tags `v*` publish a GitHub release.
- **Release** (`.github/workflows/release.yml`): *Actions › Release › Run workflow* with the version (`X.Y.Z`, empty = next patch version) sets the version, commits, tags and starts the build.
- **Jenkins** (`Jenkinsfile`, builds.mirranet.de): the same checks plus `.deb` and tarball as build artifacts. Agent needs Rust ≥ 1.90, `build-essential`, `python3` and `cargo install --locked cargo-deb`; for the optional ARM64 build also `gcc-aarch64-linux-gnu` and `rustup target add aarch64-unknown-linux-gnu`.

After dependency changes, regenerate the license file:

```sh
python3 scripts/third-party-notices.py
```
