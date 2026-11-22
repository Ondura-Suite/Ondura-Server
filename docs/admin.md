# Ondura Server – admin guide

For administrators who run Ondura Server for a company, a club, a family or themselves. End users only need their e-mail address and password.

Contents:

1. [Requirements](#requirements)
2. [Installation](#installation)
3. [Configuration](#configuration)
4. [TLS certificate](#tls-certificate)
5. [Domains and DNS](#domains-and-dns)
6. [Mailboxes](#mailboxes)
7. [Admin Center](#admin-center)
8. [Command line](#command-line)
9. [Clients](#clients)
10. [Mail flow](#mail-flow)
11. [Mobile devices](#mobile-devices)
12. [Spam filtering](#spam-filtering)
13. [Behind a reverse proxy](#behind-a-reverse-proxy)
14. [Backup and restore](#backup-and-restore)
15. [Updates](#updates)
16. [Troubleshooting](#troubleshooting)
17. [Protocol support](#protocol-support)
18. [Legal notes](#legal-notes)

## Requirements

- A Linux server with a public IPv4 (and ideally IPv6) address – Debian 12+, Ubuntu 22.04+ or any distribution for the tarball. One CPU core and 1 GB RAM are enough for dozens of mailboxes.
- A host name for the server, e.g. `mail.example.com`, with A/AAAA records pointing to it and **reverse DNS (PTR)** of the server's addresses set to the same name. Many receiving servers reject mail from addresses without matching reverse DNS; most hosting providers let you set it in their console.
- Outgoing connections on port 25. Some providers block it for new accounts until you ask; without it, set a relay (see [Mail flow](#mail-flow)).
- Free ports **25** (SMTP), **465** and **587** (submission), **443** (HTTPS) and **80** (redirect, certificate renewal). Stop other mail servers first, e.g. `systemctl disable --now postfix` or `exim4`.

## Installation

### Debian and Ubuntu

```sh
sudo apt install ./ondura-server_<version>-1_amd64.deb
```

The package

- installs `/usr/bin/ondura-server` and the systemd service `ondura-server` (enabled, not started),
- creates the system user `ondura`, the data folder `/var/lib/ondura-server` and `/etc/ondura-server/config.toml` with the machine's host name,
- ships `/usr/share/ondura-server/certbot-deploy-hook.sh` for Let's Encrypt.

The service binds the privileged ports through `CAP_NET_BIND_SERVICE` and runs as `ondura` with a read-only system. Start it after the configuration:

```sh
sudo systemctl start ondura-server
sudo journalctl -u ondura-server -f
```

`apt purge ondura-server` removes the configuration; mailbox data in `/var/lib/ondura-server` is always kept.

### Other distributions

Unpack the tarball, copy `ondura-server` to `/usr/local/bin`, then create the user, folders and configuration like the Debian package does:

```sh
sudo useradd --system --home /var/lib/ondura-server ondura
sudo install -d -o ondura -g ondura -m 0750 /var/lib/ondura-server
sudo install -d -o root -g ondura -m 0750 /etc/ondura-server /etc/ondura-server/tls
sudo ondura-server --config /etc/ondura-server/config.toml init --hostname mail.example.com
sudo cp packaging/ondura-server.service /etc/systemd/system/
sudo systemctl enable ondura-server
```

### Container

```sh
docker build -t ondura-server .
docker run -d --name ondura --restart unless-stopped \
  -p 25:25 -p 80:80 -p 443:443 -p 465:465 -p 587:587 \
  -v ondura-data:/var/lib/ondura-server \
  -v /etc/ondura-server:/etc/ondura-server:ro \
  ondura-server
docker exec -it ondura ondura-server --config /etc/ondura-server/config.toml user list
```

## Configuration

`/etc/ondura-server/config.toml` (all keys except `hostname` are optional):

```toml
# Public name of the server: MX target, Autodiscover host, TLS certificate name.
hostname = "mail.example.com"
data_dir = "/var/lib/ondura-server"
# Default time zone for calendars (IANA name).
time_zone = "Europe/Berlin"

[tls]
cert = "/etc/ondura-server/tls/fullchain.pem"
key = "/etc/ondura-server/tls/privkey.pem"

[http]
https = ["[::]:443"]
http = ["[::]:80"]           # redirects to HTTPS unless behind_proxy = true
behind_proxy = false
trusted_proxies = []         # proxies whose X-Forwarded-For is trusted
admin_center = true          # serve the Admin Center at /admin

[smtp]
inbound = ["[::]:25"]        # mail from other servers
submission = ["[::]:587"]    # mail programs, STARTTLS + login
submissions = ["[::]:465"]   # mail programs, TLS + login
max_message_size = 52428800  # bytes
verify_incoming = true       # SPF, DKIM and DMARC checks
enforce_dmarc = true         # reject mail failing DMARC with p=reject
queue_lifetime_days = 5      # then undeliverable mail bounces
# relay = "smtp.provider.example:587"
# relay_username = "user"
# relay_password = "secret"

[spam]
# rspamd = "http://127.0.0.1:11333"
# junk_score = 6.0
# reject_score = 15.0

[activesync]
provisioning = true          # devices must accept the security policy
require_password = false     # require a device PIN or password
min_password_length = 4
max_heartbeat = 1680         # seconds a device may wait for push
```

`[::]` listens on IPv6 and, on Linux, also on IPv4. Restart the service after changes: `systemctl restart ondura-server`.

Logs go to the journal. `ONDURA_LOG=debug` in the service environment (`systemctl edit ondura-server`) shows every request, including decoded ActiveSync traffic.

## TLS certificate

All client protocols need a certificate for the host name. With Let's Encrypt:

```sh
sudo apt install certbot
sudo systemctl stop ondura-server          # certbot needs port 80 once
sudo certbot certonly --standalone -d mail.example.com
sudo ln -s /usr/share/ondura-server/certbot-deploy-hook.sh /etc/letsencrypt/renewal-hooks/deploy/ondura-server
sudo RENEWED_LINEAGE=/etc/letsencrypt/live/mail.example.com /usr/share/ondura-server/certbot-deploy-hook.sh
sudo systemctl start ondura-server
```

The hook copies the certificate to `/etc/ondura-server/tls` (readable by the `ondura` group). Ondura Server checks the files every five minutes and uses renewed certificates without a restart. For renewals while the server runs, use the DNS challenge of your DNS provider's certbot plugin, or let a reverse proxy own port 80.

Without a certificate, HTTPS, STARTTLS and port 465 stay off and a warning is logged; only plain SMTP on port 25 works.

## Domains and DNS

Add every domain the server receives mail for:

```sh
sudo ondura-server domain add example.com
```

This also creates the DKIM key and prints the DNS records (the Admin Center shows them under *Domains* with copy buttons):

| Name | Type | Value | Purpose |
| --- | --- | --- | --- |
| `example.com.` | MX | `10 mail.example.com.` | Mail for the domain goes to this server |
| `example.com.` | TXT | `v=spf1 mx a:mail.example.com ~all` | SPF |
| `ondura202611._domainkey.example.com.` | TXT | `v=DKIM1; k=rsa; p=…` | DKIM signature key |
| `_dmarc.example.com.` | TXT | `v=DMARC1; p=quarantine; rua=mailto:postmaster@example.com` | DMARC policy |
| `autodiscover.example.com.` | CNAME | `mail.example.com.` | Autodiscover |
| `_autodiscover._tcp.example.com.` | SRV | `0 0 443 mail.example.com.` | Autodiscover (alternative) |
| `_submission._tcp.example.com.` | SRV | `0 1 587 mail.example.com.` | Mail submission for other clients |

Notes:

- If a hosting provider still sends mail for the domain (newsletters, web forms), add it to the SPF record, e.g. `v=spf1 mx a:mail.example.com include:_spf.provider.example ~all`.
- With the `autodiscover` CNAME the certificate must also cover `autodiscover.example.com` (`certbot … -d mail.example.com -d autodiscover.example.com`). The SRV record works without that.
- Start DMARC with `p=none` if you are not sure all senders of the domain sign correctly, and tighten it later.
- Long DKIM records: most DNS providers split them automatically. At Hetzner DNS, enter the value as one string.

## Mailboxes

There are three kinds of mailbox:

| Kind | Use |
| --- | --- |
| `user` | A person: signs in with address and password. |
| `shared` | A team address (info@, support@): no own login, opened by users with *full access*. |
| `room` | A meeting room or resource: no login, books itself – it accepts invitations when free and declines conflicts. |

Further addresses go to the same mailbox as **aliases**; `@example.com` as alias catches every address of the domain that does not exist. Plus addressing (`name+anything@example.com`) always works.

**Permissions**: *full access* lets a user open a shared or another person's mailbox (Ondura Mail and other EWS clients open it by its address). *Send as* lets them send with that address.

**Quota**: optional per mailbox in the Admin Center; 0 means unlimited.

## Admin Center

`https://mail.example.com/admin` – sign in with a mailbox that has administrator rights (`--admin` when created, or the checkbox in the Admin Center). It has the same functions as the command line plus:

- overview of storage, devices, the queue and the connection details for clients,
- mailbox details with folders and sizes, forwarding, automatic replies status and devices,
- device actions (block, remote wipe),
- an audit log of all changes.

Sessions last eight hours. Sign-ins are rate limited per user name and address. To turn the Admin Center off, set `admin_center = false`.

## Command line

Run as root (or as `ondura`); files created by root are handed over to the service user automatically.

```sh
ondura-server domain add|remove|list [example.com]
ondura-server user add you@example.com --name "Your Name" [--password …] [--admin] [--kind user|shared|room]
ondura-server user passwd you@example.com [--password …]     # without --password: random, printed
ondura-server user admin you@example.com on|off
ondura-server user delete you@example.com
ondura-server user list
ondura-server alias add info@example.com you@example.com
ondura-server alias remove info@example.com
ondura-server alias list
ondura-server dkim generate|show example.com
ondura-server dns example.com                               # DNS records again
ondura-server queue list | retry <id> | delete <id>
```

The configuration file defaults to `/etc/ondura-server/config.toml` (`--config` or `ONDURA_CONFIG` for another one).

## Clients

With the Autodiscover record in place, clients only need the e-mail address and password.

| Client | Setup |
| --- | --- |
| Browser (no install) | Open `https://mail.example.com/mail` and sign in – see the [webmail guide](webmail.md) |
| Ondura Mail | Add an Exchange Server (on-premises) account. EWS URL if asked: `https://mail.example.com/EWS/Exchange.asmx` |
| iPhone / iPad | *Settings › Mail › Accounts › Add Account › Microsoft Exchange*. Server if asked: `mail.example.com` |
| Android (Gmail app, Samsung Email) | *Add account › Exchange*. Server: `mail.example.com` |
| Outlook for iOS / Android | Add the address, choose *Exchange* if asked. |
| Thunderbird, Evolution and other EWS clients | Exchange/EWS account with `https://mail.example.com/EWS/Exchange.asmx` |
| Other mail programs | Sending via SMTP `mail.example.com`, port 587 (STARTTLS) or 465 (SSL/TLS), login with address and password |

User names: the e-mail address, an alias, or the part before `@` when it is unique on the server. `DOMAIN\name` is accepted as well.

Classic Outlook for Windows (MAPI) is not supported; use Ondura Mail on Windows.

## Mail flow

**Incoming**: other servers deliver to port 25. Ondura Server accepts mail only for its domains, checks SPF, DKIM and DMARC (result in the `Authentication-Results` header), rejects mail that fails DMARC with `p=reject` and puts `p=quarantine` failures and spam into the junk folder.

**Outgoing**: messages to other domains are signed with the domain's DKIM key and delivered via the recipient's MX hosts, with opportunistic TLS. Failed attempts are retried after 5, 10 and 30 minutes, then every few hours; after `queue_lifetime_days` the sender gets a non-delivery report. The queue is visible in the Admin Center and with `ondura-server queue list`.

**Relay**: if port 25 is blocked or you want a provider to send for you, set `smtp.relay` (STARTTLS on port 587, or implicit TLS on port 465) and optionally `relay_username`/`relay_password`.

**Automatic replies** are sent at most once every four days per sender, never to mailing lists, bounces or other automatic messages.

## Mobile devices

Devices that sync with ActiveSync appear in the Admin Center under *Mobile devices* and in each mailbox. With `provisioning = true`, a device must accept the server's security policy before it syncs; `require_password = true` makes the policy demand a device PIN of `min_password_length`.

- **Block** stops a device from syncing until unblocked.
- **Remote wipe** resets the device to factory settings the next time it connects – use it for lost phones. The status changes to *Wiped* when the device confirms.
- **Remove** forgets the device; it sets up its sync again on the next connection.

## Spam filtering

Ondura Server can ask [rspamd](https://rspamd.com) about every incoming message:

```sh
sudo apt install rspamd
```

```toml
[spam]
rspamd = "http://127.0.0.1:11333"
```

rspamd's action decides by default (*reject* rejects, *add header*/*rewrite subject* go to junk); `junk_score` and `reject_score` set your own thresholds. Spam score and flag are added as `X-Spam-Score`/`X-Spam-Flag` headers.

## Behind a reverse proxy

If nginx, Caddy or Traefik already serve port 443, let them forward to Ondura Server's plain HTTP listener:

```toml
[http]
https = []
http = ["127.0.0.1:8080"]
behind_proxy = true
trusted_proxies = ["127.0.0.1"]
```

nginx example:

```nginx
server {
    listen 443 ssl http2;
    server_name mail.example.com autodiscover.example.com;
    # ssl_certificate …
    client_max_body_size 110m;
    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto https;
        proxy_read_timeout 1900s;      # ActiveSync push and EWS streaming
        proxy_buffering off;
    }
}
```

SMTP is not proxied: keep `[tls]` configured so STARTTLS and port 465 work.

## Backup and restore

All data is in one SQLite database (`/var/lib/ondura-server/ondura.db` plus `-wal`/`-shm` files). Consistent backups while the server runs:

```sh
sqlite3 /var/lib/ondura-server/ondura.db ".backup '/backup/ondura-$(date +%F).db'"
```

Also back up `/etc/ondura-server`. To restore, stop the service, copy the backup to `/var/lib/ondura-server/ondura.db` (owner `ondura`), remove old `-wal`/`-shm` files and start the service. Phones resynchronise automatically.

## Updates

Install the new package over the old one (`apt install ./ondura-server_<new>.deb`) and restart the service. The database is upgraded automatically; clients keep their sync state.

## Troubleshooting

- **Port 25 unreachable / no mail arrives**: check the MX record, the firewall (`ss -ltnp | grep :25`) and that no other mail server is running.
- **Mail is rejected by Gmail, Microsoft, …**: check reverse DNS, SPF, DKIM (`ondura-server dns example.com`) and DMARC. The bounce in the sender's inbox contains the remote server's answer.
- **Phones do not find the server**: test `https://autodiscover.example.com/autodiscover/autodiscover.xml` (should ask for a password) or enter `mail.example.com` as server manually.
- **iPhone keeps asking for the password**: the certificate must be valid for the name the phone uses.
- **Device stuck**: remove it in the Admin Center; it will resynchronise.
- **More details**: `ONDURA_LOG=debug` (see [Configuration](#configuration)).

## Protocol support

| Protocol | Supported |
| --- | --- |
| SMTP | Receiving (port 25), submission with STARTTLS (587) and implicit TLS (465), AUTH PLAIN/LOGIN, SIZE, 8BITMIME, PIPELINING, SMTPUTF8 |
| Exchange ActiveSync | Versions 12.0–14.1: Provision (security policy, remote wipe), FolderSync and folder commands, Sync (mail, calendar, contacts, tasks, notes; windowing, filters, long polling), Ping (push), GetItemEstimate, SendMail, SmartReply, SmartForward, MoveItems, ItemOperations (fetch, attachments, empty folder), MeetingResponse, Settings (automatic replies, device and user information), ResolveRecipients (with free/busy), Search (address book and mailbox), ValidateCert |
| Exchange Web Services | Folders (get, find, create, update, move, delete, empty, hierarchy sync), items (find with paging, restrictions, sorting and search; calendar view with recurring series; get with MIME content; create, update, delete, move, copy, send; item sync), meeting requests, replies and cancellations, attachments, ResolveNames, automatic replies, free/busy (GetUserAvailability), room lists, streaming and pull notifications, mail tips |
| Autodiscover | POX (`/autodiscover/autodiscover.xml`, Outlook and mobile schema) and JSON (`/autodiscover/autodiscover.json`) |
| iCalendar (iTIP) | Invitations, replies and cancellations by mail with other systems |

Not supported: MAPI/HTTP (classic Outlook for Windows), IMAP/POP3, CalDAV/CardDAV, public folders.

## Legal notes

Exchange ActiveSync and Exchange Web Services are implemented from the protocol documentation Microsoft publishes free of charge as Open Specifications. The documentation states that Microsoft holds patents that may cover implementations of some of these protocols; some specifications fall under the Microsoft Open Specifications Promise, and for others Microsoft offers patent licenses (for example the Exchange ActiveSync and Exchange Server protocol patent licenses). Operators who distribute or run Ondura Server commercially should check whether such a license is needed for their use. This is not legal advice.

Microsoft, Exchange, ActiveSync and Outlook are trademarks of the Microsoft group of companies. They are used only to name the protocols and clients Ondura Server works with; Ondura Server is not affiliated with or endorsed by Microsoft.
