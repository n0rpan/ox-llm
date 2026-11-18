//! Shared value functions for the settings menus (issue #1489, design 4.2): display, typed input,
//! checks, the current value of a key and search. The TUI row, the web row and the docs table all
//! call these, so the surfaces cannot disagree.

use crate::descriptor::{descriptors, Control, Descriptor, MapKind, Options, Unit};
use crate::schema::{find_key, kind_matches, kind_name};
use crate::{Settings, FALSE_WORDS, TRUE_WORDS};
use serde_json::Value;

/// Where a value comes from. `Set` waits for `Settings::source` (A4) to tell user from project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    Set,
}

/// The value a row shows, and where it comes from.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub value: Value,
    pub source: Source,
    /// Set, and different from the default.
    pub differs: bool,
}

/// The effective default: the descriptor's JSON text, `null` when it has none.
fn eff(d: &Descriptor) -> Value {
    d.effective_default
        .and_then(|json| serde_json::from_str::<Value>(json).ok())
        .unwrap_or(Value::Null)
}

/// The value as the menus show it: `on`, `300 s`, `5 min`, `8 of 12`, `unset`, `****` for secrets.
/// An unset value shows the effective default.
pub fn display_value(d: &Descriptor, v: &Value) -> String {
    if d.secret {
        let masked = if v.is_null() { "unset" } else { "****" };
        return masked.to_string();
    }
    let v = if v.is_null() { eff(d) } else { v.clone() };
    if v.is_null() {
        return "unset".to_string();
    }
    show(d, &v)
}

fn show(d: &Descriptor, v: &Value) -> String {
    match (d.control, v) {
        (Control::Toggle | Control::TriState, Value::Bool(b)) => {
            let word = if *b { "on" } else { "off" };
            word.to_string()
        }
        (Control::Number | Control::NumberOr(_), Value::String(word)) => word.clone(),
        (Control::Number | Control::NumberOr(_), _) => match v.as_u64() {
            Some(0) if !d.zero_means.is_empty() => d.zero_means.to_string(),
            Some(n) => count_unit(d.unit, n),
            None => v.to_string(),
        },
        (Control::Decimal, _) => match (d.unit, v.as_f64()) {
            (Some(Unit::Usd), Some(x)) => format!("${x:.2}"),
            (_, Some(x)) => format!("{x}"),
            _ => v.to_string(),
        },
        (
            Control::Choice | Control::Model | Control::Theme | Control::Text | Control::Path,
            Value::String(s),
        ) => s.clone(),
        (Control::List(_), _) => v.as_array().map_or_else(
            || v.to_string(),
            |a| {
                if a.is_empty() {
                    "none".to_string()
                } else {
                    count(a.len(), "item", "items")
                }
            },
        ),
        (Control::MultiSelect(Options::Fixed(all)), _) => v
            .as_array()
            .map_or_else(|| v.to_string(), |a| format!("{} of {}", a.len(), all.len())),
        (Control::MultiSelect(_), _) => v
            .as_array()
            .map_or_else(|| v.to_string(), |a| format!("{} selected", a.len())),
        (Control::Map(_), _) => v
            .as_object()
            .map_or_else(|| v.to_string(), |m| count(m.len(), "entry", "entries")),
        (Control::Fields, _) => v
            .as_object()
            .map_or_else(|| v.to_string(), |m| count(m.len(), "field", "fields")),
        (Control::Ignored, _) => "ignored".to_string(),
        _ => v.to_string(),
    }
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn count_unit(unit: Option<Unit>, n: u64) -> String {
    match unit {
        Some(Unit::Ms) => human_ms(n),
        Some(Unit::Seconds) => format!("{n} s"),
        Some(Unit::Tokens) => format!("{n} tokens"),
        Some(Unit::Bytes) => format!("{n} bytes"),
        Some(Unit::Rows) => format!("{n} rows"),
        Some(Unit::Cells) => format!("{n} cells"),
        Some(Unit::Usd) => format!("${n}"),
        Some(Unit::Count) | None => n.to_string(),
    }
}

/// Milliseconds in the largest unit that reads well: `750 ms`, `1.5 s`, `5 min`.
fn human_ms(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms} ms"),
        1_000..=59_999 if ms % 1_000 == 0 => format!("{} s", ms / 1_000),
        1_000..=59_999 => format!("{:.1} s", ms as f64 / 1_000.0),
        _ if ms % 60_000 == 0 => format!("{} min", ms / 60_000),
        _ => format!("{:.1} min", ms as f64 / 60_000.0),
    }
}

/// Parses text typed in the TUI or sent by a form. Empty text is `null` (unset). Kind and range
/// are checked by [`check`].
pub fn parse_input(d: &Descriptor, text: &str) -> Result<Value, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(Value::Null);
    }
    let lower = t.to_ascii_lowercase();
    match d.control {
        Control::Toggle | Control::TriState => match lower.as_str() {
            w if TRUE_WORDS.contains(&w) => Ok(Value::Bool(true)),
            w if FALSE_WORDS.contains(&w) => Ok(Value::Bool(false)),
            "unset" => Ok(Value::Null),
            _ => Err("type on or off".to_string()),
        },
        Control::NumberOr(word) if lower == word.to_ascii_lowercase() => Ok(Value::String(word.to_string())),
        Control::Number | Control::NumberOr(_) => parse_number(t),
        Control::Decimal => t
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| "enter a number".to_string()),
        Control::Choice | Control::Model | Control::Theme | Control::Text | Control::Path => {
            Ok(Value::String(t.to_string()))
        }
        Control::List(_) | Control::MultiSelect(_) => Ok(Value::Array(
            t.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_string()))
                .collect(),
        )),
        Control::Map(_) | Control::Fields | Control::Raw | Control::Ignored => {
            serde_json::from_str(t).map_err(|e| format!("not valid JSON: {e}"))
        }
    }
}

fn parse_number(t: &str) -> Result<Value, String> {
    t.parse::<u64>()
        .map(Value::from)
        .map_err(|_| "enter a whole number".to_string())
}

/// `Ok(None)` when fine; `Ok(Some(note))` when outside an advised range (it saves with the note);
/// `Err` for a wrong kind or outside a hard range. `null` is always fine (unset).
pub fn check(d: &Descriptor, v: &Value) -> Result<Option<&'static str>, String> {
    if v.is_null() {
        return Ok(None);
    }
    let ok = |cond: bool, what: &str| -> Result<Option<&'static str>, String> {
        if cond {
            Ok(None)
        } else {
            Err(format!("{} expects {what}", d.label))
        }
    };
    match d.control {
        Control::Toggle | Control::TriState => ok(v.is_boolean(), "true or false"),
        Control::Number => match v.as_u64() {
            Some(n) => range(d, n as f64),
            None => ok(false, "a whole number"),
        },
        Control::NumberOr(word) => match v.as_u64() {
            Some(n) => range(d, n as f64),
            None => ok(v.as_str() == Some(word), "a whole number or the listed word"),
        },
        Control::Decimal => match v.as_f64() {
            Some(x) => range(d, x),
            None => ok(false, "a number"),
        },
        Control::Choice => match find_key(d.key) {
            Some(def) => ok(kind_matches(def.kind, v), &kind_name(def.kind)),
            None => ok(
                v.as_str()
                    .is_some_and(|s| d.choices.is_empty() || d.choices.iter().any(|(o, _)| *o == s)),
                "one of the listed options",
            ),
        },
        Control::Text | Control::Path | Control::Model | Control::Theme => ok(v.is_string(), "text"),
        Control::List(_) | Control::MultiSelect(_) => ok(
            v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
            "a list of text",
        ),
        Control::Map(kind) => ok(
            v.as_object().is_some_and(|m| {
                m.values().all(|x| match kind {
                    MapKind::Text => x.is_string(),
                    MapKind::Number => x.is_u64(),
                })
            }),
            "an object of matching values",
        ),
        Control::Fields => ok(v.is_object(), "an object"),
        Control::Raw | Control::Ignored => Ok(None),
    }
}

fn range(d: &Descriptor, x: f64) -> Result<Option<&'static str>, String> {
    let Some(r) = d.range else {
        return Ok(None);
    };
    let outside = x < r.min as f64 || r.max.is_some_and(|m| x > m as f64);
    if !outside {
        Ok(None)
    } else if !r.hard {
        Ok(Some(r.note))
    } else {
        Err(match r.max {
            Some(m) => format!("{} must be from {} to {m}", d.label, r.min),
            None => format!("{} must be at least {}", d.label, r.min),
        })
    }
}

/// What `d.key` holds in `settings`: the merged value, or the default when no layer sets it.
pub fn current(settings: &Settings, d: &Descriptor) -> Shown {
    let schema = find_key(d.key)
        .and_then(|def| serde_json::from_str::<Value>(def.default).ok())
        .unwrap_or(Value::Null);
    let default = if schema.is_null() { eff(d) } else { schema };
    match settings.get(d.key) {
        // A value injected by the defaults layer (`defaultTools`) is not an explicit setting.
        Some(v) if settings.is_set(d.key) => Shown {
            value: v.clone(),
            source: Source::Set,
            differs: *v != default,
        },
        Some(v) => Shown {
            value: v.clone(),
            source: Source::Default,
            differs: false,
        },
        None => Shown {
            value: default,
            source: Source::Default,
            differs: false,
        },
    }
}

/// Every descriptor whose label, key or help contains `query` (case-insensitive): label hits
/// first, then key, then help; display order within a rank. A blank query finds nothing.
pub fn search(query: &str) -> Vec<&'static Descriptor> {
    search_in(descriptors(), query)
}

fn search_in<'a>(table: &'a [Descriptor], query: &str) -> Vec<&'a Descriptor> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let rank = |d: &Descriptor| {
        if d.label.to_lowercase().contains(&q) {
            0
        } else if d.key.to_lowercase().contains(&q) {
            1
        } else if d.help.to_lowercase().contains(&q) {
            2
        } else {
            3
        }
    };
    let mut hits: Vec<&Descriptor> = table.iter().filter(|d| rank(*d) < 3).collect();
    hits.sort_by_key(|d| rank(*d));
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{d, Group};
    use serde_json::json;

    fn timeout() -> Descriptor {
        d(
            "oxa.bash.defaultTimeout",
            Group::Tools,
            "Bash",
            "Bash timeout",
            "Timeout of a bash call that names none.",
        )
        .number(Unit::Seconds)
        .eff("120")
        .zero("no timeout")
        .advise(0, 3600, 30, "a hung command holds the turn that long")
    }

    #[test]
    fn display_units_and_human_ms() {
        let ms = d("k", Group::Tools, "", "L", "h").number(Unit::Ms);
        assert_eq!(display_value(&ms, &json!(300_000)), "5 min");
        assert_eq!(display_value(&ms, &json!(750)), "750 ms");
        assert_eq!(display_value(&ms, &json!(1_500)), "1.5 s");
        assert_eq!(display_value(&timeout(), &json!(0)), "no timeout");
        assert_eq!(display_value(&timeout(), &json!(300)), "300 s");
        let usd = Descriptor {
            unit: Some(Unit::Usd),
            ..d("k", Group::Budgets, "", "L", "h").decimal()
        };
        assert_eq!(display_value(&usd, &json!(1.5)), "$1.50");
    }

    #[test]
    fn effective_default_shows_on_for_unset() {
        let on = d("k", Group::Tools, "", "L", "h").toggle().eff("true");
        assert_eq!(display_value(&on, &Value::Null), "on");
        assert_eq!(display_value(&on, &json!(false)), "off");
        let bare = d("k", Group::Tools, "", "L", "h").toggle();
        assert_eq!(display_value(&bare, &Value::Null), "unset");
    }

    #[test]
    fn secret_is_masked() {
        let s = d("k", Group::Lead, "", "L", "h").text().secret();
        assert_eq!(display_value(&s, &json!("sk-abc123")), "****");
        assert_eq!(display_value(&s, &Value::Null), "unset");
    }

    #[test]
    fn parse_number_or_word() {
        let w = d("k", Group::Tools, "", "L", "h").control(Control::NumberOr("none"));
        assert_eq!(parse_input(&w, "NONE"), Ok(json!("none")));
        assert_eq!(parse_input(&w, "42"), Ok(json!(42)));
        assert_eq!(parse_input(&w, "  "), Ok(Value::Null));
        assert!(parse_input(&w, "abc").is_err());
        let t = d("k", Group::Tools, "", "L", "h").toggle();
        assert_eq!(parse_input(&t, "Off"), Ok(json!(false)));
        assert!(parse_input(&t, "maybe").is_err());
    }

    #[test]
    fn advised_range_returns_note_not_error() {
        let t = timeout();
        assert_eq!(
            check(&t, &json!(3601)),
            Ok(Some("a hung command holds the turn that long"))
        );
        assert_eq!(check(&t, &json!(3600)), Ok(None));
        assert_eq!(check(&t, &Value::Null), Ok(None));
    }

    #[test]
    fn hard_range_is_error() {
        let h = d("k", Group::Safety, "", "Rows", "h")
            .number(Unit::Rows)
            .hard(1, 9, 1, "rows");
        assert!(check(&h, &json!(10)).is_err());
        assert!(check(&h, &json!(0)).is_err());
        assert_eq!(check(&h, &json!(9)), Ok(None));
        assert!(check(&h, &json!("9")).is_err());
    }

    #[test]
    fn current_reports_default_and_set() {
        // Settings files are nested objects; `Settings::get` walks the dotted key through them.
        let s = Settings::from_values(json!({ "oxa": { "test": { "k": 5 } } }), Value::Null);
        let set = d("oxa.test.k", Group::Tools, "", "L", "h").number(Unit::Count);
        let shown = current(&s, &set);
        assert_eq!(shown.value, json!(5));
        assert_eq!(shown.source, Source::Set);
        assert!(shown.differs);
        let unset = d("oxa.test.other", Group::Tools, "", "L", "h")
            .toggle()
            .eff("true");
        let shown = current(&s, &unset);
        assert_eq!(shown.value, json!(true));
        assert_eq!(shown.source, Source::Default);
        assert!(!shown.differs);
    }

    #[test]
    fn current_reports_an_injected_default_as_default() {
        // `defaultTools` comes from the injected defaults layer, so `get` finds it while no
        // settings file names it: the row shows the built-in list, not an override.
        let list = d("defaultTools", Group::Tools, "", "L", "h").text();
        let s = Settings::from_values(json!({}), Value::Null);
        let shown = current(&s, &list);
        assert_eq!(shown.source, Source::Default);
        assert!(!shown.differs);
        let s = Settings::from_values(json!({ "defaultTools": ["read"] }), Value::Null);
        let shown = current(&s, &list);
        assert_eq!(shown.source, Source::Set);
        assert_eq!(shown.value, json!(["read"]));
        assert!(shown.differs);
    }

    #[test]
    fn search_ranks_label_key_help() {
        let table = [
            d(
                "oxa.a.help_only",
                Group::Display,
                "",
                "Other",
                "matches needle here",
            ),
            d("oxa.needle.key", Group::Display, "", "Other two", "x"),
            d("oxa.b", Group::Display, "", "Needle label", "y"),
        ];
        let keys = |q: &str| -> Vec<&'static str> { search_in(&table, q).iter().map(|d| d.key).collect() };
        let want = ["oxa.b", "oxa.needle.key", "oxa.a.help_only"];
        assert_eq!(keys("needle"), want);
        assert_eq!(keys("NEEDLE"), want);
        assert!(keys("   ").is_empty());
    }
}
