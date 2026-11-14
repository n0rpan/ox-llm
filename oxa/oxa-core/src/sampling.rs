//! Request sampling (`oxa.sampling.mode`): greedy (W6, the default) or the model card's values.
//!
//! `greedy` sends `"temperature":0` and nothing else, as every request did before this setting:
//! deterministic, and the only mode in which the engine runs MTP speculation
//! (`engine/crates/server/src/generate.rs`, "MTP speculation (greedy only)"). `model` sends the
//! sampling the model's card recommends for the request's thinking state, for the models this
//! table knows; any other model stays greedy. Only chat and subagent replies are sampled:
//! summaries and prefills stay greedy.
//!
//! The Qwen3.6 card (also the base of Ornith) recommends, for thinking on and precise coding:
//! `temperature=0.6, top_p=0.95, top_k=20, min_p=0, presence_penalty=0`; for thinking off:
//! `temperature=0.7, top_p=0.8, top_k=20, min_p=0, presence_penalty=1.5`. The card warns that
//! greedy decoding can repeat endlessly. `oxa.sampling.seed` fixes the seed (tests).
use crate::SessionState;
use oxa_proto::Sampling;

/// The mode setting: `"greedy"` (default) or `"model"`.
pub const SETTING_MODE: &str = "oxa.sampling.mode";
/// A fixed seed sent with sampled requests; unset = the engine's own.
pub const SETTING_SEED: &str = "oxa.sampling.seed";

/// The sampling of a chat or subagent request with thinking `thinking`; `None` = greedy.
pub(crate) fn for_request(st: &SessionState, thinking: bool) -> Option<Sampling> {
    if st.settings.string(SETTING_MODE).as_deref() != Some("model") {
        return None;
    }
    let seed = st.settings.get(SETTING_SEED).and_then(oxa_proto::Value::as_u64);
    card(&st.model, thinking).map(|s| Sampling { seed, ..s })
}

/// The card values for `model` (matched on its id, case-insensitive); `None` = not known.
pub fn card(model: &str, thinking: bool) -> Option<Sampling> {
    let id = model.to_ascii_lowercase();
    if !(id.contains("qwen3") || id.contains("ornith")) {
        return None;
    }
    Some(if thinking {
        Sampling {
            temperature: 0.6,
            top_p: 0.95,
            top_k: 20,
            min_p: 0.0,
            presence_penalty: 0.0,
            seed: None,
        }
    } else {
        Sampling {
            temperature: 0.7,
            top_p: 0.8,
            top_k: 20,
            min_p: 0.0,
            presence_penalty: 1.5,
            seed: None,
        }
    })
}
