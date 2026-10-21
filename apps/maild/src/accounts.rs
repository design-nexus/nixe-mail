//! Mail accounts: GNOME Online Accounts with Mail switched on and IMAP settings.
//! Credentials come from GOA for each connection and are never kept.
//!
//! Tests (and only tests) can add accounts from a JSON file named by
//! `NIXE_MAIL_TEST_ACCOUNTS`: `[{"id", "address", "name", "imap": {host, port,
//! security, user}, "smtp": {...}, "password"}]`.

use nixe_mail_core::net::{Auth, Security, Server};
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct MailAccount {
    pub id: String,
    pub address: String,
    pub name: String,
    pub imap: Server,
    pub smtp: Option<Server>,
    pub gmail: bool,
    source: Source,
}

#[derive(Clone)]
enum Source {
    Goa { account: Box<nixe_accounts::Account>, oauth: bool },
    Test { password: String },
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Goa { oauth, .. } => write!(f, "Goa {{ oauth: {oauth} }}"),
            Self::Test { .. } => f.write_str("Test"),
        }
    }
}

/// A problem with an account the user should see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub id: String,
    pub address: String,
    pub reason: String,
}

fn security(tls: nixe_accounts::mail::Tls) -> Option<Security> {
    match tls {
        nixe_accounts::mail::Tls::Implicit => Some(Security::Implicit),
        nixe_accounts::mail::Tls::StartTls => Some(Security::StartTls),
        nixe_accounts::mail::Tls::None => None,
    }
}

fn server(s: &nixe_accounts::mail::Server) -> Option<Server> {
    Some(Server { host: s.host.clone(), port: s.port, security: security(s.tls)?, user: s.user.clone() })
}

/// Every usable mail account, and the ones that can't be used (and why).
pub fn discover(client: Option<&nixe_accounts::Client>) -> (Vec<MailAccount>, Vec<Skipped>) {
    let mut accounts = Vec::new();
    let mut skipped = Vec::new();
    if let Some(client) = client {
        for account in client.with_service(&nixe_accounts::Service::Mail).unwrap_or_default() {
            let settings = match client.mail_settings(&account) {
                Ok(Some(s)) => s,
                Ok(None) => {
                    let reason = if account.provider_type == "ms_graph" {
                        "Microsoft 365 mail isn't supported yet."
                    } else {
                        "This account has no mail server settings."
                    };
                    skipped.push(Skipped { id: account.id.clone(), address: account.label().to_string(), reason: reason.into() });
                    continue;
                }
                Err(e) => {
                    skipped.push(Skipped { id: account.id.clone(), address: account.label().to_string(), reason: e.to_string() });
                    continue;
                }
            };
            let Some(imap) = settings.imap.as_ref().and_then(server) else {
                skipped.push(Skipped {
                    id: account.id.clone(),
                    address: settings.address.clone(),
                    reason: "Mail needs a server that uses TLS.".into(),
                });
                continue;
            };
            accounts.push(MailAccount {
                id: account.id.clone(),
                address: settings.address.clone(),
                name: settings.name.clone(),
                smtp: settings.smtp.as_ref().and_then(server),
                gmail: account.provider_type == "google" || imap.host.ends_with("gmail.com"),
                imap,
                source: Source::Goa { oauth: settings.auth == nixe_accounts::mail::MailAuth::OAuth2, account: Box::new(account) },
            });
        }
    }
    accounts.extend(test_accounts());
    (accounts, skipped)
}

#[derive(Deserialize)]
struct TestServer {
    host: String,
    port: u16,
    security: String,
    user: String,
}

#[derive(Deserialize)]
struct TestAccount {
    id: String,
    address: String,
    #[serde(default)]
    name: String,
    imap: TestServer,
    smtp: Option<TestServer>,
    password: String,
}

fn test_accounts() -> Vec<MailAccount> {
    let Some(path) = std::env::var_os("NIXE_MAIL_TEST_ACCOUNTS") else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    let list: Vec<TestAccount> = serde_json::from_str(&text).unwrap_or_default();
    let conv = |s: TestServer| Server {
        host: s.host,
        port: s.port,
        security: if s.security == "starttls" { Security::StartTls } else { Security::Implicit },
        user: s.user,
    };
    list.into_iter()
        .map(|t| MailAccount {
            id: t.id,
            address: t.address,
            name: t.name,
            imap: conv(t.imap),
            smtp: t.smtp.map(conv),
            gmail: false,
            source: Source::Test { password: t.password },
        })
        .collect()
}

impl MailAccount {
    /// Credentials for IMAP, fresh from GOA.
    pub fn imap_auth(&self, client: Option<&nixe_accounts::Client>) -> Result<Auth, String> {
        self.auth(client, "imap-password")
    }

    pub fn smtp_auth(&self, client: Option<&nixe_accounts::Client>) -> Result<Auth, String> {
        self.auth(client, "smtp-password")
    }

    fn auth(&self, client: Option<&nixe_accounts::Client>, password_id: &str) -> Result<Auth, String> {
        match &self.source {
            Source::Test { password } => Ok(Auth::Password(password.clone())),
            Source::Goa { account, oauth } => {
                let client = client.ok_or("Online Accounts isn't available.")?;
                if *oauth {
                    let token = client.access_token(account).map_err(|e| e.to_string())?;
                    Ok(Auth::OAuth2(token.token.expose().to_string()))
                } else {
                    let secret = client.password(account, password_id).map_err(|e| e.to_string())?;
                    Ok(Auth::Password(secret.expose().to_string()))
                }
            }
        }
    }

    /// Asks GOA to check the account's sign-in (after the server refused it), so
    /// Settings shows "needs sign-in".
    pub fn check_sign_in(&self, client: Option<&nixe_accounts::Client>) {
        if let (Source::Goa { account, .. }, Some(client)) = (&self.source, client) {
            let _ = client.ensure_credentials(&account.path);
        }
    }
}
