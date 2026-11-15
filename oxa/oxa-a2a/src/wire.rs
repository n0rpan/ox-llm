//! Wire objects for the initial text/data-only profile. Unknown optional object
//! fields are tolerated; unknown discriminants are rejected, never reinterpreted.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const PROTOCOL_VERSION: &str = "0.3.0";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    Submitted,
    Working,
    InputRequired,
    AuthRequired,
    Completed,
    Failed,
    Canceled,
    Rejected,
    Unknown,
}

impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Part {
    Text { text: String },
    Data { data: BTreeMap<String, Value> },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Agent,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub message_id: String,
    pub role: Role,
    pub parts: Vec<Part>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskStatus {
    pub state: TaskState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<MessageObject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

// Tagged wrappers ensure required kind constants are checked on deserialization
// without adding a caller-controlled string field to each payload.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum MessageObject {
    #[serde(rename = "message")]
    Message(Message),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub artifact_id: String,
    pub parts: Vec<Part>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<MessageObject>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatusUpdate {
    pub task_id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(rename = "final")]
    pub final_event: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactUpdate {
    pub task_id: String,
    pub context_id: String,
    pub artifact: Artifact,
    #[serde(default)]
    pub append: bool,
    #[serde(default)]
    pub last_chunk: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum Event {
    #[serde(rename = "message")]
    Message(Message),
    #[serde(rename = "task")]
    Task(Task),
    #[serde(rename = "status-update")]
    Status(StatusUpdate),
    #[serde(rename = "artifact-update")]
    Artifact(ArtifactUpdate),
}

impl Event {
    /// Reject cross-task/context updates before they reach a task accumulator.
    pub fn belongs_to(&self, task_id: &str, context_id: &str) -> bool {
        match self {
            Self::Task(t) => t.id == task_id && t.context_id == context_id,
            Self::Status(e) => e.task_id == task_id && e.context_id == context_id,
            Self::Artifact(e) => e.task_id == task_id && e.context_id == context_id,
            Self::Message(m) => {
                m.task_id.as_deref() == Some(task_id) && m.context_id.as_deref() == Some(context_id)
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SendParams {
    pub message: MessageObject,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskParams {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RequestId {
    Text(String),
    Number(i64),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    pub params: Value,
}

impl Request {
    pub fn new(id: RequestId, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.jsonrpc != "2.0" {
            return Err("unsupported JSON-RPC version");
        }
        if !self.params.is_object() {
            return Err("expected object params");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Checked envelope: exactly one of result/error, the expected ID and version.
/// Parse this before decoding result into an Event or a Task.
pub fn response_result(value: Value, expected: &RequestId) -> Result<Value, RpcError> {
    let invalid = || RpcError {
        code: -32603,
        message: "invalid agent response".into(),
        data: None,
    };
    let obj = value.as_object().ok_or_else(invalid)?;
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || obj.get("id") != Some(&serde_json::to_value(expected).map_err(|_| invalid())?)
        || obj.contains_key("result") == obj.contains_key("error")
    {
        return Err(invalid());
    }
    if let Some(error) = obj.get("error") {
        return Err(serde_json::from_value(error.clone()).map_err(|_| invalid())?);
    }
    Ok(obj["result"].clone())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    pub protocol_version: String,
    pub name: String,
    pub description: String,
    pub url: String,
    pub version: String,
    #[serde(default = "jsonrpc_transport")]
    pub preferred_transport: String,
    pub capabilities: Capabilities,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
    pub skills: Vec<Skill>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub security_schemes: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security: Vec<BTreeMap<String, Vec<String>>>,
}

fn jsonrpc_transport() -> String {
    "JSONRPC".into()
}

impl AgentCard {
    /// This profile never follows an endpoint supplied by an untrusted card.
    pub fn validate_for(&self, registered_url: &str) -> Result<(), &'static str> {
        if self.protocol_version != PROTOCOL_VERSION || self.preferred_transport != "JSONRPC" {
            return Err("unsupported protocol or transport");
        }
        if self.url != registered_url {
            return Err("agent endpoint differs from registration");
        }
        if self.capabilities.extensions.iter().any(|e| e.required) {
            return Err("unsupported required extension");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub push_notifications: bool,
    #[serde(default)]
    pub extensions: Vec<Extension>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Extension {
    pub uri: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
}
