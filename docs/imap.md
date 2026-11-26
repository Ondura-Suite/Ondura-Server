# IMAP and POP3

Ondura speaks IMAP4rev1 (and POP3) so standard mail clients — Thunderbird,
Apple Mail, K-9, mutt — can use an account alongside the Exchange protocols,
ActiveSync and the native Ondura client. All of them share one mailbox: a
message filed, flagged or categorized in one client shows up in the others.

## Ports

| Service | Port | Transport |
| --- | --- | --- |
| IMAP | 143 | STARTTLS |
| IMAP | 993 | implicit TLS |
| POP3 | 110 | STARTTLS |
| POP3 | 995 | implicit TLS |

The listeners are configured under `[imap]` in the server config and share the
SMTP/HTTPS TLS certificate. Set any list to empty to disable that port.

## IMAP support

Authentication is `LOGIN` and `AUTHENTICATE PLAIN`; on a plaintext port the
client must `STARTTLS` first. Implemented commands: `CAPABILITY`, `NOOP`,
`LOGOUT`, `STARTTLS`, `LIST`/`LSUB`, `NAMESPACE`, `SELECT`/`EXAMINE`, `STATUS`,
`CREATE`/`DELETE`/`RENAME`, `SUBSCRIBE`/`UNSUBSCRIBE`, `APPEND`, `CHECK`,
`CLOSE`, `EXPUNGE`, `FETCH`, `STORE`, `SEARCH`, `COPY`, `MOVE`, their `UID`
variants, and `IDLE`.

Folders map to mailboxes with `/` as the hierarchy separator; the inbox is
always `INBOX`, and Sent/Drafts/Trash/Junk/Archive carry their special-use
flags. Message flags map to `\Seen`, `\Flagged`, `\Answered` and `\Draft`;
`\Deleted` is tracked per session and acted on at `EXPUNGE` (moved to Deleted
Items, or removed outright when already there). Each mailbox reports a stable
`UIDVALIDITY` and messages carry a monotonic UID that rises on arrival.

`BODYSTRUCTURE` is reported as a single text part; clients fetch `BODY[]` (and
`BODY[HEADER]`/`BODY[TEXT]`/`HEADER.FIELDS`) for full content. `SEARCH` covers
the common criteria: flags, `UID`, `FROM`/`TO`/`CC`/`SUBJECT`/`BODY`/`TEXT`,
`SINCE`/`BEFORE`/`ON`, and `LARGER`/`SMALLER`.
