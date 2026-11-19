//! Keeps secrets, and with `--redact` personal data, out of everything the
//! tool prints.
//!
//! Two jobs share one place so that stdout, stderr and the log all go
//! through the same filter:
//!
//! * **Secrets** (the password, the PIN, the session tokens) are replaced by
//!   `<redacted>` wherever they appear in text, as a backstop in case a
//!   library ever echoes one.
//! * **Redaction** (`--redact`) masks VINs, coordinates, e-mail addresses
//!   and plates, so a dump can be pasted into a bug report. It works on the
//!   JSON by key (latitude, longitude, plate) and on all text by pattern
//!   (VIN-shaped words, e-mail addresses) and by value (anything learned
//!   from the data that was printed).

use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use openbyd_protocol::redact::redact_for_log_with;
use serde_json::{Map, Value};

/// What a hidden secret becomes.
pub const HIDDEN: &str = "<redacted>";

/// Secrets shorter than this are not scrubbed from text: they would match
/// ordinary words and damage the output.
const MIN_SECRET_LEN: usize = 4;

/// Coordinates are only scrubbed from free text when their rendering is at
/// least this long (`52.0` would match inside other numbers).
const MIN_COORDINATE_LEN: usize = 6;

#[derive(Default)]
struct Inner {
    secrets: Vec<String>,
    redact: bool,
    vins: Vec<String>,
    /// Needles learned from the data and what replaces them.
    literals: Vec<(String, &'static str)>,
}

/// The filter. One per process, reached through [`global`]; tests make their own.
#[derive(Default)]
pub struct Scrubber {
    inner: Mutex<Inner>,
}

/// The process-wide scrubber that stdout, stderr and the log go through.
pub fn global() -> &'static Scrubber {
    static GLOBAL: OnceLock<Scrubber> = OnceLock::new();
    GLOBAL.get_or_init(Scrubber::default)
}

fn is_coordinate_key(key: &str) -> bool {
    matches!(
        normalise(key).as_str(),
        "latitude" | "longitude" | "lat" | "lng" | "lon" | "long"
    )
}

fn is_plate_key(key: &str) -> bool {
    matches!(
        normalise(key).as_str(),
        "autoplate" | "plate" | "plateno" | "platenumber" | "licenseplate" | "licenceplate"
    )
}

fn is_vin_key(key: &str) -> bool {
    normalise(key) == "vin"
}

fn normalise(key: &str) -> String {
    key.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

impl Scrubber {
    fn state(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers a secret to hide from all output.
    pub fn add_secret(&self, secret: &str) {
        if secret.len() >= MIN_SECRET_LEN {
            let mut state = self.state();
            if !state.secrets.iter().any(|known| known == secret) {
                state.secrets.push(secret.to_owned());
            }
        }
    }

    /// Turns `--redact` on or off.
    pub fn set_redact(&self, on: bool) {
        self.state().redact = on;
    }

    /// Whether `--redact` is on.
    pub fn redacting(&self) -> bool {
        self.state().redact
    }

    /// Registers a VIN or e-mail address the user supplied, so it is masked
    /// even where it does not look like one.
    pub fn add_identity(&self, text: &str, email: bool) {
        if text.is_empty() {
            return;
        }
        if email {
            self.add_literal(text, "<email>");
        } else {
            self.add_vin(text);
        }
    }

    fn add_vin(&self, vin: &str) {
        let mut state = self.state();
        if !state.vins.iter().any(|known| known == vin) {
            state.vins.push(vin.to_owned());
        }
    }

    fn add_literal(&self, needle: &str, mask: &'static str) {
        let mut state = self.state();
        if !state.literals.iter().any(|(known, _)| known == needle) {
            state.literals.push((needle.to_owned(), mask));
        }
    }

    /// Learns the VINs, plates and coordinates in `value`, so that the same
    /// text is masked wherever else it appears. Does nothing without `--redact`.
    pub fn learn(&self, value: &Value) {
        if !self.redacting() {
            return;
        }
        match value {
            Value::Object(map) => {
                for (key, item) in map {
                    self.learn_entry(key, item);
                    self.learn(item);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| self.learn(item)),
            _ => {}
        }
    }

    fn learn_entry(&self, key: &str, item: &Value) {
        let rendering = match item {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            _ => return,
        };
        if is_vin_key(key) && !rendering.is_empty() {
            self.add_vin(&rendering);
        } else if is_plate_key(key) && !rendering.is_empty() {
            self.add_literal(&rendering, "<plate>");
        } else if is_coordinate_key(key) && rendering.len() >= MIN_COORDINATE_LEN {
            self.add_literal(&rendering, "<coordinate>");
        }
    }

    /// `text` with secrets hidden and, with `--redact`, personal data masked.
    pub fn scrub_text(&self, text: &str) -> String {
        let (secrets, redact, vins, literals) = {
            let state = self.state();
            (
                state.secrets.clone(),
                state.redact,
                state.vins.clone(),
                state.literals.clone(),
            )
        };
        let mut text = text.to_owned();
        for secret in &secrets {
            text = text.replace(secret.as_str(), HIDDEN);
        }
        if !redact {
            return text;
        }
        let mut literals = literals;
        literals.sort_by_key(|(needle, _)| std::cmp::Reverse(needle.len()));
        for (needle, mask) in &literals {
            text = text.replace(needle.as_str(), mask);
        }
        let mut vins = vins;
        let text = self.mask_vins(&text, &mut vins);
        mask_emails(&text)
    }

    /// Replaces each VIN by `<vin-N>`, where N numbers the distinct VINs in
    /// the order they were met, so a reader can still tell cars apart.
    fn mask_vins(&self, text: &str, known: &mut Vec<String>) -> String {
        let mut out = String::with_capacity(text.len());
        let mut word = String::new();
        let flush = |word: &mut String, out: &mut String, known: &mut Vec<String>| {
            if word.is_empty() {
                return;
            }
            let looks_like_vin = word.len() == 17
                && word.chars().any(|c| c.is_ascii_digit())
                && word.chars().any(|c| c.is_ascii_alphabetic());
            if looks_like_vin || known.iter().any(|vin| vin == word) {
                let index = match known.iter().position(|vin| vin == word) {
                    Some(index) => index,
                    None => {
                        known.push(word.clone());
                        known.len() - 1
                    }
                };
                out.push_str(&format!("<vin-{}>", index + 1));
            } else {
                out.push_str(word);
            }
            word.clear();
        };
        for c in text.chars() {
            if c.is_ascii_alphanumeric() {
                word.push(c);
            } else {
                flush(&mut word, &mut out, known);
                out.push(c);
            }
        }
        flush(&mut word, &mut out, known);
        // Keep the numbering stable across calls.
        let mut state = self.state();
        for vin in known.iter() {
            if !state.vins.iter().any(|seen| seen == vin) {
                state.vins.push(vin.clone());
            }
        }
        out
    }

    /// A copy of `value` that is safe to print: the library's own secret
    /// hiding, and with `--redact` VINs, coordinates, plates and e-mail
    /// addresses masked, keys included.
    pub fn clean_value(&self, value: &Value) -> Value {
        // Hide token-like keys without cutting any string short.
        let value = redact_for_log_with(value, i64::MAX);
        if !self.redacting() {
            return value;
        }
        self.learn(&value);
        self.mask_value(&value)
    }

    fn mask_value(&self, value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut out = Map::new();
                for (key, item) in map {
                    let masked = match item {
                        Value::Number(_) | Value::String(_) if is_coordinate_key(key) => {
                            Value::String("<coordinate>".to_owned())
                        }
                        Value::String(text) if is_plate_key(key) && !text.is_empty() => {
                            Value::String("<plate>".to_owned())
                        }
                        other => self.mask_value(other),
                    };
                    out.insert(self.scrub_text(key), masked);
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(|i| self.mask_value(i)).collect()),
            Value::String(text) => Value::String(self.scrub_text(text)),
            other => other.clone(),
        }
    }
}

/// Replaces e-mail addresses by `<email>`.
fn mask_emails(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let local = |c: char| c.is_ascii_alphanumeric() || "._%+-".contains(c);
    let domain = |c: char| c.is_ascii_alphanumeric() || ".-".contains(c);
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            let mut start = out.chars().count();
            let kept: Vec<char> = out.chars().collect();
            while start > 0 && local(kept[start - 1]) {
                start -= 1;
            }
            let mut end = i + 1;
            while end < chars.len() && domain(chars[end]) {
                end += 1;
            }
            // Trailing dots belong to the sentence, not the address.
            while end > i + 1 && chars[end - 1] == '.' {
                end -= 1;
            }
            let host: String = chars[i + 1..end].iter().collect();
            if start < kept.len() && host.contains('.') && !host.starts_with('.') {
                out = kept[..start].iter().collect();
                out.push_str("<email>");
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn redacting() -> Scrubber {
        let scrubber = Scrubber::default();
        scrubber.set_redact(true);
        scrubber
    }

    #[test]
    fn secrets_are_hidden_in_text() {
        let scrubber = Scrubber::default();
        scrubber.add_secret("hunter2hunter2");
        scrubber.add_secret("hunter2hunter2");
        scrubber.add_secret("ab");
        assert_eq!(
            scrubber.scrub_text("pw=hunter2hunter2 ab"),
            "pw=<redacted> ab"
        );
        assert!(!scrubber.redacting());
    }

    #[test]
    fn personal_data_is_kept_without_redact() {
        let scrubber = Scrubber::default();
        let value = json!({"vin": "TESTVIN0000000001", "latitude": 52.12345});
        assert_eq!(scrubber.clean_value(&value), value);
        scrubber.learn(&value);
        assert_eq!(
            scrubber.scrub_text("TESTVIN0000000001"),
            "TESTVIN0000000001"
        );
    }

    #[test]
    fn redact_masks_vins_coordinates_plates_and_emails() {
        let scrubber = redacting();
        scrubber.add_identity("TESTVIN0000000009", false);
        scrubber.add_identity("user@example.com", true);
        scrubber.add_identity("", true);
        let value = json!({
            "vin": "SHORTVIN",
            "TESTVIN0000000001": {"latitude": 52.12345, "lng": "4.56789"},
            "auto_plate": "AB 12 CD",
            "empty_plate": "",
            "plate": "",
            "note": "car AB 12 CD near 52.12345 at TESTVIN0000000009 ok TESTVIN0000000001",
            "owner": "someone.name+x@mail.example.org.",
            "token": "abc",
            "list": [1, "SHORTVIN", null],
        });
        let cleaned = scrubber.clean_value(&value);
        let text = cleaned.to_string();
        for leaked in [
            "SHORTVIN",
            "TESTVIN",
            "52.12345",
            "4.56789",
            "AB 12 CD",
            "someone.name",
            "mail.example.org",
        ] {
            assert!(!text.contains(leaked), "{leaked} leaked in {text}");
        }
        assert_eq!(cleaned["token"], json!(HIDDEN));
        assert!(text.contains("<vin-"));
        assert!(text.contains("<coordinate>"));
        assert!(text.contains("<plate>"));
        assert!(text.contains("<email>"));
        // The same car keeps the same number.
        let once = scrubber.scrub_text("TESTVIN0000000001");
        assert_eq!(once, scrubber.scrub_text("TESTVIN0000000001"));
        assert_ne!(once, scrubber.scrub_text("TESTVIN0000000009"));
    }

    #[test]
    fn emails_need_a_local_part_and_a_dotted_host() {
        assert_eq!(mask_emails("a@b.co, c@d. e@f"), "<email>, c@d. e@f");
        assert_eq!(mask_emails("@x.org and y@.org"), "@x.org and y@.org");
        assert_eq!(mask_emails("plain"), "plain");
    }

    #[test]
    fn vin_shaped_words_need_letters_and_digits() {
        let scrubber = redacting();
        assert_eq!(
            scrubber.scrub_text("12345678901234567"),
            "12345678901234567"
        );
        assert_eq!(
            scrubber.scrub_text("ABCDEFGHIJKLMNOPQ"),
            "ABCDEFGHIJKLMNOPQ"
        );
        assert_eq!(scrubber.scrub_text("(LNBXXXXXX00000001)"), "(<vin-1>)");
    }
}
