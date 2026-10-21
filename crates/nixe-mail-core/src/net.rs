//! Connections to mail servers: TLS only (implicit TLS or STARTTLS), checked
//! against the system's trusted certificates, signing in with a password or
//! an OAuth2 token (XOAUTH2).

use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

pub type Stream = TlsStream<TcpStream>;
pub type Session = async_imap::Session<Stream>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    /// TLS from the first byte (IMAP 993, SMTP 465).
    Implicit,
    /// A plain connection upgraded with STARTTLS (IMAP 143, SMTP 587).
    StartTls,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub user: String,
}

/// How to sign in. Secrets are used for one connection and dropped.
pub enum Auth {
    Password(String),
    OAuth2(String),
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Password(_) => "Auth::Password([redacted])",
            Self::OAuth2(_) => "Auth::OAuth2([redacted])",
        })
    }
}

#[derive(Debug)]
pub enum NetError {
    /// Couldn't reach the server (offline, DNS, refused, timed out).
    Unreachable(String),
    /// The server's certificate isn't trusted.
    Certificate(String),
    /// The server refused the credentials.
    SignIn(String),
    /// Anything else the server said.
    Protocol(String),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(m) => write!(f, "Couldn't reach the server: {m}"),
            Self::Certificate(m) => write!(f, "The server's certificate isn't trusted: {m}"),
            Self::SignIn(m) => write!(f, "The server refused to sign in: {m}"),
            Self::Protocol(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for NetError {}

impl NetError {
    /// Worth trying again later (as opposed to needing the user).
    pub fn transient(&self) -> bool {
        matches!(self, Self::Unreachable(_) | Self::Protocol(_))
    }
}

impl From<async_imap::error::Error> for NetError {
    fn from(e: async_imap::error::Error) -> Self {
        use async_imap::error::Error as E;
        match e {
            E::Io(io) => NetError::Unreachable(io.to_string()),
            E::ConnectionLost => NetError::Unreachable("the connection was lost".into()),
            other => NetError::Protocol(other.to_string()),
        }
    }
}

/// TLS settings: the system's trusted certificates, plus an extra CA file when
/// `NIXE_MAIL_EXTRA_CA` names one (tests and self-hosted servers).
pub fn tls_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if let Some(path) = std::env::var_os("NIXE_MAIL_EXTRA_CA")
        && let Ok(pem) = std::fs::read(&path)
    {
        use rustls_pki_types::pem::PemObject;
        for cert in rustls_pki_types::CertificateDer::pem_slice_iter(&pem).flatten() {
            let _ = roots.add(cert);
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("rustls protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

async fn tcp(server: &Server) -> Result<TcpStream, NetError> {
    let addr = format!("{}:{}", server.host, server.port);
    match tokio::time::timeout(std::time::Duration::from_secs(20), TcpStream::connect(&addr)).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(NetError::Unreachable(e.to_string())),
        Err(_) => Err(NetError::Unreachable(format!("{addr} didn't answer"))),
    }
}

async fn wrap(server: &Server, tcp: TcpStream) -> Result<Stream, NetError> {
    let name = rustls_pki_types::ServerName::try_from(server.host.clone()).map_err(|e| NetError::Protocol(e.to_string()))?;
    TlsConnector::from(tls_config()).connect(name, tcp).await.map_err(|e| {
        let text = e.to_string();
        if text.to_lowercase().contains("certificate") { NetError::Certificate(text) } else { NetError::Unreachable(text) }
    })
}

/// Connects to an IMAP server and signs in.
pub async fn imap(server: &Server, auth: Auth) -> Result<Session, NetError> {
    let stream = tcp(server).await?;
    let client = match server.security {
        Security::Implicit => {
            let mut c = async_imap::Client::new(wrap(server, stream).await?);
            greeting(&mut c).await?;
            c
        }
        Security::StartTls => {
            let mut plain = async_imap::Client::new(stream);
            greeting(&mut plain).await?;
            plain.run_command_and_check_ok("STARTTLS", None).await.map_err(|e| NetError::Protocol(format!("STARTTLS: {e}")))?;
            let tls = wrap(server, plain.into_inner()).await?;
            async_imap::Client::new(tls)
        }
    };
    let session = match auth {
        Auth::Password(password) => {
            client.login(&server.user, &password).await.map_err(|(e, _)| NetError::SignIn(e.to_string()))?
        }
        Auth::OAuth2(token) => {
            let xoauth2 = XOAuth2 { response: format!("user={}\x01auth=Bearer {token}\x01\x01", server.user) };
            client.authenticate("XOAUTH2", xoauth2).await.map_err(|(e, _)| NetError::SignIn(e.to_string()))?
        }
    };
    Ok(session)
}

async fn greeting<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + std::fmt::Debug + Send>(
    client: &mut async_imap::Client<T>,
) -> Result<(), NetError> {
    match tokio::time::timeout(std::time::Duration::from_secs(20), client.read_response()).await {
        Ok(Ok(Some(_))) => Ok(()),
        Ok(Ok(None)) => Err(NetError::Unreachable("the server closed the connection".into())),
        Ok(Err(e)) => Err(NetError::Unreachable(e.to_string())),
        Err(_) => Err(NetError::Unreachable("the server didn't greet".into())),
    }
}

struct XOAuth2 {
    response: String,
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> String {
        // The first round sends the token; a challenge back means it was refused,
        // and an empty answer ends the exchange so the error comes through.
        std::mem::take(&mut self.response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_stay_out_of_debug() {
        assert_eq!(format!("{:?}", Auth::Password("hunter2".into())), "Auth::Password([redacted])");
        assert!(!format!("{:?}", Auth::OAuth2("ya29.x".into())).contains("ya29"));
    }

    #[test]
    fn tls_config_loads_system_roots() {
        let _ = tls_config();
    }
}
