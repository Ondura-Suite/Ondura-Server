//! DNS records a domain needs for this server (shown by the CLI and the
//! Admin Center).

use serde::Serialize;

use crate::smtp::dkim;
use crate::store::Store;

#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub kind: &'static str,
    pub name: String,
    pub value: String,
    pub purpose: &'static str,
}

pub fn records(store: &Store, hostname: &str, domain: &str) -> Vec<Record> {
    let mut out = vec![
        Record { kind: "MX", name: format!("{domain}."), value: format!("10 {hostname}."), purpose: "Mail for the domain is delivered to this server" },
        Record { kind: "TXT", name: format!("{domain}."), value: format!("v=spf1 mx a:{hostname} ~all"), purpose: "SPF: this server may send mail for the domain" },
        Record {
            kind: "TXT",
            name: format!("_dmarc.{domain}."),
            value: format!("v=DMARC1; p=quarantine; rua=mailto:postmaster@{domain}"),
            purpose: "DMARC policy",
        },
        Record { kind: "CNAME", name: format!("autodiscover.{domain}."), value: format!("{hostname}."), purpose: "Autodiscover for Outlook, phones and Ondura" },
        Record {
            kind: "SRV",
            name: format!("_autodiscover._tcp.{domain}."),
            value: format!("0 0 443 {hostname}."),
            purpose: "Autodiscover (alternative to the CNAME)",
        },
        Record { kind: "SRV", name: format!("_submission._tcp.{domain}."), value: format!("0 1 587 {hostname}."), purpose: "Mail submission for other clients" },
    ];
    if let Ok(Some(key)) = store.dkim(domain) {
        out.insert(2, Record { kind: "TXT", name: format!("{}._domainkey.{domain}.", key.selector), value: dkim::dns_record(&key), purpose: "DKIM signature key" });
    }
    out
}

/// The server's own host name needs an A/AAAA record and matching reverse DNS.
pub fn host_hint(hostname: &str) -> String {
    format!("{hostname} needs A/AAAA records pointing to this server, and the reverse DNS (PTR) of these addresses should be {hostname}.")
}
