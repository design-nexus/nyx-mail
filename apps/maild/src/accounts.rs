//! Native Mail accounts with Mail enabled, with IMAP
//! settings or (Microsoft 365, Outlook.com) Microsoft Graph. Credentials come
//! from Nyx Accounts private credential sessions.
//!
//! Tests (and only tests) can add accounts from a JSON file named by
//! `NYX_MAIL_TEST_ACCOUNTS`: `[{"id", "address", "name", "imap": {host, port,
//! security, user}, "smtp": {...}, "password"}]`, or with `"graph": BASE_URL`
//! in place of `imap` and `smtp` (the password is then the token).

use nyx_mail_core::net::{Auth, NetError, Security, Server};
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct MailAccount {
    pub id: String,
    pub address: String,
    pub name: String,
    pub backend: Backend,
    source: Source,
}

/// How an account's mail is reached.
#[derive(Clone, Debug)]
pub enum Backend {
    Imap {
        imap: Server,
        smtp: Option<Server>,
        gmail: bool,
    },
    /// Microsoft Graph at this base URL.
    Graph {
        base: String,
    },
}

#[derive(Clone)]
enum Source {
    Native { id: String, oauth: bool },
    Test { password: String },
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native { oauth, .. } => write!(f, "Native {{ oauth: {oauth} }}"),
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

fn server(endpoint: &str, user: &str) -> Option<Server> {
    let uri = url::Url::parse(endpoint).ok()?;
    let (security, default_port) = match uri.scheme() {
        "imaps" => (Security::Implicit, 993),
        "imap+starttls" => (Security::StartTls, 143),
        "smtps" => (Security::Implicit, 465),
        "smtp+starttls" => (Security::StartTls, 587),
        _ => return None,
    };
    Some(Server { host: uri.host_str()?.into(), port: uri.port().unwrap_or(default_port), security, user: user.into() })
}

pub fn discover(client: Option<&nyx_accounts::Client>) -> (Vec<MailAccount>, Vec<Skipped>) {
    let mut accounts = Vec::new();
    let mut skipped = Vec::new();
    if let Some(client) = client {
        for account in client.list().unwrap_or_default().into_iter().filter(|a| a.enabled.contains(&nyx_accounts::Service::Mail)) {
            let backend = if account.provider == "microsoft_personal" {
                Backend::Graph { base: nyx_mail_core::graph::GRAPH.into() }
            } else {
                let Some(imap) = account.endpoints.get("imap").and_then(|s| server(s, &account.identity)) else {
                    skipped.push(Skipped { id: account.id.clone(), address: account.identity.clone(), reason: "Configure a secure IMAP endpoint in Online Accounts.".into() }); continue;
                };
                Backend::Imap { smtp: account.endpoints.get("smtp").and_then(|s| server(s, &account.identity)), gmail: account.provider == "google", imap }
            };
            let oauth = matches!(account.provider.as_str(), "google" | "microsoft_personal");
            accounts.push(MailAccount { id: account.legacy_id.clone().unwrap_or_else(|| account.id.clone()), address: account.identity, name: account.label, backend, source: Source::Native { id: account.id, oauth } });
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
    imap: Option<TestServer>,
    smtp: Option<TestServer>,
    graph: Option<String>,
    password: String,
}

fn test_accounts() -> Vec<MailAccount> {
    let Some(path) = std::env::var_os("NYX_MAIL_TEST_ACCOUNTS") else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    let list: Vec<TestAccount> = serde_json::from_str(&text).unwrap_or_default();
    let conv = |s: TestServer| Server {
        host: s.host,
        port: s.port,
        security: if s.security == "starttls" { Security::StartTls } else { Security::Implicit },
        user: s.user,
    };
    list.into_iter()
        .filter_map(|t| {
            let backend = match (t.graph, t.imap) {
                (Some(base), _) => Backend::Graph { base },
                (None, Some(imap)) => Backend::Imap { imap: conv(imap), smtp: t.smtp.map(conv), gmail: false },
                (None, None) => return None,
            };
            Some(MailAccount {
                id: t.id,
                address: t.address,
                name: t.name,
                backend,
                source: Source::Test { password: t.password },
            })
        })
        .collect()
}

impl MailAccount {
    pub fn is_graph(&self) -> bool {
        matches!(self.backend, Backend::Graph { .. })
    }

    /// Whether mail can go out from it.
    pub fn can_send(&self) -> bool {
        match &self.backend {
            Backend::Imap { smtp, .. } => smtp.is_some(),
            Backend::Graph { .. } => true,
        }
    }

    /// A Graph connection with a fresh token (Graph accounts only).
    pub fn graph(&self, client: Option<&nyx_accounts::Client>) -> Result<nyx_mail_core::graph::Graph, NetError> {
        let Backend::Graph { base } = &self.backend else { return Err(NetError::Refused("not a Microsoft 365 account".into())) };
        let token = match self.auth(client, "").map_err(NetError::SignIn)? {
            Auth::OAuth2(token) | Auth::Password(token) => token,
        };
        nyx_mail_core::graph::Graph::new(base, token)
    }

    /// Credentials for IMAP, fresh from GOA.
    pub fn imap_auth(&self, client: Option<&nyx_accounts::Client>) -> Result<Auth, String> {
        self.auth(client, "imap-password")
    }

    pub fn smtp_auth(&self, client: Option<&nyx_accounts::Client>) -> Result<Auth, String> {
        self.auth(client, "smtp-password")
    }

    fn auth(&self, client: Option<&nyx_accounts::Client>, _password_id: &str) -> Result<Auth, String> {
        match &self.source {
            Source::Test { password } => Ok(Auth::Password(password.clone())),
            Source::Native { id, oauth } => {
                let client = client.ok_or("Online Accounts is unavailable")?;
                let credentials = client.credential(id, nyx_accounts::Service::Mail).map_err(|_| "Account needs sign-in or its service is disabled".to_string())?;
                if *oauth { Ok(Auth::OAuth2(credentials.access_token.clone())) }
                else { Ok(Auth::Password(credentials.password.clone())) }
            }
        }
    }
    pub fn check_sign_in(&self, client: Option<&nyx_accounts::Client>) {
        if let (Source::Native{id,..},Some(client))=(&self.source,client){let _=client.report_auth_failure(id,nyx_accounts::Service::Mail);}
    }
}
