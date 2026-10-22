//! Sorting mail on this computer, without moving anything on the server.
//!
//! Every message gets a kind when it arrives: **Imbox** (people writing to
//! you), **Feed** (newsletters and lists: `List-Id`, `List-Unsubscribe`, bulk
//! senders) or **Paper Trail** (receipts, orders, shipping, sign-in codes,
//! statements, and no-reply senders). A choice the user makes for a sender
//! (`senders.category`) wins over the guess.
//!
//! The **Screener** (off until switched on) holds inbox mail from people the
//! user hasn't let in yet. Anyone they've written to is let in on their own;
//! so is everyone who'd written before the Screener was switched on.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Imbox,
    Feed,
    Paper,
}

impl Kind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Imbox => "imbox",
            Self::Feed => "feed",
            Self::Paper => "paper",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        [Self::Imbox, Self::Feed, Self::Paper].into_iter().find(|k| k.id() == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Imbox => "Imbox",
            Self::Feed => "The Feed",
            Self::Paper => "Paper Trail",
        }
    }
}

/// Words in a subject that mark a transaction or an account notice.
const PAPER_SUBJECT: &[&str] = &[
    "receipt",
    "invoice",
    "your order",
    "order confirm",
    "order #",
    "order number",
    "has shipped",
    "shipped",
    "shipping",
    "out for delivery",
    "delivered",
    "delivery",
    "payment",
    "statement",
    "your bill",
    "billing",
    "refund",
    "subscription",
    "renewal",
    "password",
    "verification code",
    "verify your",
    "confirm your",
    "sign-in",
    "sign in",
    "signed in",
    "login",
    "security alert",
    "security code",
    "one-time",
    "booking",
    "reservation",
    "itinerary",
    "e-ticket",
    "boarding pass",
    "tax",
    "your account",
];

/// Local parts of machine senders.
const PAPER_SENDER: &[&str] = &[
    "noreply",
    "no-reply",
    "no_reply",
    "donotreply",
    "do-not-reply",
    "receipts",
    "receipt",
    "billing",
    "invoice",
    "invoices",
    "orders",
    "order",
    "payments",
    "shipping",
    "shipment",
    "notifications",
    "notification",
    "alerts",
    "alert",
    "security",
    "account",
    "accounts",
    "auto-confirm",
    "confirm",
    "bookings",
    "reservations",
    "mailer-daemon",
];

/// The kind of a message from what it says about itself.
pub fn classify(from: &str, subject: &str, list_id: Option<&str>, unsubscribe: Option<&str>, bulk: bool) -> Kind {
    let subject = subject.to_lowercase();
    let local = from.split('@').next().unwrap_or("").to_lowercase();
    let local = local.split('+').next().unwrap_or("");
    let transactional = PAPER_SUBJECT.iter().any(|w| subject.contains(w));
    let machine =
        PAPER_SENDER.iter().any(|w| local == *w || local.starts_with(&format!("{w}-")) || local.starts_with(&format!("{w}.")));
    let listy = list_id.is_some() || unsubscribe.is_some() || bulk;
    match (transactional, listy, machine) {
        // A receipt often carries List-Unsubscribe too; what it says wins.
        (true, _, _) => Kind::Paper,
        (false, true, _) => Kind::Feed,
        (false, false, true) => Kind::Paper,
        _ => Kind::Imbox,
    }
}

/// Which side of the Screener a conversation is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// The Screener is off, or this isn't an inbox: everything.
    Any,
    /// Senders let in (or known).
    Cleared,
    /// Waiting to be let in.
    Pending,
    /// Screened out.
    Out,
}

/// What a list shows, beyond its mailboxes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    pub kind: Option<Kind>,
    pub screen: Screen,
    /// Only snoozed conversations, instead of everything else.
    pub snoozed: bool,
}

impl Default for Filter {
    fn default() -> Self {
        Self { kind: None, screen: Screen::Any, snoozed: false }
    }
}

/// The best way to unsubscribe from a `List-Unsubscribe` header: an https
/// link, else a mailto.
pub fn unsubscribe_link(header: &str) -> Option<String> {
    let links: Vec<&str> = header.split(',').map(|l| l.trim().trim_start_matches('<').trim_end_matches('>').trim()).collect();
    links
        .iter()
        .find(|l| l.starts_with("https://"))
        .or_else(|| links.iter().find(|l| l.to_ascii_lowercase().starts_with("mailto:")))
        .map(|l| l.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(classify("sam@example.com", "Lunch on Friday?", None, None, false), Kind::Imbox);
        assert_eq!(classify("news@fieldnotes.example", "This week: trails", Some("<fn.example>"), None, false), Kind::Feed);
        assert_eq!(classify("hello@shop.example", "Spring sale", None, Some("<https://u.example>"), false), Kind::Feed);
        assert_eq!(
            classify("hello@shop.example", "Your order #1234 has shipped", None, Some("<https://u.example>"), true),
            Kind::Paper
        );
        assert_eq!(classify("noreply@bank.example", "Monthly update", None, None, false), Kind::Paper);
        assert_eq!(classify("no-reply+abc@accounts.example", "Hi", None, None, false), Kind::Paper);
        assert_eq!(classify("orders.eu@shop.example", "Thanks", None, None, false), Kind::Paper);
        assert_eq!(classify("ordering@example.com", "Thanks", None, None, false), Kind::Imbox, "whole words only");
        assert_eq!(classify("sam@example.com", "Your verification code is 123456", None, None, false), Kind::Paper);
        assert_eq!(classify("robot@ci.example", "Build failed", None, None, true), Kind::Feed);
    }

    #[test]
    fn unsubscribe_links() {
        assert_eq!(
            unsubscribe_link("<mailto:u@list.example?subject=unsub>, <https://list.example/u/1>").as_deref(),
            Some("https://list.example/u/1")
        );
        assert_eq!(unsubscribe_link("<mailto:u@list.example>").as_deref(), Some("mailto:u@list.example"));
        assert_eq!(unsubscribe_link("<http://insecure.example/u>"), None);
        assert_eq!(Kind::from_id("paper"), Some(Kind::Paper));
    }
}
