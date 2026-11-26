# Security review

A full review of the Ondura Server mail stack: authentication, transport
security, the SMTP/IMAP/POP3/EWS/ActiveSync protocols, the webmail front end
and the collaborative-document service. This document records what was checked,
what held up, the issues found, and the hardening still recommended.

Date: 2026-11-26. Reviewed at commit on `main` after the IMAP/POP3 and calendar
work.

## Summary

The server is soundly built. The classic server-side vulnerability classes are
absent:

- **SQL injection** — every query uses bound parameters (`?N`); the only string
  interpolation into SQL is compile-time column/table constants, never request
  data.
- **Command injection** — no request path shells out; `runuser` is invoked with
  an argv (no shell), only during setup/privilege-drop.
- **Path traversal** — attachments and avatars live in the database as BLOBs;
  no request-controlled filesystem paths.
- **`unsafe`** — none in the crate.

The findings below are the concrete issues; the first was fixed in this change,
the rest are smaller or are recommendations.

## Findings

### 1. Header injection via a CRLF display name — FIXED (medium)

`mime::format_person` emitted an ASCII display name verbatim, and its exclusion
set covered quoting characters but not control characters. A name containing
`\r\n` was therefore written straight into a `To:`/`Cc:` header, and the custom
`headers` list and `In-Reply-To` were likewise unsanitised. This is reachable
from attacker-controlled `Person` data — for example an attendee name in an
inbound iMIP `REQUEST` that the server echoes when it generates a `REPLY`, or a
forwarded message — so it is not limited to the authenticated sender.

Fix: `encode_word` now RFC 2047-encodes on any control character (not just
`\r`/`\n`); `format_person` routes control/non-ASCII names through it and strips
CR/LF from the address; and `Outgoing::build` strips CR/LF from custom header
names/values and from `In-Reply-To`. Regression tests
`display_name_cannot_inject_headers` and `build_strips_crlf_from_custom_headers`
cover it.

### 2. IMAP/POP3 passed no client IP to the login throttle — FIXED (low)

The IMAP and POP3 sessions called `auth::login(.., None)`. Failed logins are
throttled per `(login, ip)`, so a `None` IP put every client into one bucket:
a remote attacker could lock a user out of IMAP/POP by failing ten times, and
per-IP granularity was lost. Fixed by threading the real peer IP into both
session handlers.

### 3. HTML e-mail can still load remote content — recommendation (low, privacy)

The reading pane renders HTML mail in an iframe with
`sandbox="allow-same-origin"` and **without** `allow-scripts`, so scripted XSS
from mail content cannot run — this is correct. However, the frame has no
Content-Security-Policy, so remote images and styles load, which allows
tracking pixels. Consider adding a CSP to the `srcdoc` (e.g. default-src 'none';
img-src with a proxy or blocked by default and a "load remote content" button)
to match the privacy behaviour of mainstream clients.

### 4. Outbound delivery uses opportunistic TLS without verification — informational

The outbound SMTP client accepts any certificate (`AcceptAny`). This is the
standard opportunistic-TLS posture for MTA-to-MTA delivery, where most peers
present self-signed certificates. For strict delivery security, enforce DANE
(TLSA) and/or MTA-STS for domains that publish them; this is an enhancement, not
a defect.

## What was verified sound

- **Passwords**: Argon2id hashing; constant-work verification for unknown users
  so accounts cannot be probed by timing; a short-lived in-memory login cache
  keyed by a salted SHA-256 of the password, invalidated on password change.
- **Login throttling**: ten failures per `(login, ip)` per fifteen minutes.
- **Webmail sessions**: 256-bit CSPRNG tokens stored server-side; the cookie is
  `HttpOnly`, `SameSite=Strict` and `Secure` (outside plain-HTTP dev), which
  also defends against CSRF. Login runs Argon2 off the async executor and is
  audit-logged.
- **No open relay**: port 25 accepts mail only for local domains; relaying
  requires authentication, which is offered only on the submission ports, and
  submission rejects a spoofed `From`.
- **Mail authentication**: inbound SPF/DKIM/DMARC verification with optional
  DMARC enforcement; outbound DKIM signing.
- **Transport**: rustls with safe default protocol versions (TLS 1.2/1.3) on
  all listening sockets.
- **Trusted proxies**: `X-Forwarded-For` is honoured only from configured
  trusted proxies (or loopback when behind a proxy), so the client IP used for
  throttling cannot be spoofed by remote clients.
- **Authorization**: admin endpoints require `is_admin`; collaborative documents
  enforce an owner plus per-user shares with an explicit write flag; mailbox
  handlers resolve items by the authenticated user.
- **Resource limits**: per-IP connection limits on the protocol listeners; a
  64 KiB line cap and a 64 MiB literal cap on IMAP; a 1 MiB avatar cap.

## Recommended next steps

1. Add a Content-Security-Policy to the HTML-mail iframe and an opt-in to load
   remote content (finding 3).
2. Implement DANE/MTA-STS enforcement for outbound delivery (finding 4).
3. Consider a global per-IP login-failure cap in addition to the per-login cap,
   to blunt spray attacks across many accounts from one source.
4. Add CSRF double-submit or an `Origin` check to the collab and mail APIs as
   defence in depth behind the existing `SameSite=Strict` cookie.
