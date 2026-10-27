//! Message ids and conversation keys.
//!
//! A conversation is keyed by its root message id: the first id in
//! References, else In-Reply-To, else the message's own id. When a parent is
//! already stored, the message joins the parent's conversation, so replies
//! that drop References still thread. Gmail's thread ids win when present.

/// `<abc@host>` → `abc@host`, lowercased; `None` when empty.
pub fn normalize(id: &str) -> Option<String> {
    let id = id.trim().trim_start_matches('<').trim_end_matches('>').trim();
    (!id.is_empty()).then(|| id.to_ascii_lowercase())
}

/// Every `<id>` in a References or In-Reply-To header, in order.
pub fn ids(header: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = header;
    while let Some(start) = rest.find('<') {
        let Some(end) = rest[start..].find('>') else { break };
        if let Some(id) = normalize(&rest[start + 1..start + end])
            && !out.contains(&id)
        {
            out.push(id);
        }
        rest = &rest[start + end + 1..];
    }
    // Headers without angle brackets (rare): whitespace-separated ids.
    if out.is_empty() {
        out = header.split_whitespace().filter_map(normalize).collect();
    }
    out
}

/// The conversation key for a message. `known` finds the conversation of an
/// already-stored message by its id.
pub fn key(
    message_id: Option<&str>,
    in_reply_to: &[String],
    references: &[String],
    gmail_thread: Option<u64>,
    known: impl Fn(&str) -> Option<String>,
) -> String {
    if let Some(t) = gmail_thread {
        return format!("gm:{t}");
    }
    // Newest parent first: the direct parent is the likeliest to be stored.
    for parent in in_reply_to.iter().chain(references.iter().rev()) {
        if let Some(k) = known(parent) {
            return k;
        }
    }
    let root = references.first().or(in_reply_to.first()).cloned().or_else(|| message_id.map(str::to_string));
    match root {
        Some(id) => format!("id:{id}"),
        None => format!("lone:{}", std::process::id()),
    }
}

/// "Re: Fwd: RE: Lunch" → "lunch", for matching subjects.
pub fn base_subject(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        let cut = ["re:", "fwd:", "fw:", "aw:", "wg:", "sv:", "vs:"].iter().find(|p| lower.starts_with(*p)).map(|p| p.len());
        match cut {
            Some(n) => s = s[n..].trim_start(),
            None => break,
        }
    }
    s.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ids() {
        assert_eq!(normalize(" <ABC@Host> "), Some("abc@host".into()));
        assert_eq!(ids("<a@x> <b@x>\r\n <a@x>"), vec!["a@x", "b@x"]);
        assert_eq!(ids("a@x b@x"), vec!["a@x", "b@x"]);
        assert!(ids("").is_empty());
    }

    #[test]
    fn keys_follow_parents() {
        let none = |_: &str| None;
        assert_eq!(key(Some("m@x"), &[], &[], None, none), "id:m@x");
        assert_eq!(key(Some("r@x"), &["m@x".into()], &["root@x".into(), "m@x".into()], None, none), "id:root@x");
        // A reply without References joins its stored parent's conversation.
        let known = |id: &str| (id == "m@x").then(|| "id:root@x".to_string());
        assert_eq!(key(Some("r2@x"), &["m@x".into()], &[], None, known), "id:root@x");
        assert_eq!(key(Some("g@x"), &[], &[], Some(42), none), "gm:42");
    }

    #[test]
    fn subjects() {
        assert_eq!(base_subject("Re: Fwd: RE:  Lunch Plans"), "lunch plans");
        assert_eq!(base_subject("Report"), "report");
    }
}
