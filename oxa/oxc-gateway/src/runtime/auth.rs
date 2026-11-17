#[cfg(feature = "upstream-openai")]
use crate::openai::chatgpt_tokens::TokenRecord;
use crate::{policy::Refusal, redaction, secrets::Key};
use serde_json::Value;
use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

/// Trusted subscription configuration. The credential path is derived beside
/// the existing key file; configuration never supplies a bearer token.
#[derive(Clone)]
pub struct Subscription {
    pub refresh_endpoint: String,
    pub client_id: String,
    pub timeout: Duration,
}
impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Subscription([redacted])")
    }
}

/// Owned across provider callbacks, so observing usage or Sending never borrows
/// the Call's credential fields for the lifetime of the stream.
pub enum OpenAiAuthHandle {
    ApiKey(Arc<Key>),
    Subscription {
        token_path: PathBuf,
        expected_account_id: String,
        refresh_endpoint: String,
        client_id: String,
        timeout: Duration,
    },
}
impl fmt::Debug for OpenAiAuthHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OpenAiAuthHandle([redacted])")
    }
}
#[cfg(feature = "upstream-openai")]
impl OpenAiAuthHandle {
    pub fn as_auth(&self) -> crate::openai::client::Auth<'_> {
        use crate::openai::client::{Auth, Refresh};
        match self {
            Self::ApiKey(key) => Auth::ApiKey(key),
            Self::Subscription {
                token_path,
                expected_account_id,
                refresh_endpoint,
                client_id,
                timeout,
            } => Auth::Subscription {
                token_path,
                expected_account_id,
                refresh: Refresh {
                    endpoint: refresh_endpoint,
                    client_id,
                    timeout: *timeout,
                },
            },
        }
    }
}

pub(super) enum Credential {
    ApiKey(Arc<Key>),
    #[cfg(feature = "upstream-openai")]
    Subscription(TokenRecord),
}
impl Credential {
    pub fn contains_secret(&self, value: &Value) -> bool {
        match self {
            Self::ApiKey(key) => redaction::contains_secret(value, key.expose()),
            #[cfg(feature = "upstream-openai")]
            Self::Subscription(record) => {
                redaction::contains_secret(value, record.access_token())
                    || redaction::contains_secret(value, record.refresh_token())
            }
        }
    }
    pub fn key(&self) -> Result<Arc<Key>, Refusal> {
        match self {
            Self::ApiKey(key) => Ok(Arc::clone(key)),
            #[cfg(feature = "upstream-openai")]
            Self::Subscription(_) => Err(Refusal::credentials()),
        }
    }
    pub fn redact(&self, value: &mut Value) {
        match self {
            Self::ApiKey(key) => redaction::value(value, key),
            #[cfg(feature = "upstream-openai")]
            Self::Subscription(record) => {
                redaction::secret(value, record.access_token());
                redaction::secret(value, record.refresh_token());
            }
        }
    }
    pub fn account_scope(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let identity = match self {
            Self::ApiKey(key) => key.expose(),
            #[cfg(feature = "upstream-openai")]
            Self::Subscription(record) => record.account_id(),
        };
        Sha256::digest(identity.as_bytes()).into()
    }
}
