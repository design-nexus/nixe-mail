//! Writing and sending: drafts become RFC 5322 messages (mail-builder), replies
//! and forwards are prepared from the original, and SMTP (lettre) sends over
//! TLS with a password or XOAUTH2.

use crate::mime::{Parsed, Person};
use crate::net::{Auth, NetError, Security, Server};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Certificate, Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAttachment {
    pub name: String,
    pub mime: String,
    /// Base64 in JSON (drafts and the engine's D-Bus calls).
    #[serde(with = "base64_bytes")]
    pub bytes: Vec<u8>,
}

mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

/// A message being written.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Draft {
    pub account: String,
    pub from: Person,
    pub to: Vec<Person>,
    pub cc: Vec<Person>,
    pub bcc: Vec<Person>,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<FileAttachment>,
}

impl Draft {
    /// Everyone it goes to (To, Cc and Bcc), once each.
    pub fn recipients(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for p in self.to.iter().chain(&self.cc).chain(&self.bcc) {
            if !out.iter().any(|e| e.eq_ignore_ascii_case(&p.email)) {
                out.push(p.email.clone());
            }
        }
        out
    }

    /// The message as it goes out (no Bcc header). `id` is its Message-ID
    /// without brackets; `date` Unix seconds.
    pub fn build(&self, id: &str, date: i64) -> Vec<u8> {
        let addr = |p: &Person| Address::new_address(Some(p.name.clone()).filter(|n| !n.is_empty()), p.email.clone());
        let list = |ps: &[Person]| Address::new_list(ps.iter().map(addr).collect());
        let mut m = MessageBuilder::new()
            .from(addr(&self.from))
            .subject(self.subject.clone())
            .message_id(id.to_string())
            .date(date)
            .text_body(self.body.clone());
        if !self.to.is_empty() {
            m = m.to(list(&self.to));
        }
        if !self.cc.is_empty() {
            m = m.cc(list(&self.cc));
        }
        if let Some(parent) = &self.in_reply_to {
            m = m.in_reply_to(parent.clone());
        }
        if !self.references.is_empty() {
            m = m.references(self.references.clone());
        }
        for a in &self.attachments {
            m = m.attachment(a.mime.clone(), a.name.clone(), a.bytes.clone());
        }
        m.write_to_vec().unwrap_or_default()
    }
}

/// Looks like an address a server will take: one `@`, something on both
/// sides, no spaces or brackets.
pub fn valid_address(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else { return false };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email.chars().any(|c| c.is_whitespace() || "<>(),;:\\\"[]".contains(c))
}

/// Recipients as typed: `Sam Lee <sam@example.com>, ann@example.com`.
/// Commas inside quotes or angle brackets don't split. An entry that isn't an
/// address comes back as the error.
pub fn parse_people(text: &str) -> Result<Vec<Person>, String> {
    let mut parts = Vec::new();
    let (mut current, mut quoted, mut angle) = (String::new(), false, false);
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' if !quoted && !angle => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    parts.push(current);
    let mut out: Vec<Person> = Vec::new();
    for part in parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        let person = match (part.rfind('<'), part.rfind('>')) {
            (Some(open), Some(close)) if open < close => Person {
                name: part[..open].trim().trim_matches('"').trim().to_string(),
                email: part[open + 1..close].trim().to_string(),
            },
            _ => Person { name: String::new(), email: part.trim_matches(['<', '>']).to_string() },
        };
        if !valid_address(&person.email) {
            return Err(part.to_string());
        }
        if !out.iter().any(|p| p.email.eq_ignore_ascii_case(&person.email)) {
            out.push(person);
        }
    }
    Ok(out)
}

/// The other way round, for showing recipients in an entry.
pub fn people_text(people: &[Person]) -> String {
    people
        .iter()
        .map(|p| {
            if p.name.is_empty() {
                p.email.clone()
            } else if p.name.contains([',', ';', '"', '<', '>']) {
                format!("\"{}\" <{}>", p.name.replace('"', ""), p.email)
            } else {
                format!("{} <{}>", p.name, p.email)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A draft from a `mailto:` link (RFC 6068): recipients, `cc`, `bcc`,
/// `subject` and `body`. Other headers are ignored.
pub fn parse_mailto(uri: &str) -> Draft {
    let rest = uri.trim().strip_prefix("mailto:").or_else(|| uri.trim().strip_prefix("MAILTO:")).unwrap_or(uri.trim());
    let (to, query) = rest.split_once('?').unwrap_or((rest, ""));
    let people = |text: &str| parse_people(&percent_decode(text)).unwrap_or_default();
    let mut draft = Draft { to: people(to), ..Draft::default() };
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key.to_ascii_lowercase().as_str() {
            "to" => draft.to.extend(people(value)),
            "cc" => draft.cc.extend(people(value)),
            "bcc" => draft.bcc.extend(people(value)),
            "subject" => draft.subject = percent_decode(value),
            "body" => draft.body = percent_decode(value).replace("\r\n", "\n"),
            _ => {}
        }
    }
    draft
}

/// A new Message-ID for a message from `from` (`random@domain`).
pub fn message_id(from: &str) -> String {
    let domain = from.rsplit_once('@').map(|(_, d)| d).filter(|d| !d.is_empty()).unwrap_or("localhost");
    let mut bytes = [0u8; 12];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut bytes);
    }
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{token}@{domain}")
}

/// "Re: x" unless it already starts so.
fn prefixed(prefix: &str, subject: &str) -> String {
    let lower = subject.trim().to_ascii_lowercase();
    if lower.starts_with(&format!("{}:", prefix.to_ascii_lowercase())) {
        subject.trim().to_string()
    } else {
        format!("{prefix}: {}", subject.trim())
    }
}

fn quote(text: &str) -> String {
    text.lines().map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n")
}

/// A reply (to the sender, or to everyone with `all`). `me` are the user's own
/// addresses, left out of the recipients. `when` describes the original's date.
pub fn reply(original: &Parsed, all: bool, me: &[String], from: Person, account: &str, when: &str) -> Draft {
    let mine = |e: &str| me.iter().any(|m| m.eq_ignore_ascii_case(e));
    let sender = original.reply_to.first().cloned().or(original.from.clone());
    let mut to: Vec<Person> = sender.into_iter().filter(|p| !mine(&p.email)).collect();
    // Replying to your own message goes to its recipients.
    if to.is_empty() {
        to = original.to.clone();
    }
    let mut cc = Vec::new();
    if all {
        for p in original.to.iter().chain(&original.cc) {
            if !mine(&p.email) && !to.iter().chain(&cc).any(|q: &Person| q.email.eq_ignore_ascii_case(&p.email)) {
                cc.push(p.clone());
            }
        }
    }
    let mut references = original.references.clone();
    if let Some(id) = &original.message_id
        && !references.contains(id)
    {
        references.push(id.clone());
    }
    let who = original.from.as_ref().map(|p| p.label().to_string()).unwrap_or_default();
    Draft {
        account: account.to_string(),
        from,
        to,
        cc,
        subject: prefixed("Re", &original.subject),
        body: format!("\n\nOn {when}, {who} wrote:\n{}", quote(original.plain_text().trim_end())),
        in_reply_to: original.message_id.clone(),
        references,
        ..Draft::default()
    }
}

/// A forward with the original's text and attachments.
pub fn forward(original: &Parsed, attachments: Vec<FileAttachment>, from: Person, account: &str, when: &str) -> Draft {
    let people = people_text;
    let header = format!(
        "\n\n---------- Forwarded message ----------\nFrom: {}\nDate: {when}\nSubject: {}\nTo: {}\n\n",
        original.from.as_ref().map(|p| people(std::slice::from_ref(p))).unwrap_or_default(),
        original.subject,
        people(&original.to)
    );
    Draft {
        account: account.to_string(),
        from,
        subject: prefixed("Fwd", &original.subject),
        body: format!("{header}{}", original.plain_text().trim_end()),
        attachments,
        ..Draft::default()
    }
}

/// Sends a raw message over SMTP.
pub async fn smtp(server: &Server, auth: Auth, from: &str, to: &[String], raw: &[u8]) -> Result<(), NetError> {
    let mut tls = TlsParameters::builder(server.host.clone());
    if let Some(path) = std::env::var_os("NIXE_MAIL_EXTRA_CA")
        && let Ok(pem) = std::fs::read(&path)
        && let Ok(cert) = Certificate::from_pem(&pem)
    {
        tls = tls.add_root_certificate(cert);
    }
    let tls = tls.build_rustls().map_err(|e| NetError::Certificate(e.to_string()))?;
    let (credentials, mechanism) = match auth {
        Auth::Password(p) => (Credentials::new(server.user.clone(), p), vec![Mechanism::Plain, Mechanism::Login]),
        Auth::OAuth2(t) => (Credentials::new(server.user.clone(), t), vec![Mechanism::Xoauth2]),
    };
    let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(server.host.clone())
        .port(server.port)
        .tls(match server.security {
            Security::Implicit => Tls::Wrapper(tls),
            Security::StartTls => Tls::Required(tls),
        })
        .credentials(credentials)
        .authentication(mechanism)
        .timeout(Some(std::time::Duration::from_secs(60)))
        .build();
    let envelope = lettre::address::Envelope::new(
        Some(from.parse().map_err(|e| NetError::SignIn(format!("the From address: {e}")))?),
        to.iter()
            .map(|t| t.parse())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| NetError::SignIn(format!("a recipient's address: {e}")))?,
    )
    .map_err(|e| NetError::SignIn(e.to_string()))?;
    transport.send_raw(&envelope, raw).await.map(|_| ()).map_err(|e| {
        let text = e.to_string();
        if e.is_permanent() {
            if text.to_lowercase().contains("auth") { NetError::SignIn(text) } else { NetError::Protocol(text) }
        } else if text.to_lowercase().contains("certificate") {
            NetError::Certificate(text)
        } else {
            NetError::Unreachable(text)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mime;

    fn person(name: &str, email: &str) -> Person {
        Person { name: name.into(), email: email.into() }
    }

    #[test]
    fn builds_and_reads_back() {
        let draft = Draft {
            account: "a".into(),
            from: person("Me", "me@example.com"),
            to: vec![person("Sam Lee", "sam@example.com")],
            cc: vec![person("", "ann@example.com")],
            bcc: vec![person("", "secret@example.com")],
            subject: "Café on Friday".into(),
            body: "See you there.\n".into(),
            in_reply_to: Some("p@example.com".into()),
            references: vec!["root@example.com".into(), "p@example.com".into()],
            attachments: vec![FileAttachment { name: "plan.txt".into(), mime: "text/plain".into(), bytes: b"step 1".to_vec() }],
        };
        let raw = draft.build("new@example.com", 1_792_591_200);
        let text = String::from_utf8_lossy(&raw);
        assert!(!text.contains("secret@example.com"), "Bcc must not appear in the message");
        let back = mime::parse(&raw).unwrap();
        assert_eq!(back.subject, "Café on Friday");
        assert_eq!(back.message_id.as_deref(), Some("new@example.com"));
        assert_eq!(back.in_reply_to, vec!["p@example.com"]);
        assert_eq!(back.references, vec!["root@example.com", "p@example.com"]);
        assert_eq!(back.to[0].email, "sam@example.com");
        assert_eq!(back.attachments[0].name, "plan.txt");
        assert_eq!(draft.recipients(), vec!["sam@example.com", "ann@example.com", "secret@example.com"]);
    }

    #[test]
    fn replies_and_forwards() {
        let original = Parsed {
            subject: "Lunch".into(),
            from: Some(person("Sam", "sam@example.com")),
            to: vec![person("Me", "ME@example.com"), person("Ann", "ann@example.com")],
            cc: vec![person("", "bob@example.com")],
            message_id: Some("m1@example.com".into()),
            references: vec!["root@example.com".into()],
            text: Some("Ramen?\nOr tacos.".into()),
            ..Parsed::default()
        };
        let me = vec!["me@example.com".to_string()];
        let r = reply(&original, false, &me, person("Me", "me@example.com"), "a", "Oct 21");
        assert_eq!(r.to.iter().map(|p| p.email.as_str()).collect::<Vec<_>>(), ["sam@example.com"]);
        assert!(r.cc.is_empty());
        assert_eq!(r.subject, "Re: Lunch");
        assert!(r.body.contains("On Oct 21, Sam wrote:\n> Ramen?\n> Or tacos."));
        assert_eq!(r.references, vec!["root@example.com", "m1@example.com"]);
        assert_eq!(r.in_reply_to.as_deref(), Some("m1@example.com"));
        let all = reply(&original, true, &me, person("Me", "me@example.com"), "a", "Oct 21");
        assert_eq!(all.cc.iter().map(|p| p.email.as_str()).collect::<Vec<_>>(), ["ann@example.com", "bob@example.com"]);
        let again = reply(
            &Parsed { subject: "RE: Lunch".into(), ..original.clone() },
            false,
            &me,
            person("", "me@example.com"),
            "a",
            "x",
        );
        assert_eq!(again.subject, "RE: Lunch");
        let f = forward(&original, Vec::new(), person("", "me@example.com"), "a", "Oct 21");
        assert_eq!(f.subject, "Fwd: Lunch");
        assert!(f.body.contains("From: Sam <sam@example.com>") && f.body.contains("Ramen?"));
        assert!(f.to.is_empty());
    }

    #[test]
    fn typed_recipients() {
        let people =
            parse_people("Sam Lee <sam@example.com>, ann@example.com; \"Lee, Bo\" <bo@example.com>,, SAM@example.com").unwrap();
        assert_eq!(
            people,
            vec![person("Sam Lee", "sam@example.com"), person("", "ann@example.com"), person("Lee, Bo", "bo@example.com")]
        );
        assert_eq!(parse_people("sam@example.com, not an address"), Err("not an address".into()));
        assert_eq!(parse_people("  ").unwrap(), vec![]);
        assert_eq!(parse_people(&people_text(&people)).unwrap(), people);
        assert!(!valid_address("a@b@c") && !valid_address("@x") && !valid_address("a b@x") && valid_address("me@localhost"));
    }

    #[test]
    fn mailto_links() {
        let d = parse_mailto(
            "mailto:sam@example.com,ann@example.com?cc=bo%40example.com&subject=Caf%C3%A9%20plans&body=Line%201%0D%0ALine%202&x-other=1",
        );
        assert_eq!(d.to.iter().map(|p| p.email.as_str()).collect::<Vec<_>>(), ["sam@example.com", "ann@example.com"]);
        assert_eq!(d.cc[0].email, "bo@example.com");
        assert_eq!(d.subject, "Café plans");
        assert_eq!(d.body, "Line 1\nLine 2");
        assert_eq!(parse_mailto("mailto:?subject=Hi").to, vec![]);
        assert_eq!(parse_mailto("mailto:a@x%").to[0].email, "a@x%");
    }

    #[test]
    fn drafts_travel_as_json_with_base64_files() {
        let d = Draft {
            attachments: vec![FileAttachment {
                name: "a.bin".into(),
                mime: "application/octet-stream".into(),
                bytes: vec![0, 255, 7],
            }],
            ..Draft::default()
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"AP8H\""), "{json}");
        assert_eq!(serde_json::from_str::<Draft>(&json).unwrap(), d);
        // Missing fields are fine (older drafts, other callers).
        assert_eq!(serde_json::from_str::<Draft>(r#"{"subject":"Hi"}"#).unwrap().subject, "Hi");
    }

    #[test]
    fn message_ids_use_the_sender_domain() {
        let id = message_id("me@example.com");
        assert!(id.ends_with("@example.com") && id.len() > 20);
        assert_ne!(id, message_id("me@example.com"));
    }
}
