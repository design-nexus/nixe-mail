//! Reading messages: headers, bodies, attachments, previews, and HTML made safe
//! to show.
//!
//! HTML goes through ammonia: no script, forms, frames, embeds, event
//! handlers or `javascript:`. Layout markup and CSS stay so newsletters look
//! right; the message view also runs with JavaScript off and every network
//! load blocked unless the user allows images, so CSS can't fetch anything.

use crate::thread;
use mail_parser::{MessageParser, MimeHeaders};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Person {
    pub name: String,
    pub email: String,
}

impl Person {
    pub fn label(&self) -> &str {
        if self.name.trim().is_empty() { &self.email } else { &self.name }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Attachment {
    /// Position among the message's attachments (for fetching the bytes).
    pub index: usize,
    pub name: String,
    pub mime: String,
    pub size: usize,
    /// For `cid:` images in the HTML.
    pub content_id: Option<String>,
    pub inline: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub subject: String,
    pub from: Option<Person>,
    pub reply_to: Vec<Person>,
    pub to: Vec<Person>,
    pub cc: Vec<Person>,
    /// Unix seconds.
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
    /// Mailing-list headers (the Feed uses these).
    pub list_id: Option<String>,
    pub list_unsubscribe: Option<String>,
}

fn people(addr: Option<&mail_parser::Address>) -> Vec<Person> {
    addr.map(|a| {
        a.iter()
            .filter_map(|p| {
                let email = p.address.as_deref()?.trim().to_string();
                (!email.is_empty())
                    .then(|| Person { name: p.name.as_deref().unwrap_or("").trim().trim_matches('"').to_string(), email })
            })
            .collect()
    })
    .unwrap_or_default()
}

fn id_list(value: &mail_parser::HeaderValue) -> Vec<String> {
    match value {
        mail_parser::HeaderValue::Text(t) => thread::ids(&format!("<{t}>")),
        mail_parser::HeaderValue::TextList(list) => list.iter().filter_map(|t| thread::normalize(t)).collect(),
        _ => Vec::new(),
    }
}

/// Reads a raw message. `None` only for input that isn't a message at all.
pub fn parse(raw: &[u8]) -> Option<Parsed> {
    let message = MessageParser::default().parse(raw)?;
    let header_text = |name: &str| {
        message.header_raw(name).map(|v| v.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|v| !v.is_empty())
    };
    let attachments = message
        .attachments()
        .enumerate()
        .map(|(index, part)| {
            let mime = part
                .content_type()
                .map(|c| match c.subtype() {
                    Some(sub) => format!("{}/{}", c.ctype(), sub),
                    None => c.ctype().to_string(),
                })
                .unwrap_or_else(|| "application/octet-stream".into())
                .to_ascii_lowercase();
            let inline = part.content_disposition().is_some_and(|d| d.ctype().eq_ignore_ascii_case("inline"));
            Attachment {
                index,
                name: part.attachment_name().map(clean_name).unwrap_or_else(|| default_name(index, &mime)),
                size: part.contents().len(),
                content_id: part.content_id().and_then(thread::normalize),
                inline,
                mime,
            }
        })
        .collect();
    Some(Parsed {
        subject: message.subject().unwrap_or("").trim().to_string(),
        from: people(message.from()).into_iter().next(),
        reply_to: people(message.reply_to()),
        to: people(message.to()),
        cc: people(message.cc()),
        date: message.date().map(|d| d.to_timestamp()),
        message_id: message.message_id().and_then(thread::normalize),
        in_reply_to: id_list(message.in_reply_to()),
        references: id_list(message.references()),
        text: message.body_text(0).map(|t| t.into_owned()).filter(|t| !t.trim().is_empty()),
        html: message
            .body_html(0)
            .map(|t| t.into_owned())
            .filter(|_| message.html_bodies().next().is_some_and(|p| p.is_text_html())),
        attachments,
        list_id: header_text("List-Id"),
        list_unsubscribe: header_text("List-Unsubscribe"),
    })
}

/// A file name without paths or control characters.
fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() { "attachment".into() } else { cleaned }
}

fn default_name(index: usize, mime: &str) -> String {
    let ext = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "application/pdf" => "pdf",
        "text/calendar" => "ics",
        "text/plain" => "txt",
        "message/rfc822" => "eml",
        _ => "bin",
    };
    format!("attachment-{}.{ext}", index + 1)
}

/// The bytes of attachment `index` of a raw message.
pub fn attachment_bytes(raw: &[u8], index: usize) -> Option<Vec<u8>> {
    let message = MessageParser::default().parse(raw)?;
    message.attachments().nth(index).map(|p| p.contents().to_vec())
}

/// The bytes of the attachment with a `cid:` content id.
pub fn cid_bytes(raw: &[u8], cid: &str) -> Option<(String, Vec<u8>)> {
    let parsed = parse(raw)?;
    let wanted = thread::normalize(cid)?;
    let att = parsed.attachments.iter().find(|a| a.content_id.as_deref() == Some(wanted.as_str()))?;
    Some((att.mime.clone(), attachment_bytes(raw, att.index)?))
}

impl Parsed {
    /// The message as plain text: its text part, or its HTML turned into text.
    pub fn plain_text(&self) -> String {
        if let Some(text) = &self.text {
            return text.clone();
        }
        self.html.as_deref().map(html_to_text).unwrap_or_default()
    }

    /// The first ~200 characters of the body on one line, without quoted text.
    pub fn preview(&self) -> String {
        let text = self.plain_text();
        let lines = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('>'))
            .take_while(|l| !(l.starts_with("On ") && l.ends_with("wrote:")) && *l != "--");
        let mut out = String::new();
        for line in lines {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(line);
            if out.chars().count() > 200 {
                break;
            }
        }
        out.chars().take(200).collect()
    }

    /// Everyone on the message, for search.
    pub fn people_text(&self) -> String {
        self.from.iter().chain(&self.to).chain(&self.cc).map(|p| format!("{} {}", p.name, p.email)).collect::<Vec<_>>().join(" ")
    }

    pub fn has_attachments(&self) -> bool {
        self.attachments.iter().any(|a| !a.inline || a.content_id.is_none())
    }
}

pub fn html_to_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 100).unwrap_or_default()
}

/// HTML ready for the message view, and whether it wants anything from the
/// network (images, fonts, CSS `url()`), which stays blocked until allowed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clean {
    pub html: String,
    pub remote: bool,
}

const LAYOUT_ATTRIBUTES: &[&str] = &[
    "style",
    "class",
    "align",
    "valign",
    "bgcolor",
    "background",
    "width",
    "height",
    "border",
    "cellpadding",
    "cellspacing",
    "color",
    "face",
    "size",
    "dir",
    "lang",
    "role",
    "colspan",
    "rowspan",
    "nowrap",
];

pub fn sanitize(html: &str) -> Clean {
    let remote = std::sync::Arc::new(AtomicBool::new(false));
    let seen = remote.clone();
    let mut builder = ammonia::Builder::default();
    builder
        .add_tags([
            "center",
            "font",
            "style",
            "span",
            "div",
            "section",
            "article",
            "header",
            "footer",
            "main",
            "nav",
            "figure",
            "figcaption",
            "picture",
            "source",
        ])
        .rm_clean_content_tags(["style"])
        .add_generic_attributes(LAYOUT_ATTRIBUTES.iter().copied())
        .add_tag_attributes("img", ["src", "alt", "title", "width", "height"])
        .add_tag_attributes("source", ["srcset", "media", "type"])
        .add_tag_attributes("a", ["href", "title", "name"])
        .url_schemes(HashSet::from(["http", "https", "mailto", "tel", "cid"]))
        .link_rel(Some("noopener noreferrer"))
        .strip_comments(true)
        .attribute_filter(move |_, attr, value| {
            let lower = value.to_ascii_lowercase();
            let fetches = match attr {
                "src" | "srcset" | "background" => lower.trim_start().starts_with("http"),
                "style" => lower.contains("url("),
                _ => false,
            };
            if fetches {
                seen.store(true, Ordering::Relaxed);
            }
            Some(value.into())
        });
    let html = builder.clean(html).to_string();
    // <style> blocks survive as text; note whether they load anything.
    if html.to_ascii_lowercase().contains("url(") || html.to_ascii_lowercase().contains("@import") {
        remote.store(true, Ordering::Relaxed);
    }
    Clean { html, remote: remote.load(Ordering::Relaxed) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALTERNATIVE: &str = "From: \"Sam Lee\" <sam@example.com>\r\nTo: Me <me@example.com>, ann@example.com\r\n\
Cc: team@example.com\r\nSubject: =?utf-8?q?Caf=C3=A9_plans?=\r\nDate: Wed, 21 Oct 2026 14:00:00 +0000\r\n\
Message-ID: <M1@Example.com>\r\nIn-Reply-To: <p@example.com>\r\nReferences: <root@example.com> <p@example.com>\r\n\
List-Id: Team <team.example.com>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nSee you at noon.\r\n\r\n> old quote\r\n--b\r\n\
Content-Type: text/html; charset=utf-8\r\n\r\n<p>See you at <b>noon</b>.</p>\r\n--b--\r\n";

    #[test]
    fn reads_headers_and_bodies() {
        let p = parse(ALTERNATIVE.as_bytes()).unwrap();
        assert_eq!(p.subject, "Café plans");
        assert_eq!(p.from, Some(Person { name: "Sam Lee".into(), email: "sam@example.com".into() }));
        assert_eq!(p.to.iter().map(|t| t.email.as_str()).collect::<Vec<_>>(), ["me@example.com", "ann@example.com"]);
        assert_eq!(p.cc.len(), 1);
        assert_eq!(p.date, Some(1_792_591_200));
        assert_eq!(p.message_id.as_deref(), Some("m1@example.com"));
        assert_eq!(p.in_reply_to, vec!["p@example.com"]);
        assert_eq!(p.references, vec!["root@example.com", "p@example.com"]);
        assert_eq!(p.preview(), "See you at noon.");
        assert!(p.html.as_deref().unwrap().contains("<b>noon</b>"));
        assert_eq!(p.list_id.as_deref(), Some("Team <team.example.com>"));
        assert!(!p.has_attachments());
    }

    #[test]
    fn reads_attachments() {
        let raw = "From: a@example.com\r\nSubject: Invoice\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\nAttached.\r\n--m\r\n\
Content-Type: application/pdf; name=\"../../invoice.pdf\"\r\nContent-Disposition: attachment; filename=\"../../invoice.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQK\r\n--m\r\nContent-Type: image/png\r\nContent-ID: <logo@x>\r\n\
Content-Disposition: inline\r\nContent-Transfer-Encoding: base64\r\n\r\niVBORw0KGgo=\r\n--m--\r\n";
        let p = parse(raw.as_bytes()).unwrap();
        assert_eq!(p.attachments.len(), 2);
        assert_eq!(p.attachments[0].name, "invoice.pdf");
        assert_eq!(p.attachments[0].mime, "application/pdf");
        assert_eq!(attachment_bytes(raw.as_bytes(), 0).unwrap(), b"%PDF-1.4\n");
        assert_eq!(p.attachments[1].content_id.as_deref(), Some("logo@x"));
        assert!(p.has_attachments());
        let (mime, bytes) = cid_bytes(raw.as_bytes(), "<LOGO@x>").unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(&bytes[..4], b"\x89PNG");
    }

    #[test]
    fn html_only_messages_get_text() {
        let raw = "From: n@example.com\r\nSubject: News\r\nContent-Type: text/html\r\n\r\n<h1>Big news</h1><p>Read <a href=\"https://x\">more</a></p>";
        let p = parse(raw.as_bytes()).unwrap();
        assert!(p.text.is_none() || p.plain_text().contains("Big news"));
        assert!(p.preview().contains("Big news"));
    }

    #[test]
    fn hostile_html_is_neutralized() {
        let hostile = r#"<script>alert(1)</script><p onclick="steal()">Hi</p>
            <a href="javascript:alert(1)">x</a><iframe src="https://evil"></iframe>
            <form action="https://evil"><input name=p></form><svg onload="x()"><circle/></svg>
            <img src="https://track.example/p.gif" width=1><object data="x"></object>
            <a href="https://ok.example/">ok</a><img src="cid:logo@x"><style>p{color:red}</style>"#;
        let clean = sanitize(hostile);
        let h = clean.html.to_ascii_lowercase();
        for bad in ["<script", "onclick", "javascript:", "<iframe", "<form", "<input", "<svg", "onload", "<object"] {
            assert!(!h.contains(bad), "{bad} survived: {h}");
        }
        assert!(h.contains("https://ok.example/"));
        assert!(h.contains("rel=\"noopener noreferrer\""));
        assert!(h.contains("cid:logo@x"));
        assert!(h.contains("<style>p{color:red}</style>"));
        assert!(clean.remote, "the tracking pixel counts as remote");
    }

    #[test]
    fn css_urls_count_as_remote() {
        assert!(sanitize(r#"<div style="background:url(https://t.example/x.png)">x</div>"#).remote);
        assert!(sanitize(r#"<style>@import "https://t.example/a.css";</style><p>x</p>"#).remote);
        assert!(!sanitize("<p>Hello</p>").remote);
    }

    #[test]
    fn attachment_names_are_safe() {
        assert_eq!(clean_name("..\\..\\x.exe"), "x.exe");
        assert_eq!(clean_name(".bashrc"), "bashrc");
        assert_eq!(clean_name("\u{7}"), "attachment");
    }
}
