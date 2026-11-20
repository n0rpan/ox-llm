//! Model rule table for OpenAI-compatible and OpenCode Go routes (FrontierHarness N3).
//!
//! Provider quirks are keyed by **model id**, never by provider URL, so they survive a proxy or a
//! gateway in front of the vendor. A rule says how the model wants its thinking switch written,
//! whether its reasoning is replayed on assistant messages, which field carries the output cap
//! and what window it has. The built-in rows are defaults. The owner overrides or extends them in
//! user settings (`oxa.cloud.model_rules`, a pattern -> fields object). There is no hard block:
//! an id with no row gets the plain behaviour oxa had before the table.
//!
//! Local models have rows too (`Route::Local`, matched by alias or family). Those rows describe
//! the engine's own wire (`chat_template_kwargs`); nothing here rewrites the local request body,
//! so the local profile's prefix bytes and step counts do not change.
use serde_json::{json, Map, Value};

/// Which kind of route asks. A row applies to one route, or to both when the owner leaves it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Cloud,
    Local,
}

/// How the thinking switch is written on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Thinking {
    /// Only the standard top-level `reasoning_effort` (the behaviour before the table).
    Effort,
    /// `"thinking":{"type":"enabled"|"disabled"}` (DeepSeek V4).
    Object,
    /// As `Object`, enabled with `"clear_thinking":false` so the vendor keeps earlier reasoning (GLM).
    ObjectKeep,
    /// `"chat_template_kwargs":{"enable_thinking":B}`, the local engine's own switch.
    Template,
}

/// What happens to `reasoning_content` on replayed assistant messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replay {
    /// Sent as the history holds it.
    AsIs,
    /// Every assistant message with tool calls carries the field (an empty string if none was kept).
    Always,
    /// Removed from every assistant message.
    Strip,
}

/// The field that carries the output cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenField {
    MaxTokens,
    MaxCompletionTokens,
}

/// The effective rule for one model id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rule {
    pub thinking: Thinking,
    pub replay: Replay,
    pub token_field: TokenField,
    pub context_window: Option<u32>,
    pub max_output: Option<u32>,
}
impl Rule {
    const PLAIN: Rule = Rule {
        thinking: Thinking::Effort,
        replay: Replay::AsIs,
        token_field: TokenField::MaxTokens,
        context_window: None,
        max_output: None,
    };
    fn merge(&mut self, patch: &Patch) {
        self.thinking = patch.thinking.unwrap_or(self.thinking);
        self.replay = patch.replay.unwrap_or(self.replay);
        self.token_field = patch.token_field.unwrap_or(self.token_field);
        self.context_window = patch.context_window.or(self.context_window);
        self.max_output = patch.max_output.or(self.max_output);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Patch {
    thinking: Option<Thinking>,
    replay: Option<Replay>,
    token_field: Option<TokenField>,
    context_window: Option<u32>,
    max_output: Option<u32>,
}
const NONE: Patch = Patch {
    thinking: None,
    replay: None,
    token_field: None,
    context_window: None,
    max_output: None,
};

struct Entry {
    pattern: &'static str,
    route: Route,
    patch: Patch,
}
/// The built-in defaults. The first matching row of the asking route wins.
const BUILTIN: &[Entry] = &[
    // GLM-5.3: keep reasoning on the vendor side; window and output from the vendor's own table.
    Entry {
        pattern: "glm-5.3*",
        route: Route::Cloud,
        patch: Patch {
            thinking: Some(Thinking::ObjectKeep),
            context_window: Some(1_000_000),
            max_output: Some(128_000),
            ..NONE
        },
    },
    // Kimi K3: reasoning must come back on assistant messages that made tool calls.
    Entry {
        pattern: "kimi-k3*",
        route: Route::Cloud,
        patch: Patch {
            replay: Some(Replay::Always),
            ..NONE
        },
    },
    // DeepSeek V4: `thinking` switch, `reasoning_content` always present on tool-call turns.
    Entry {
        pattern: "deepseek-v4*",
        route: Route::Cloud,
        patch: Patch {
            thinking: Some(Thinking::Object),
            replay: Some(Replay::Always),
            ..NONE
        },
    },
    // Local models by alias family: the engine's template switch, reasoning replayed as kept.
    Entry {
        pattern: "qwen*",
        route: Route::Local,
        patch: Patch {
            thinking: Some(Thinking::Template),
            ..NONE
        },
    },
    Entry {
        pattern: "gemma*",
        route: Route::Local,
        patch: Patch {
            thinking: Some(Thinking::Template),
            ..NONE
        },
    },
    // Any other local alias.
    Entry {
        pattern: "*",
        route: Route::Local,
        patch: Patch {
            thinking: Some(Thinking::Template),
            ..NONE
        },
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct Override {
    pattern: String,
    route: Option<Route>,
    patch: Patch,
}

/// The built-in table plus the owner's overrides.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rules {
    overrides: Vec<Override>,
}

/// An owner override that is not understood. Carries no value, only what was wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadRule(pub &'static str);

/// Case-insensitive match with `*` wildcards, against the full id and the id without a vendor
/// prefix (`zai-org/GLM-5.3` and `opencode/glm-5.3` both match `glm-5.3*`).
fn matches(pattern: &str, id: &str) -> bool {
    fn glob(pattern: &[u8], text: &[u8]) -> bool {
        let (mut p, mut t, mut star, mut mark) = (0, 0, None, 0);
        while t < text.len() {
            if p < pattern.len() && pattern[p] == b'*' {
                star = Some(p);
                mark = t;
                p += 1;
            } else if p < pattern.len() && pattern[p] == text[t] {
                p += 1;
                t += 1;
            } else if let Some(s) = star {
                p = s + 1;
                mark += 1;
                t = mark;
            } else {
                return false;
            }
        }
        pattern[p..].iter().all(|b| *b == b'*')
    }
    let id = id.to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    let bare = id.rsplit('/').next().unwrap_or(&id);
    glob(pattern.as_bytes(), id.as_bytes()) || glob(pattern.as_bytes(), bare.as_bytes())
}

impl Rules {
    /// Parse `oxa.cloud.model_rules`: `{"<pattern>":{"thinking":..,"replay":..,"token_field":..,
    /// "context_window":N,"max_output":N,"route":"cloud"|"local"}}`. Any field may be left out; it
    /// then keeps the built-in value. Unknown keys and values are refused so a typo is not silent.
    pub fn from_overrides(raw: &Value) -> Result<Self, BadRule> {
        let bad = BadRule("model_rules");
        let mut overrides = Vec::new();
        for (pattern, fields) in raw.as_object().ok_or(bad)? {
            if pattern.is_empty() || pattern.len() > 256 || pattern.chars().any(char::is_control) {
                return Err(bad);
            }
            let mut route = None;
            let mut patch = NONE;
            for (key, value) in fields.as_object().ok_or(bad)? {
                let text = value.as_str();
                let number = || {
                    value
                        .as_u64()
                        .filter(|n| *n > 0)
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or(bad)
                };
                match key.as_str() {
                    "route" => {
                        route = Some(match text {
                            Some("cloud") => Route::Cloud,
                            Some("local") => Route::Local,
                            _ => return Err(bad),
                        })
                    }
                    "thinking" => {
                        patch.thinking = Some(match text {
                            Some("effort") => Thinking::Effort,
                            Some("object") => Thinking::Object,
                            Some("object-keep") => Thinking::ObjectKeep,
                            Some("template") => Thinking::Template,
                            _ => return Err(bad),
                        })
                    }
                    "replay" => {
                        patch.replay = Some(match text {
                            Some("as-is") => Replay::AsIs,
                            Some("always") => Replay::Always,
                            Some("strip") => Replay::Strip,
                            _ => return Err(bad),
                        })
                    }
                    "token_field" => {
                        patch.token_field = Some(match text {
                            Some("max_tokens") => TokenField::MaxTokens,
                            Some("max_completion_tokens") => TokenField::MaxCompletionTokens,
                            _ => return Err(bad),
                        })
                    }
                    "context_window" => patch.context_window = Some(number()?),
                    "max_output" => patch.max_output = Some(number()?),
                    _ => return Err(bad),
                }
            }
            overrides.push(Override {
                pattern: pattern.clone(),
                route,
                patch,
            });
        }
        Ok(Self { overrides })
    }

    /// The effective rule for `id` on `route`: the plain rule, then the first built-in row of the
    /// route that matches, then every matching owner override in file order (later wins).
    pub fn resolve(&self, route: Route, id: &str) -> Rule {
        let mut rule = Rule::PLAIN;
        if let Some(entry) = BUILTIN
            .iter()
            .find(|entry| entry.route == route && matches(entry.pattern, id))
        {
            rule.merge(&entry.patch);
        }
        for over in &self.overrides {
            if over.route.is_none_or(|r| r == route) && matches(&over.pattern, id) {
                rule.merge(&over.patch);
            }
        }
        rule
    }
}

impl Rule {
    /// Rewrite an OpenAI-compatible cloud body for this rule. `effort` is the reasoning effort the
    /// caller asked for, before the route dropped a level it does not list. A field the caller
    /// already set is never replaced.
    pub fn apply(&self, body: &mut Map<String, Value>, effort: Option<&str>) {
        if self.token_field == TokenField::MaxCompletionTokens {
            if let Some(cap) = body.remove("max_tokens") {
                body.insert("max_completion_tokens".into(), cap);
            }
        }
        if let Some(effort) = effort {
            let off = effort == "none";
            match self.thinking {
                Thinking::Effort => {}
                Thinking::Object | Thinking::ObjectKeep => {
                    let mut think = json!({"type": if off { "disabled" } else { "enabled" }});
                    if self.thinking == Thinking::ObjectKeep && !off {
                        think["clear_thinking"] = Value::Bool(false);
                    }
                    body.entry("thinking").or_insert(think);
                }
                Thinking::Template => {
                    body.entry("chat_template_kwargs")
                        .or_insert(json!({"enable_thinking": !off}));
                }
            }
        }
        if self.replay != Replay::AsIs {
            if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
                for message in messages.iter_mut().filter(|m| m["role"] == "assistant") {
                    let Some(message) = message.as_object_mut() else {
                        continue;
                    };
                    if self.replay == Replay::Strip {
                        message.remove("reasoning_content");
                    } else if message
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .is_some_and(|calls| !calls.is_empty())
                        && message.get("reasoning_content").is_none_or(Value::is_null)
                    {
                        message.insert("reasoning_content".into(), Value::String(String::new()));
                    }
                }
            }
        }
    }
}
