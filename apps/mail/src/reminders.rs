//! "Add to Reminders": a reminder about a conversation, made through Nixe
//! Reminders' control interface (`reminders.create`) with a `nixe-mail:` link
//! back to the message, which Reminders' "From" button opens. Reminders is
//! started when it isn't running: its interface is on its primary instance, and
//! takes reminders once the task lists have loaded.

use crate::window;
use chrono::{Datelike, Duration, Local, Timelike};
use gtk::prelude::*;
use gtk::{gio, glib};
use serde_json::{Value, json};

const NAME: &str = "io.github.design_nexus.nixe.Reminders";
const PATH: &str = "/io/github/design_nexus/nixe/Reminders";
const INTERFACE: &str = "io.github.design_nexus.nixe.Control";
/// Retries while a started Reminders loads (every 500 ms, up to 15 s).
const TRIES: u32 = 30;

/// The link a reminder keeps back to a message.
pub fn link(id: i64) -> String {
    format!("nixe-mail:message/{id}")
}

/// The message a `nixe-mail:` link names.
pub fn message_of(link: &str) -> Option<i64> {
    link.strip_prefix("nixe-mail:")?.trim_start_matches('/').strip_prefix("message/")?.parse().ok()
}

/// When a reminder can be due: (label, due as Reminders reads it). A date
/// alone gets Reminders' day alert.
pub fn times() -> Vec<(String, Option<String>)> {
    let now = Local::now().naive_local();
    let today = now.date();
    let mut out = Vec::new();
    let later = (now + Duration::hours(3)).with_minute(0).unwrap_or(now);
    if later.date() == today && later.hour() <= 21 {
        out.push((format!("Later today · {}", later.format("%H:%M")), Some(later.format("%Y-%m-%dT%H:%M").to_string())));
    }
    out.push(("Tomorrow".into(), Some((today + Duration::days(1)).to_string())));
    // Monday to Thursday (on Friday, Saturday is tomorrow).
    let weekday = today.weekday().num_days_from_monday();
    if weekday < 4 {
        out.push(("This weekend".into(), Some((today + Duration::days(5 - weekday as i64)).to_string())));
    }
    let monday = today + Duration::days((7 - today.weekday().num_days_from_monday()) as i64);
    out.push((format!("Next week · {}", monday.format("%a %b %-d")), Some(monday.to_string())));
    out.push(("No date".into(), None));
    out
}

/// The title for a message's reminder: its subject without Re:/Fwd:.
pub fn title(subject: &str, from: &str) -> String {
    let mut s = subject.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        let Some(rest) = ["re:", "fwd:", "fw:"].iter().find_map(|p| lower.starts_with(p).then(|| &s[p.len()..])) else { break };
        s = rest.trim_start();
    }
    if s.is_empty() { format!("Email from {from}") } else { s.to_string() }
}

enum Fail {
    /// Reminders isn't on the bus.
    Absent(String),
    /// Reminders answered with an error.
    Said(String),
}

fn call(tool: &str, args: &Value, done: impl FnOnce(Result<Value, Fail>) + 'static) {
    let bus = match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
        Ok(bus) => bus,
        Err(e) => return done(Err(Fail::Absent(e.to_string()))),
    };
    bus.call(
        Some(NAME),
        PATH,
        INTERFACE,
        "Call",
        Some(&(tool, args.to_string()).to_variant()),
        Some(glib::VariantTy::new("(s)").expect("valid type")),
        gio::DBusCallFlags::NO_AUTO_START,
        10_000,
        gio::Cancellable::NONE,
        move |result| {
            let result = match result {
                Err(e) => Err(Fail::Absent(e.to_string())),
                Ok(v) => {
                    let reply: Value = v.get::<(String,)>().and_then(|(t,)| serde_json::from_str(&t).ok()).unwrap_or_default();
                    match reply.get("ok") {
                        Some(ok) => Ok(ok.clone()),
                        None => Err(Fail::Said(reply["error"].as_str().unwrap_or("Reminders didn't answer.").to_string())),
                    }
                }
            };
            done(result)
        },
    );
}

/// Starts Reminders (it opens its window); false when it isn't installed.
fn start() -> bool {
    let local = glib::home_dir().join(".local/bin/nixe-reminders");
    for program in [std::path::PathBuf::from("nixe-reminders"), local] {
        if let Ok(mut child) = std::process::Command::new(&program).spawn() {
            std::thread::spawn(move || child.wait());
            return true;
        }
    }
    false
}

/// Adds a reminder about message `id`.
pub fn add(title: String, notes: String, due: Option<String>, id: i64) {
    let mut args = json!({"title": title, "notes": notes, "source": link(id)});
    if let Some(due) = due {
        args["due"] = json!(due);
    }
    attempt(args, 0, false);
}

fn attempt(args: Value, tries: u32, started: bool) {
    let again = args.clone();
    let retry = move |started: bool| {
        glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || attempt(again, tries + 1, started));
    };
    call("reminders.create", &args, move |result| match result {
        Ok(made) => {
            let (list, uid) = (made["list"].clone(), made["uid"].clone());
            window::toast_undo("Added to Reminders.", move || {
                call("reminders.delete", &json!({"list": list, "uid": uid}), |r| {
                    if let Err(Fail::Absent(e) | Fail::Said(e)) = r {
                        window::toast(&e);
                    }
                })
            });
        }
        Err(Fail::Absent(_)) if !started => {
            if start() {
                retry(true);
            } else {
                window::toast("Reminders isn't installed.");
            }
        }
        // A starting Reminders isn't on the bus yet, or hasn't loaded its lists.
        Err(_) if started && tries < TRIES => retry(true),
        Err(Fail::Absent(_)) => window::toast("Reminders didn't start, so the reminder wasn't added."),
        Err(Fail::Said(e)) => window::toast(&format!("Reminders couldn't add it: {e}")),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_round_trip() {
        assert_eq!(message_of(&link(42)), Some(42));
        assert_eq!(message_of("nixe-mail://message/7"), Some(7));
        assert_eq!(message_of("nixe-mail:thread/7"), None);
        assert_eq!(message_of("mailto:a@b"), None);
    }

    #[test]
    fn titles_drop_reply_prefixes() {
        assert_eq!(title("Re: Fwd: Lunch", "Sam"), "Lunch");
        assert_eq!(title("RE:  Budget", "Sam"), "Budget");
        assert_eq!(title("  ", "Sam"), "Email from Sam");
        assert_eq!(title("Rebate offer", "Sam"), "Rebate offer");
    }
}
