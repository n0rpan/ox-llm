//! Process-shared reservations and settlement fields. A hold remains reserved until
//! its row is written to the ledger; write failures stop spending.
use crate::{
    billing::{Billing, Counts, SubscriptionLimits},
    policy::{self, Refusal},
    pricing::Cache,
    reservations::Shared,
    spend::SpendReader,
};
use serde_json::{json, Value};
use std::{
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct Model {
    pub provider: String,
    pub upstream: String,
    pub context: u64,
    pub max_output: u64,
    pub billing: Billing,
}
#[derive(Default)]
struct State {
    reader: SpendReader,
    failed: bool,
}
#[derive(Clone)]
pub struct Budget {
    path: PathBuf,
    state: Arc<Mutex<State>>,
}
pub struct Hold {
    budget: Budget,
    model: Model,
    reserved: f64,
    reserved_tokens: u64,
    output: u64,
    forwarded: bool,
    settled: bool,
    id: String,
}
#[derive(Clone, Default, Debug)]
pub struct Usage {
    pub prompt: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub invalid_cache: bool,
    pub reported_cost: Option<f64>,
    pub finish: Option<String>,
}
#[derive(Clone, Copy)]
pub enum Outcome {
    Complete,
    /// The provider explicitly refused before generation.
    Refused,
    Failed,
    Cancelled {
        request_bytes: usize,
        streamed_bytes: usize,
        usage_final: bool,
        /// Include time spent generating hidden reasoning, not just wire bytes.
        elapsed: Duration,
    },
}
impl Usage {
    fn total_tokens(&self) -> Option<u64> {
        let (prompt, output) = self.prompt.zip(self.output)?;
        let cached = self
            .cache_read
            .unwrap_or(0)
            .checked_add(self.cache_write.unwrap_or(0))?;
        if self.invalid_cache || cached > prompt {
            return None;
        }
        prompt.checked_add(output)
    }
    /// Consume canonical OpenAI usage after provider translation. Anthropic
    /// translators must include cache reads/writes in prompt and keep details.
    pub fn observe(&mut self, value: &Value) {
        let usage = &value["usage"];
        for (field, target) in [
            ("prompt_tokens", &mut self.prompt),
            ("completion_tokens", &mut self.output),
        ] {
            if let Some(number) = usage[field].as_u64() {
                *target = Some(number);
            }
        }
        for (field, target) in [
            ("cached_tokens", &mut self.cache_read),
            ("cache_write_tokens", &mut self.cache_write),
        ] {
            if let Some(value) = usage["prompt_tokens_details"].get(field) {
                if let Some(number) = value.as_u64() {
                    *target = Some(number);
                } else {
                    self.invalid_cache = true;
                }
            }
        }
        if let Some(cost) = usage["cost"].as_f64().filter(|cost| (0.0..=1e100).contains(cost)) {
            self.reported_cost = Some(self.reported_cost.unwrap_or(0.0).max(cost));
        }
        if let Some(finish) = value["choices"][0]["finish_reason"].as_str() {
            self.finish = Some(finish.into());
        }
    }
}
fn exhausted() -> Refusal {
    Refusal {
        status: 429,
        message: "The daily cloud budget is exhausted.",
        code: "budget_exceeded",
        param: "model",
    }
}
impl Budget {
    /// Record an unpaid refusal under the same admission lock. A failed audit
    /// closes later spending across processes without replacing its original error.
    #[cfg(feature = "anthropic-gateway")]
    pub(crate) fn write_unpaid(&self, write: impl FnOnce() -> io::Result<()>) -> Result<(), Refusal> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut shared = Shared::open(&self.path).map_err(|_| Refusal::ledger())?;
        let result = write();
        if result.is_err() {
            state.failed = true;
            shared.failed = true;
            let _ = shared.save();
        }
        result.map_err(|_| Refusal::ledger())
    }
    /// Ledger directories must already be private (0700) and owned by this user.
    /// Independent instances and processes share persisted daily reservations.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Arc::new(Mutex::new(State::default())),
        }
    }
    pub fn spend(&self) -> Result<f64, Refusal> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.failed {
            return Err(Refusal::ledger());
        }
        let shared = Shared::open(&self.path).map_err(|_| Refusal::ledger())?;
        if shared.failed {
            return Err(Refusal::ledger());
        }
        state.reader.read(&self.path).map_err(|_| Refusal::ledger())
    }
    pub fn reserve(
        &self,
        cap: &Value,
        ledger_enabled: bool,
        model: &Model,
        output: u64,
    ) -> Result<Hold, Refusal> {
        // The gateway has resolved the request's output-limit field and alias.
        // A zero limit must never become an under-reserved forwarded request.
        if output == 0 {
            return Err(Refusal {
                status: 400,
                message: "The output token limit must be a positive integer.",
                code: "invalid_value",
                param: "max_tokens",
            });
        }
        policy::daily_budget(cap, ledger_enabled, Some(0.0), 0.0, 0.0)?;
        let Billing::Metered(price) = &model.billing else {
            return Err(invalid_billing());
        };
        let output = output.min(model.max_output);
        let required = price
            .reservation(&model.provider, &model.upstream, model.context, output)
            .ok_or_else(exhausted)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.failed {
            return Err(Refusal::ledger());
        }
        let mut shared = Shared::open(&self.path).map_err(|_| Refusal::ledger())?;
        if shared.failed {
            return Err(Refusal::ledger());
        }
        let spent = state.reader.read(&self.path).map_err(|_| Refusal::ledger())?;
        policy::daily_budget(cap, ledger_enabled, Some(spent), shared.reserved(), required)?;
        let id = shared.insert(required).map_err(|_| Refusal::ledger())?;
        shared.save().map_err(|_| Refusal::ledger())?;
        Ok(Hold {
            budget: self.clone(),
            model: model.clone(),
            reserved: required,
            reserved_tokens: 0,
            output,
            forwarded: false,
            settled: false,
            id,
        })
    }
    pub fn reserve_subscription(
        &self,
        limits: Option<SubscriptionLimits>,
        ledger_enabled: bool,
        model: &Model,
        output: u64,
    ) -> Result<Hold, Refusal> {
        let limits = limits.ok_or_else(SubscriptionLimits::missing)?;
        if !ledger_enabled {
            return Err(Refusal::ledger());
        }
        if !matches!(model.billing, Billing::Subscription)
            || !matches!(model.provider.as_str(), "chatgpt" | "opencode")
        {
            return Err(invalid_billing());
        }
        if output == 0 || model.max_output == 0 || model.context == 0 {
            return Err(Refusal {
                status: 400,
                code: "invalid_value",
                param: "max_tokens",
                message: "Subscription context and output limits must be positive.",
            });
        }
        let output = output.min(model.max_output);
        let tokens = model.context.checked_add(output).ok_or_else(Refusal::ledger)?;
        let required = Counts { requests: 1, tokens };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.failed {
            return Err(Refusal::ledger());
        }
        let mut shared = Shared::open(&self.path).map_err(|_| Refusal::ledger())?;
        if shared.failed {
            return Err(Refusal::ledger());
        }
        state.reader.read(&self.path).map_err(|_| Refusal::ledger())?;
        let held = shared.subscription_reserved().map_err(|_| Refusal::ledger())?;
        limits.check(state.reader.subscription(), held, required)?;
        let id = shared
            .insert_subscription(tokens)
            .map_err(|_| Refusal::ledger())?;
        shared.save().map_err(|_| Refusal::ledger())?;
        Ok(Hold {
            budget: self.clone(),
            model: model.clone(),
            reserved: 0.0,
            reserved_tokens: tokens,
            output,
            forwarded: false,
            settled: false,
            id,
        })
    }
}
fn invalid_billing() -> Refusal {
    Refusal {
        status: 400,
        code: "invalid_billing",
        param: "model",
        message: "The provider and billing class do not match.",
    }
}
impl Hold {
    pub fn reserved(&self) -> f64 {
        self.reserved
    }
    /// On first forwarding, verify the persisted reservation before paid bytes.
    /// All holds already survive a process crash, including before forwarding.
    pub fn mark_forwarded(&mut self) -> Result<(), Refusal> {
        // Admission failures must not prevent already-paid work from settling.
        if self.forwarded {
            return Ok(());
        }
        let mut state = self.budget.state.lock().unwrap_or_else(|e| e.into_inner());
        let checked: io::Result<()> = (|| {
            let shared = Shared::open(&self.budget.path)?;
            if shared.failed || !shared.holds.contains_key(&self.id) {
                return Err(io::ErrorKind::InvalidData.into());
            }
            Ok(())
        })();
        if state.failed || checked.is_err() {
            state.failed = true;
            return Err(Refusal::ledger());
        }
        self.forwarded = true;
        Ok(())
    }
    fn record(&self, usage: &Usage, outcome: Outcome) -> Value {
        let model = &self.model;
        let Billing::Metered(price) = &model.billing else {
            return self.subscription_record(usage, outcome);
        };
        let mut row = json!({"remote":true,"remote_provider":model.provider,"remote_model":model.upstream});
        row["billing"] = json!("metered");
        if matches!(outcome, Outcome::Refused) {
            row["cost_usd"] = json!(0.0);
            row["settled"] = json!("refused");
            return row;
        }
        for (field, value) in [
            ("prompt_tokens", usage.prompt),
            ("completion_tokens", usage.output),
            ("cached_tokens", usage.cache_read),
            ("cache_write_tokens", usage.cache_write),
        ] {
            if let Some(value) = value {
                row[field] = json!(value);
            }
        }
        if let Some(finish) = &usage.finish {
            row["finish_reason"] = json!(finish);
        }
        let invalid = usage.invalid_cache
            || usage.prompt.is_some_and(|prompt| {
                usage
                    .cache_read
                    .unwrap_or(0)
                    .saturating_add(usage.cache_write.unwrap_or(0))
                    > prompt
            });
        let cost = if invalid {
            None
        } else {
            price.cost(
                &model.provider,
                &model.upstream,
                usage.prompt.unwrap_or(0),
                usage.output.unwrap_or(0),
                Cache::Tokens {
                    read: usage.cache_read.unwrap_or(0),
                    write: usage.cache_write.unwrap_or(0),
                },
            )
        };
        if let Some(cost) = cost {
            row["cost_usd"] = json!(cost);
            if model.provider != "anthropic" {
                row["catalog_cost_usd"] = json!(cost);
            }
        }
        if model.provider == "openrouter" {
            if let Some(cost) = usage.reported_cost {
                row["reported_cost_usd"] = json!(cost);
                row["cost_usd"] = json!(cost);
            }
        }
        if invalid {
            row["usage_missing"] = json!(true);
        }
        let actual = row["cost_usd"].as_f64();
        let missing = invalid || usage.prompt.is_none() || usage.output.is_none() || actual.is_none();
        match outcome {
            Outcome::Cancelled {
                request_bytes,
                streamed_bytes,
                usage_final,
                elapsed,
            } if !usage_final || usage.prompt.is_none() || usage.output.is_none() || actual.is_none() => {
                let output = self.output.min(
                    (streamed_bytes as u64)
                        .max(usage.output.unwrap_or(0))
                        .max((elapsed.as_secs_f64() * 300.0) as u64),
                );
                let estimate = price
                    .cost(
                        &model.provider,
                        &model.upstream,
                        usage.prompt.unwrap_or(request_bytes as u64),
                        output,
                        // Final cache composition is unknown. Include cache-write
                        // premiums together with hidden output in the estimate.
                        Cache::Invalid,
                    )
                    .unwrap_or(self.reserved);
                row["cost_usd"] = json!(self.reserved.min(estimate).max(actual.unwrap_or(0.0)));
                row["usage_missing"] = json!(true);
                row["settled"] = json!("estimated");
            }
            Outcome::Cancelled { .. } => {
                row["settled"] = json!("usage");
            }
            Outcome::Failed => {
                row["cost_usd"] = json!(self.reserved.max(actual.unwrap_or(0.0)));
                row["usage_missing"] = json!(true);
                row["settled"] = json!("reserved");
            }
            _ if missing => {
                row["cost_usd"] = json!(self.reserved.max(actual.unwrap_or(0.0)));
                row["usage_missing"] = json!(true);
                row["settled"] = json!("reserved");
            }
            _ => row["settled"] = json!("usage"),
        }
        row
    }
    fn subscription_record(&self, usage: &Usage, outcome: Outcome) -> Value {
        let mut row = json!({"remote":true,"remote_provider":self.model.provider,
            "remote_model":self.model.upstream,"billing":"subscription","cost_usd":0.0,
            "subscription_requests":u64::from(self.forwarded),"subscription_tokens":0});
        if !self.forwarded {
            row["settled"] = json!("not_forwarded");
            return row;
        }
        if matches!(outcome, Outcome::Refused) {
            row["settled"] = json!("refused");
            return row;
        }
        for (field, value) in [
            ("prompt_tokens", usage.prompt),
            ("completion_tokens", usage.output),
            ("cached_tokens", usage.cache_read),
            ("cache_write_tokens", usage.cache_write),
        ] {
            if let Some(value) = value {
                row[field] = json!(value);
            }
        }
        let actual = usage.total_tokens();
        let final_usage = matches!(
            outcome,
            Outcome::Complete
                | Outcome::Cancelled {
                    usage_final: true,
                    ..
                }
        );
        if let Some(actual) = actual.filter(|_| final_usage) {
            row["subscription_tokens"] = json!(actual);
            row["settled"] = json!("usage");
        } else {
            row["subscription_tokens"] = json!(self.reserved_tokens.max(actual.unwrap_or(0)));
            row["settled"] = json!("reserved");
            row["usage_missing"] = json!(true);
        }
        row
    }
    /// The writer merges these fixed remote fields with its private request
    /// metadata and durably appends one complete row to this daily ledger
    /// (for example with spend::append_private). Never put prompts/replies in it.
    pub fn finish(
        mut self,
        usage: &Usage,
        outcome: Outcome,
        write: impl FnOnce(&Value) -> io::Result<()>,
    ) -> Result<Value, Refusal> {
        let row = self.record(usage, outcome);
        let mut state = self.budget.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut shared = Shared::open(&self.budget.path).map_err(|_| Refusal::ledger())?;
        if !shared.holds.contains_key(&self.id) {
            return Err(Refusal::ledger());
        }
        let result = write(&row);
        if result.is_err() {
            state.failed = true;
            shared.failed = true;
        } else {
            shared.holds.remove(&self.id);
        }
        self.settled = true;
        if shared.save().is_err() {
            state.failed = true;
            return Err(Refusal::ledger());
        }
        result.map(|()| row).map_err(|_| Refusal::ledger())
    }
}
impl Drop for Hold {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut state = self.budget.state.lock().unwrap_or_else(|e| e.into_inner());
        // Before forwarding it is safe to release an abandoned admission. A
        // potentially paid request without a recorded settlement fails closed.
        let result = (|| {
            let mut shared = Shared::open(&self.budget.path)?;
            if self.forwarded || !shared.holds.contains_key(&self.id) {
                state.failed = true;
                shared.failed = true;
            } else {
                shared.holds.remove(&self.id);
            }
            shared.save()
        })();
        if result.is_err() {
            state.failed = true;
        }
    }
}
