<p align="center">
  <img src="crates/server/src/admin/logo.svg" alt="Ondura" width="96">
</p>

<h1 align="center">Ondura Server</h1>

<p align="center"><b>Mail · Calendar · Contacts · Tasks · Files</b> – one server for your domain, with Exchange ActiveSync and Exchange Web Services</p>

<p align="center"><a href="https://ondura.mirranet.de">ondura.mirranet.de</a></p>

**Ondura Server** is a complete mail and collaboration server for small and medium organisations, in the style of Office 365. It receives and sends e-mail for your domains, keeps calendars, contacts and tasks, and provides cloud file storage with personal drives and shared libraries. Every person has an account; a mailbox is an optional capability of that account, so an account can use files and collaboration without being a mailbox. Phones, tablets and desktop apps connect with the protocols they already speak, so people just enter their e-mail address and password:

- **Ondura Mail** on Windows, macOS and Linux
- **Outlook for iOS and Android**, the built-in mail, calendar and contacts apps of **iPhone, iPad** and **Android**
- Desktop clients with **Exchange Web Services** (EWS)
- Any other mail program via **SMTP** submission

Everything runs as one program with one database: no separate mail transfer agent, no extra calendar server, no web server in front.

## Features

**Mail**

- Receiving mail for any number of domains, aliases and catch-all addresses, plus addressing (`name+tag@`)
- Sending with automatic DKIM signatures, a delivery queue with retries and clear non-delivery reports
- SPF, DKIM and DMARC checks on incoming mail; optional spam filtering with rspamd
- Automatic replies (out of office) with separate texts for colleagues and external senders
- Forwarding, shared mailboxes with full access and send-as rights

**Calendar, contacts, tasks**

- Calendars with recurring series, changed and cancelled occurrences and reminders – series are expanded on the server, so every client shows the same dates
- Meeting invitations, replies and cancellations between mailboxes and with other mail systems (iCalendar)
- Room mailboxes that accept bookings when free and decline conflicts
- Free/busy lookups for scheduling assistants
- Contacts with all common fields, tasks and notes
- Company directory (address book of all mailboxes)

**Phones and tablets**

- Push e-mail and calendar updates
- Device overview, blocking and remote wipe
- Automatic setup through Autodiscover

**Cloud storage**

- **Personal drives** (like OneDrive) and shared **libraries** (like SharePoint) with members and owner/editor/viewer roles — see [Cloud storage](docs/cloud.md)
- Library folders with their own permissions, file check-out and check-in, content approval, custom metadata columns with sorting and filtering, and guest links for single items or a whole library
- Folders, file upload and download, inline previews for images, PDFs and text, a Recent list, rename, move and a per-drive recycle bin
- **Share links** for files and folders that anyone can open as a read-only guest, and **version history** that keeps previous contents of a file
- **Nextcloud-compatible** so the official Nextcloud mobile and desktop apps connect to the personal drive and the shared libraries, for automatic photo upload and desktop sync — see [Nextcloud clients](docs/nextcloud.md)

**Collaboration**

- **Real-time collaborative documents**: several people signed in with their accounts edit the same text at once, with live presence, sharing per account (read-only or read-write), and conflict-free merging via Operational Transform
- Markdown formatting with a live preview, comments anchored to text with replies and @mentions (mentioned people get an e-mail), and download as Markdown, text, HTML or Word (.docx)
- Documents live inside the cloud storage next to ordinary files; a document REST API and a WebSocket let the Ondura clients integrate editing — see [Collaborative documents](docs/collab.md)

**Modular**

- Features are toggles: webmail, the Admin Center, cloud storage (`[cloud]`) and collaboration (`[collab]`) can each be turned off. Turn them all off and Ondura runs as a pure mail server

**Webmail**

- **Webmail** in the browser at `/mail`: sign in with any account and read, write and organise mail, calendar and contacts – no extra client needed, in German and English, light and dark
- A home dashboard with an app for each area — Mail (with calendar and contacts), Files, SharePoint, Tasks and Notes — each at its own URL
- Compose with attachments, reply and forward, flag, move and delete; month, week and day calendar views with invitations; a notification bell for mentions, shares and approvals; contact management
- Shares the same storage as ActiveSync and EWS, so changes appear everywhere at once

![Webmail](docs/screenshots/webmail.png)

**Administration**

- **Admin Center** in the browser: mailboxes, aliases, permissions, domains with the DNS records to enter, DKIM keys, the delivery queue, mobile devices and an audit log – in German and English, light and dark
- Command line for scripts and quick changes
- TLS certificates (e.g. Let's Encrypt) are picked up automatically after renewal

![Admin Center](docs/screenshots/admin-center.png)

## Download

Get the packages from the [releases page](https://github.com/Ondura-Suite/Ondura-Server/releases):

| System | File |
| --- | --- |
| Debian 12+, Ubuntu 22.04+ (x86_64) | `ondura-server_<version>-1_amd64.deb` |
| Debian 12+, Ubuntu 22.04+ (ARM64) | `ondura-server_<version>-1_arm64.deb` |
| Other Linux distributions | `ondura-server-<version>-<arch>-linux.tar.gz` |
| Container | build with the included `Dockerfile` |

On Debian and Ubuntu you can install from the MirraNET APT repository (`apt.mirranet.de`) instead, so new versions arrive with `sudo apt upgrade`:

```sh
sudo install -d /etc/apt/keyrings
sudo curl -fsSL https://apt.mirranet.de/mirranet.gpg -o /etc/apt/keyrings/mirranet.gpg
echo "deb [signed-by=/etc/apt/keyrings/mirranet.gpg] https://apt.mirranet.de stable main" \
    | sudo tee /etc/apt/sources.list.d/mirranet.list
sudo apt update && sudo apt install ondura-server
```

## Quick start

```sh
sudo apt install ./ondura-server_<version>-1_amd64.deb
sudo nano /etc/ondura-server/config.toml          # host name, certificate
sudo ondura-server domain add example.com         # prints the DNS records
sudo ondura-server user add you@example.com --name "Your Name" --admin
sudo systemctl start ondura-server
```

Then open `https://<your host name>/admin` and sign in with the mailbox you just created. The [admin guide](docs/admin.md) walks through DNS, certificates, firewalls and client setup.

## Documentation

- [Admin guide](docs/admin.md) – installation, configuration, DNS, certificates, clients, backups, accounts and mailboxes
- [Webmail](docs/webmail.md) – browser access to mail, calendar, contacts, files and SharePoint at `/mail`
- [Cloud storage](docs/cloud.md) – personal drives, shared libraries, sharing and versions
- [Nextcloud clients](docs/nextcloud.md) – connecting the Nextcloud apps for photo auto-upload and sync
- [Collaborative documents](docs/collab.md) – real-time co-editing, sharing, and the client API
- [Development](docs/development.md) – architecture, protocols, building and testing
- [Security](SECURITY.md) and [fuzzing](docs/fuzzing.md)

## License

Ondura Server is free software under the [GNU Affero General Public License v3.0](LICENSE) or later. Third-party components and their licenses are listed in [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt).

Exchange ActiveSync and Exchange Web Services are implemented from the protocol documentation Microsoft publishes in its Open Specifications; no Microsoft source code is used. Microsoft holds patents it actively licenses chiefly around **Exchange ActiveSync**; the EWS specifications largely fall under the Open Specifications Promise. Publishing and self-hosting this source is common practice for open-source ActiveSync servers (e.g. Z-Push) — a patent license mainly matters when you **operate the server for third parties or distribute it commercially**, and is primarily a US concern. This is not legal advice; see [Legal notes](docs/admin.md#legal-notes). Microsoft, Exchange, ActiveSync and Outlook are trademarks of the Microsoft group of companies; Ondura Server is not affiliated with or endorsed by Microsoft.
