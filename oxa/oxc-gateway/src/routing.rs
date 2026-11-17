//! O-384: OpenRouter provider routing preferences from user settings.
//!
//! oxa enforces no provider policy of its own. With no settings the request carries
//! no `provider` object at all, so the OpenRouter account/workspace settings apply as
//! the user configured them. Every configured preference is passed through exactly as
//! written; an invalid one is refused while the settings are read, never at request time.
use serde_json::{json, Map, Value};
use std::fmt;

/// `data_collection` of the OpenRouter `provider` object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataCollection {
    Allow,
    Deny,
}
impl DataCollection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// `sort` of the OpenRouter `provider` object. `:nitro` and `:floor` are OpenRouter's
/// documented shortcuts for `throughput` and `price`; both spellings are accepted and
/// sent as the canonical value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Price,
    Throughput,
    Latency,
}
impl Sort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Price => "price",
            Self::Throughput => "throughput",
            Self::Latency => "latency",
        }
    }
}

/// One refused preference: the offending field and why. The message names the field so
/// a settings typo is fixable without reading the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub field: String,
    pub reason: String,
}
impl Error {
    fn new(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            reason: reason.into(),
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.field.is_empty() {
            write!(f, "provider_routing: {}", self.reason)
        } else {
            write!(f, "provider_routing.{}: {}", self.field, self.reason)
        }
    }
}
impl std::error::Error for Error {}

/// The OpenRouter `provider` object a route sends. Every field is optional: `None` means
/// the user expressed no preference and the field is not sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderRouting {
    pub zdr: Option<bool>,
    pub data_collection: Option<DataCollection>,
    pub sort: Option<Sort>,
    pub order: Option<Vec<String>>,
    pub only: Option<Vec<String>>,
    pub ignore: Option<Vec<String>>,
    pub allow_fallbacks: Option<bool>,
}

impl ProviderRouting {
    /// Parse one `provider_routing` object. Unknown keys, wrong types, empty or repeated
    /// provider lists and `only` together with `ignore` are refused.
    pub fn parse(value: &Value) -> Result<Self, Error> {
        let object = value
            .as_object()
            .ok_or_else(|| Error::new("", "must be an object"))?;
        let mut routing = Self::default();
        for (key, value) in object {
            match key.as_str() {
                "zdr" => {
                    routing.zdr = Some(
                        value
                            .as_bool()
                            .ok_or_else(|| Error::new("zdr", "must be true or false"))?,
                    )
                }
                "data_collection" => {
                    routing.data_collection = Some(match value.as_str() {
                        Some("allow") => DataCollection::Allow,
                        Some("deny") => DataCollection::Deny,
                        _ => return Err(Error::new("data_collection", "must be \"allow\" or \"deny\"")),
                    })
                }
                "sort" => routing.sort = Some(match value.as_str() {
                    Some("price" | ":floor" | "floor") => Sort::Price,
                    Some("throughput" | ":nitro" | "nitro") => Sort::Throughput,
                    Some("latency") => Sort::Latency,
                    _ => return Err(Error::new(
                        "sort",
                        "must be \"price\", \"throughput\" or \"latency\" (or the :floor / :nitro shortcut)",
                    )),
                }),
                "order" => routing.order = Some(providers(value, "order")?),
                "only" => routing.only = Some(providers(value, "only")?),
                "ignore" => routing.ignore = Some(providers(value, "ignore")?),
                "allow_fallbacks" => {
                    routing.allow_fallbacks = Some(
                        value
                            .as_bool()
                            .ok_or_else(|| Error::new("allow_fallbacks", "must be true or false"))?,
                    )
                }
                other => return Err(Error::new(other, "is not an OpenRouter provider preference")),
            }
        }
        if routing.only.is_some() && routing.ignore.is_some() {
            return Err(Error::new("only", "cannot be combined with `ignore`"));
        }
        Ok(routing)
    }

    /// `true` when the user expressed no preference: the request then carries no
    /// `provider` object.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// The `provider` object to send, or `None` when nothing is configured.
    pub fn to_json(&self) -> Option<Value> {
        if self.is_empty() {
            return None;
        }
        let mut object = Map::new();
        if let Some(zdr) = self.zdr {
            object.insert("zdr".to_string(), json!(zdr));
        }
        if let Some(data_collection) = self.data_collection {
            object.insert("data_collection".to_string(), json!(data_collection.as_str()));
        }
        if let Some(sort) = self.sort {
            object.insert("sort".to_string(), json!(sort.as_str()));
        }
        for (key, list) in [
            ("order", &self.order),
            ("only", &self.only),
            ("ignore", &self.ignore),
        ] {
            if let Some(list) = list {
                object.insert(key.to_string(), json!(list));
            }
        }
        if let Some(allow_fallbacks) = self.allow_fallbacks {
            object.insert("allow_fallbacks".to_string(), json!(allow_fallbacks));
        }
        Some(Value::Object(object))
    }

    /// The active preferences as one compact cell of the model view; `-` when none.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(zdr) = self.zdr {
            parts.push(format!("zdr={zdr}"));
        }
        if let Some(data_collection) = self.data_collection {
            parts.push(format!("data={}", data_collection.as_str()));
        }
        if let Some(sort) = self.sort {
            parts.push(format!("sort={}", sort.as_str()));
        }
        for (key, list) in [
            ("order", &self.order),
            ("only", &self.only),
            ("ignore", &self.ignore),
        ] {
            if let Some(list) = list {
                parts.push(format!("{key}={}", list.join("|")));
            }
        }
        if let Some(allow_fallbacks) = self.allow_fallbacks {
            parts.push(format!("fallbacks={allow_fallbacks}"));
        }
        if parts.is_empty() {
            "-".to_string()
        } else {
            parts.join(",")
        }
    }

    /// The per-model preferences on top of the global ones: every field the model sets
    /// wins. A model that sets `only` or `ignore` replaces the global pair, so the
    /// mutually exclusive lists can never be combined by the merge.
    pub fn overridden_by(&self, model: &Self) -> Self {
        let lists = model.only.is_some() || model.ignore.is_some();
        Self {
            zdr: model.zdr.or(self.zdr),
            data_collection: model.data_collection.or(self.data_collection),
            sort: model.sort.or(self.sort),
            order: model.order.clone().or_else(|| self.order.clone()),
            only: if lists {
                model.only.clone()
            } else {
                self.only.clone()
            },
            ignore: if lists {
                model.ignore.clone()
            } else {
                self.ignore.clone()
            },
            allow_fallbacks: model.allow_fallbacks.or(self.allow_fallbacks),
        }
    }
}

/// One provider list (`order`, `only`, `ignore`): non-empty, unique, printable slugs.
fn providers(value: &Value, field: &str) -> Result<Vec<String>, Error> {
    let list = value
        .as_array()
        .ok_or_else(|| Error::new(field, "must be a list of provider names"))?;
    if list.is_empty() {
        return Err(Error::new(field, "must name at least one provider"));
    }
    let mut names: Vec<String> = Vec::with_capacity(list.len());
    for name in list {
        let name = name
            .as_str()
            .ok_or_else(|| Error::new(field, "must be a list of provider names"))?;
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
        {
            return Err(Error::new(
                field,
                "names must be 1-64 characters of letters, digits, `.`, `_`, `-` or `/`",
            ));
        }
        if names.iter().any(|seen| seen == name) {
            return Err(Error::new(field, "must not repeat a provider"));
        }
        names.push(name.to_string());
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_preferences_send_nothing() {
        let routing = ProviderRouting::parse(&json!({})).unwrap();
        assert!(routing.is_empty());
        assert_eq!(routing.to_json(), None);
        assert_eq!(routing.describe(), "-");
        assert!(ProviderRouting::default().is_empty());
        assert_eq!(ProviderRouting::default().to_json(), None);
    }

    #[test]
    fn every_preference_is_passed_through_exactly() {
        let routing = ProviderRouting::parse(&json!({
            "zdr": true,
            "data_collection": "deny",
            "sort": "latency",
            "order": ["synthetic-a", "synthetic-b"],
            "ignore": ["synthetic-c"],
            "allow_fallbacks": false
        }))
        .unwrap();
        assert_eq!(
            routing.to_json().unwrap(),
            json!({"zdr":true,"data_collection":"deny","sort":"latency",
                "order":["synthetic-a","synthetic-b"],"ignore":["synthetic-c"],
                "allow_fallbacks":false})
        );
        assert_eq!(
            routing.describe(),
            "zdr=true,data=deny,sort=latency,order=synthetic-a|synthetic-b,ignore=synthetic-c,fallbacks=false"
        );
        // `only` is the other half of the exclusive pair.
        let only = ProviderRouting::parse(&json!({"only":["synthetic-a"]})).unwrap();
        assert_eq!(only.to_json().unwrap(), json!({"only":["synthetic-a"]}));
        assert_eq!(only.describe(), "only=synthetic-a");
    }

    #[test]
    fn sort_shortcuts_map_to_their_canonical_value() {
        for (written, canonical) in [
            (":nitro", "throughput"),
            ("nitro", "throughput"),
            ("throughput", "throughput"),
            (":floor", "price"),
            ("floor", "price"),
            ("price", "price"),
            ("latency", "latency"),
        ] {
            let routing = ProviderRouting::parse(&json!({"sort":written})).unwrap();
            assert_eq!(routing.to_json().unwrap(), json!({"sort":canonical}), "{written}");
        }
    }

    #[test]
    fn invalid_values_are_refused_with_the_field_named() {
        for (value, field) in [
            (json!([]), ""),
            (json!({"zdr":"yes"}), "zdr"),
            (json!({"data_collection":"maybe"}), "data_collection"),
            (json!({"data_collection":true}), "data_collection"),
            (json!({"sort":"cheapest"}), "sort"),
            (json!({"sort":1}), "sort"),
            (json!({"order":"synthetic-a"}), "order"),
            (json!({"order":[]}), "order"),
            (json!({"order":["synthetic-a","synthetic-a"]}), "order"),
            (json!({"order":["synthetic a"]}), "order"),
            (json!({"order":[""]}), "order"),
            (json!({"order":[7]}), "order"),
            (json!({"only":["synthetic-a"],"ignore":["synthetic-b"]}), "only"),
            (json!({"allow_fallbacks":"no"}), "allow_fallbacks"),
            (json!({"zdrr":true}), "zdrr"),
        ] {
            let error = ProviderRouting::parse(&value).unwrap_err();
            assert_eq!(error.field, field, "{value}");
            assert!(error.to_string().starts_with("provider_routing"), "{error}");
        }
    }

    #[test]
    fn per_model_fields_override_the_global_ones() {
        let global = ProviderRouting::parse(&json!({
            "zdr": true, "data_collection": "deny", "sort": "price",
            "only": ["synthetic-a"], "allow_fallbacks": false
        }))
        .unwrap();
        // A model that sets nothing keeps the global preferences.
        assert_eq!(global.overridden_by(&ProviderRouting::default()), global);
        // A model that sets one field keeps the others.
        let merged = global.overridden_by(&ProviderRouting::parse(&json!({"sort":"latency"})).unwrap());
        assert_eq!(merged.zdr, Some(true));
        assert_eq!(merged.data_collection, Some(DataCollection::Deny));
        assert_eq!(merged.sort, Some(Sort::Latency));
        assert_eq!(merged.only, Some(vec!["synthetic-a".to_string()]));
        assert_eq!(merged.allow_fallbacks, Some(false));
        // Setting `ignore` replaces the global `only` instead of combining them.
        let merged =
            global.overridden_by(&ProviderRouting::parse(&json!({"ignore":["synthetic-b"]})).unwrap());
        assert_eq!(merged.only, None);
        assert_eq!(merged.ignore, Some(vec!["synthetic-b".to_string()]));
        assert!(ProviderRouting::parse(&merged.to_json().unwrap()).is_ok());
    }
}
