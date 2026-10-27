//! Test support (the `testing` feature): the Dovecot that `tools/test-imap.sh`
//! starts, and an SMTP server to send to.

use crate::mime::Person;
use crate::net::{self, Auth, NetError, Security, Server, Session};
use crate::send::{self, Draft};
use futures::TryStreamExt;
use std::sync::{Arc, Mutex};

pub fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("{key} isn't set; run tools/test-imap.sh"))
}

/// The test Dovecot: any user name signs in with the password `secret` and
/// gets a mail store of its own.
pub struct Dovecot {
    pub port: u16,
}

impl Dovecot {
    pub fn from_env() -> Self {
        Self { port: env("NYX_MAIL_TEST_IMAPS").parse().expect("a port") }
    }

    pub fn server(&self, user: &str) -> Server {
        Server { host: "127.0.0.1".into(), port: self.port, security: Security::Implicit, user: user.into() }
    }

    pub async fn session(&self, user: &str) -> Result<Session, NetError> {
        net::imap(&self.server(user), Auth::Password("secret".into())).await
    }

    /// Puts a message straight into a mailbox, as another client would.
    pub async fn deliver(&self, user: &str, mailbox: &str, subject: &str, body: &str) {
        self.deliver_from(user, mailbox, "sam@example.test", subject, body).await
    }

    pub async fn deliver_from(&self, user: &str, mailbox: &str, sender: &str, subject: &str, body: &str) {
        let mut s = self.session(user).await.unwrap();
        s.append(mailbox, None, None, &message_from(sender, subject, body)).await.unwrap();
        s.logout().await.unwrap();
    }

    /// A message's system flags, by subject (not keywords such as Dovecot's
    /// `$HasAttachment`).
    pub async fn flags(&self, user: &str, mailbox: &str, subject: &str) -> Vec<String> {
        let mut s = self.session(user).await.unwrap();
        s.select(mailbox).await.unwrap();
        let uids = s.uid_search(format!("SUBJECT \"{subject}\"")).await.unwrap();
        let uid = *uids.iter().next().expect("message on the server");
        let fetched: Vec<_> = s.uid_fetch(uid.to_string(), "(FLAGS)").await.unwrap().try_collect().await.unwrap();
        let flags = fetched[0].flags().map(|f| format!("{f:?}")).filter(|f| !f.starts_with("Custom")).collect();
        s.logout().await.unwrap();
        flags
    }

    pub async fn count(&self, user: &str, mailbox: &str) -> u32 {
        let mut s = self.session(user).await.unwrap();
        let m = s.select(mailbox).await.unwrap();
        s.logout().await.unwrap();
        m.exists
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// A plain message from Sam Lee.
pub fn message(subject: &str, body: &str) -> Vec<u8> {
    message_from("sam@example.test", subject, body)
}

pub fn message_from(sender: &str, subject: &str, body: &str) -> Vec<u8> {
    let name = if sender == "sam@example.test" { "Sam Lee" } else { "" };
    let draft = Draft {
        from: Person { name: name.into(), email: sender.into() },
        to: vec![Person { name: String::new(), email: "me@example.test".into() }],
        subject: subject.into(),
        body: body.into(),
        ..Draft::default()
    };
    draft.build(&send::message_id(sender), now())
}

/// A tiny SMTP server over implicit TLS that accepts anything and keeps what
/// it got: (AUTH line, MAIL FROM, RCPT TOs, DATA).
pub type Got = (String, String, Vec<String>, Vec<u8>);

pub struct Sink {
    pub port: u16,
    pub messages: Arc<Mutex<Vec<Got>>>,
}

impl Sink {
    pub async fn start(cert: &str, key: &str) -> Self {
        use rustls_pki_types::pem::PemObject;
        let certs = rustls_pki_types::CertificateDer::pem_file_iter(cert).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        let key = rustls_pki_types::PrivateKeyDer::from_pem_file(key).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let keep = messages.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let Ok(tls) = acceptor.accept(tcp).await else { continue };
                if let Ok(got) = Self::serve(tls).await {
                    keep.lock().unwrap().push(got);
                }
            }
        });
        Self { port, messages }
    }

    async fn serve(stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin) -> std::io::Result<Got> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut stream = BufReader::new(stream);
        let mut got: Got = Default::default();
        stream.write_all(b"220 sink ESMTP\r\n").await?;
        stream.flush().await?;
        let mut line = String::new();
        loop {
            line.clear();
            if stream.read_line(&mut line).await? == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            let upper = line.to_ascii_uppercase();
            let reply: &[u8] = if upper.starts_with("EHLO") {
                b"250-sink\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n"
            } else if upper.starts_with("AUTH") {
                use base64::Engine;
                let encoded = line.split_whitespace().nth(2).unwrap_or_default();
                let decoded = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap_or_default();
                got.0 = String::from_utf8_lossy(&decoded).replace('\0', " ");
                b"235 ok\r\n"
            } else if upper.starts_with("MAIL FROM:") {
                got.1 = address(&line);
                b"250 ok\r\n"
            } else if upper.starts_with("RCPT TO:") {
                got.2.push(address(&line));
                b"250 ok\r\n"
            } else if upper.starts_with("DATA") {
                stream.write_all(b"354 go\r\n").await?;
                stream.flush().await?;
                loop {
                    line.clear();
                    stream.read_line(&mut line).await?;
                    if line == ".\r\n" || line.is_empty() {
                        break;
                    }
                    got.3.extend_from_slice(line.as_bytes());
                }
                b"250 queued\r\n"
            } else if upper.starts_with("QUIT") {
                stream.write_all(b"221 bye\r\n").await?;
                stream.flush().await?;
                return Ok(got);
            } else {
                b"250 ok\r\n"
            };
            stream.write_all(reply).await?;
            stream.flush().await?;
        }
    }
}

fn address(line: &str) -> String {
    line.split_once('<').and_then(|(_, rest)| rest.split_once('>')).map(|(a, _)| a.to_string()).unwrap_or_default()
}
