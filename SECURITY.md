# Security

This document describes Ondura Server's security model, the hardening that is
in place, and the findings of the internal security audit. It is kept in the
repository so operators can judge the server's posture before deploying it.

## Reporting a vulnerability

Please report suspected vulnerabilities privately to
`niklas.linz@mirranet.de` rather than opening a public issue. Include the
affected version, a description, and a reproduction if possible.

## Security model

Ondura Server is a single multi-tenant daemon. The trust boundaries are:

- **Anonymous network peers** on port 25 (inbound SMTP). These are untrusted.
  They may submit mail for local recipients only; relaying to foreign domains
  is refused.
- **Authenticated mailbox users** over EWS, ActiveSync, Autodiscover and SMTP
  submission (587/465). Authenticated, but each user is confined to their own
  mailbox and to mailboxes explicitly shared with them.
- **Administrators** over the Admin Center (`/admin`). A mailbox flagged
  `is_admin` may manage the whole deployment.

All request bodies — WBXML, SOAP XML, MIME, iCalendar — are parsed from
untrusted input and are treated as hostile. These parsers are exercised by
randomised smoke tests on every CI run and by coverage-guided fuzzers; see
[docs/fuzzing.md](docs/fuzzing.md).

## Hardening in place

- **Memory safety.** Written in Rust; the codebase contains a single `unsafe`
  block. Whole classes of buffer-overflow bugs typical of C mail servers do not
  apply.
- **Transport.** TLS via rustls with safe default protocol versions (TLS 1.2
  and 1.3). The listening certificate is reloaded automatically when it changes
  on disk (certbot-friendly).
- **Passwords.** Hashed with Argon2id. Login has an in-process rate limiter and
  a short-lived verification cache; the SMTP/EWS/EAS password check runs on a
  blocking thread so it cannot stall the async runtime.
- **SMTP.** No open relay (foreign recipients are refused with `554 5.7.1`).
  SPF, DKIM and DMARC are evaluated on inbound mail, with optional rspamd
  scoring. Per-command and TLS-handshake timeouts bound slow-loris behaviour. A
  message size limit and a line-length limit bound memory per connection.
- **Database.** Every query is parameterised; there is no string-built SQL with
  user input, so SQL injection does not apply. WAL mode with `foreign_keys=ON`.
- **Access control.** Mailbox isolation is enforced at the store layer. EWS
  routes call `can_access(user, owner)` before touching an item or folder; every
  ActiveSync item lookup filters on `user_id == ctx.user.id`, so a guessed or
  forged item id belonging to another user is rejected rather than returned
  (no IDOR).
- **Admin Center.** Session cookie is `HttpOnly`, `SameSite=Strict` and
  `Secure` (when served over HTTPS). Mutating requests additionally require the
  `X-Ondura-Admin` header, which blocks cross-site request forgery. All
  server-supplied values are HTML-escaped before insertion into the DOM,
  including attribute contexts, so a mailbox name or address cannot inject
  script. A Content-Security-Policy and the usual security headers are sent. All
  administrative actions are written to an audit log.
- **Process.** Runs as a dedicated unprivileged `ondura` user under a hardened
  systemd unit; binds port 25/587/465 through `CAP_NET_BIND_SERVICE` rather than
  running as root.

## Audit findings

The following were reviewed by reading the relevant code paths rather than from
memory. Severities are the auditor's own estimate.

### Fixed in this release

- **[High] Stack overflow via deeply nested WBXML (DoS).** The ActiveSync WBXML
  decoder recursed per nesting level with no bound; a hostile, deeply nested
  document could overflow the stack and abort the process. *Fix:* a depth limit
  (`MAX_DEPTH = 100`) now rejects over-nested documents with a decode error.
  Regression test added.
- **[High] Quadratic / unbounded allocation in the XML reader (DoS).** The SOAP
  parser used quick-xml 0.38, affected by RUSTSEC-2026-0194 (quadratic
  duplicate-attribute scan) and RUSTSEC-2026-0195 (unbounded namespace
  allocation). *Fix:* upgraded to quick-xml 0.42, which carries the patches. The
  parser itself is iterative, so it is not subject to the stack-overflow class.
  Verified against the real Ondura Mail EWS client (login, folder list, sync).
- **[Medium] No connection admission control (resource exhaustion).** The SMTP
  and HTTP listeners spawned a task per accepted connection with no ceiling, so
  a single host could hold many connections open until the idle timeout. *Fix:* a
  per-listener `ConnLimiter` caps total and per-IP concurrent connections (SMTP
  512/20, HTTP 1024/64). Behind a reverse proxy the per-IP cap is relaxed, since
  all connections then share the proxy's address.

### Accepted risks / documented limitations

- **[Medium] Outbound SMTP does not verify delivery-peer certificates.**
  STARTTLS to foreign MX hosts uses opportunistic TLS without certificate
  verification. This is standard for inter-server SMTP (most MX certificates are
  self-signed) but means delivery TLS can be stripped or MITM'd by an active
  network attacker. DANE/MTA-STS are not yet implemented. Inbound and client
  TLS are unaffected.
- **[Low] Generated-password modulo bias.** `random_password` maps CSPRNG bytes
  into a 56-character alphabet with `%`, giving a small bias toward the first 32
  characters. Entropy remains high (~90 bits over 16 characters); not
  practically exploitable. Candidate for rejection sampling.
- **[Low] DKIM private keys at rest.** Stored base64-encoded in the SQLite
  database, i.e. protected only by file-system permissions on the data
  directory (owned by the `ondura` user). Ensure the data directory is not
  world-readable and is covered by disk encryption if the host is shared.
- **Single SQLite writer.** One connection behind a mutex serialises all
  database access. This is a performance ceiling, not a vulnerability; see the
  performance notes in `README`/`docs`.

### Dependency advisories (`cargo audit`)

- `rsa 0.9` — RUSTSEC-2023-0071 (Marvin timing side-channel). Transitive via
  `mail-auth`, used for DKIM signing. Network timing recovery of a signing key
  is impractical; low real-world risk.
- `zip 8.6` — several advisories, all concerning `ZipArchive::extract` writing
  to disk. Transitive via `mail-auth`; Ondura Server does not extract untrusted
  archives to disk, so these do not apply at runtime.
- `chrono`, `chrono-tz`, `rustls-pemfile` — flagged *unmaintained*, no fixed
  version available. Informational.

Resolving the `rsa`/`zip` advisories requires upgrading to `mail-auth` >= 0.15,
which pulls `mail-parser` 1.0 and raises the minimum Rust toolchain to 1.98.
That MSRV bump is the recommended follow-up once the build toolchain is updated.

## Deployment hardening checklist

- Serve the Admin Center over HTTPS only; never expose port 80 directly except
  as a redirect, or set `behind_proxy = true` when a TLS-terminating proxy sits
  in front.
- Keep the data directory private (`0700`, owned by `ondura`) and back up the
  SQLite database regularly; it holds mail, PIM data and DKIM keys.
- Publish SPF, DKIM and DMARC records for every hosted domain (`ondura-server
  dns`).
- Put the public listeners behind a firewall that permits only the mail and
  HTTPS ports; restrict the Admin Center to trusted networks where possible.
- Run the latest release; subscribe to this repository for security updates.
