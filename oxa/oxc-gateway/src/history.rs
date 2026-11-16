//! Authenticated private history. Verification is linear in visible history and
//! native items; it never re-signs a stale or changed context.
use crate::{catalog::Model, ledger::Ledger, policy::Refusal};
use hmac::{Hmac, Mac};
use oxa_proto::{
    FrozenPrefix, ProviderHistory, Role, WireMsg, MAX_PROVIDER_HISTORY_BYTES, MAX_PROVIDER_HISTORY_ITEMS,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{self, Write};

fn mismatch() -> Refusal {
    Refusal {
        status: 400,
        code: "provider_history_mismatch",
        param: "messages",
        message: "Saved provider history does not match this request.",
    }
}
fn invalid_output() -> Refusal {
    Refusal {
        status: 502,
        code: "cloud_upstream_history_invalid",
        param: "",
        message: "The provider returned invalid or oversized private history.",
    }
}
fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}
struct Visible {
    hash: Sha256,
    buffer: Vec<u8>,
}
impl Visible {
    fn new(prefix: &FrozenPrefix) -> Self {
        let mut hash = Sha256::new();
        frame(&mut hash, b"oxa-provider-visible-v1");
        hash.update((prefix.sections.len() as u64).to_be_bytes());
        for (label, text) in &prefix.sections {
            frame(&mut hash, label.as_bytes());
            frame(&mut hash, text.as_bytes());
        }
        frame(&mut hash, prefix.tools_json.as_bytes());
        Self {
            hash,
            buffer: Vec::new(),
        }
    }
    fn append(&mut self, message: &WireMsg) {
        self.buffer.clear();
        message.write_json(&mut self.buffer);
        frame(&mut self.hash, &self.buffer);
    }
    fn digest(&self) -> [u8; 32] {
        self.hash.clone().finalize().into()
    }
}
fn scope(ledger: &Ledger, model: &Model, account: &[u8; 32]) -> Hmac<Sha256> {
    let mut hash = Sha256::new();
    for field in [
        model.info().provider.as_str(),
        model.info().id,
        model.upstream(),
        model.endpoint().as_str(),
    ] {
        frame(&mut hash, field.as_bytes());
    }
    hash.update(account);
    let mut mac = ledger.history_mac();
    mac.update(b"scope\0");
    mac.update(&hash.finalize());
    mac
}
fn prefix_mac(
    ledger: &Ledger,
    scope: &[u8; 32],
    visible: &Visible,
    items: &[Value],
) -> Result<Hmac<Sha256>, Refusal> {
    let mut mac = ledger.history_mac();
    mac.update(b"prefix-and-items\0");
    mac.update(scope);
    mac.update(&visible.digest());
    struct Sink<'a> {
        mac: &'a mut Hmac<Sha256>,
        left: usize,
    }
    impl Write for Sink<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.left {
                return Err(io::ErrorKind::FileTooLarge.into());
            }
            self.left -= bytes.len();
            self.mac.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(
        Sink {
            mac: &mut mac,
            left: MAX_PROVIDER_HISTORY_BYTES,
        },
        items,
    )
    .map_err(|_| invalid_output())?;
    Ok(mac)
}
impl Ledger {
    /// Seal only native items the provider client grants replay authority to,
    /// after a completed assistant turn. `account` is an API-key digest or a
    /// stable subscription-account digest, never a refreshed bearer token.
    pub fn seal_provider_history(
        &self,
        model: &Model,
        account: &[u8; 32],
        prefix: &FrozenPrefix,
        messages: &[WireMsg],
        assistant: &WireMsg,
        items: Vec<Value>,
    ) -> Result<ProviderHistory, Refusal> {
        if assistant.role != Role::Assistant || items.len() > MAX_PROVIDER_HISTORY_ITEMS {
            return Err(invalid_output());
        }
        let mut visible = Visible::new(prefix);
        for message in messages {
            visible.append(message);
        }
        visible.append(assistant);
        let tag = scope(self, model, account).finalize().into_bytes().into();
        let signature = prefix_mac(self, &tag, &visible, &items)?
            .finalize()
            .into_bytes()
            .into();
        ProviderHistory::new(tag, signature, items).map_err(|_| invalid_output())
    }
    /// Verify every present carrier before using any opaque native item. A
    /// changed prefix/account/route/item is an explicit refusal, not a fallback.
    /// The session layer drops carriers at an authorized context-changing rebase.
    pub fn verify_provider_history(
        &self,
        model: &Model,
        account: &[u8; 32],
        prefix: &FrozenPrefix,
        messages: &[WireMsg],
    ) -> Result<(), Refusal> {
        let expected = scope(self, model, account);
        let mut visible = Visible::new(prefix);
        for message in messages {
            visible.append(message);
            if let Some(history) = &message.provider_history {
                if message.role != Role::Assistant || expected.clone().verify_slice(history.scope()).is_err()
                {
                    return Err(mismatch());
                }
                prefix_mac(self, history.scope(), &visible, history.items())?
                    .verify_slice(history.prefix())
                    .map_err(|_| mismatch())?;
            }
        }
        Ok(())
    }
}
