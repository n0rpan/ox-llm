//! Minimal local board caller. Transport, policy, peer proof and rendering stay shared.
use crate::dispatch::LeadOwnerBinding;
use crate::lead_access::{self, proxy, Config, IdentityProvider, Mode, VerifiedIdentity};
use crate::lead_chat::conversation::ConversationReply;
use crate::lead_http::{OwnerForm, OwnerSubmission, RefusalFact, RefusalLog, Stage};
use oxa_config::{Dirs, Env, Settings};
use oxa_perm::ApprovalDesk;
use oxa_proto::{codes, OxaError};
use std::{
    io::Write,
    net::{TcpListener, TcpStream},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

fn invalid(reason: &str) -> OxaError {
    OxaError::new(codes::INVALID, reason)
}

/// The real service journal of the lead listener: one fixed, allowlisted line
/// per refused chat request, written to the process's stderr so the operator's
/// existing service unit already carries it to the system journal
/// (`journalctl --user -u oxa-lead.service`). Every line carries only the
/// response status and fixed stage/reason/fix tokens; owner identity, message
/// text, token counts, URLs, headers, keys and CSRF values are never written,
/// and no fact carries request or error bytes. There is no new file, rotation
/// or dependency, and a failed write never blocks or changes a refusal.
#[derive(Clone)]
struct ServiceJournal {
    out: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl ServiceJournal {
    /// The production sink: the process's standard error.
    fn stderr() -> Self {
        Self {
            out: Arc::new(Mutex::new(Box::new(std::io::stderr()))),
        }
    }

    fn log(&self, fact: RefusalFact) {
        let Ok(mut out) = self.out.lock() else {
            return;
        };
        let _ = writeln!(
            out,
            "lead chat refusal status={} stage={} reason={} fix={}",
            fact.status,
            fact.stage.as_str(),
            fact.reason,
            fact.fix
        );
    }

    fn sink(&self) -> RefusalLog {
        let journal = self.clone();
        Arc::new(move |fact| journal.log(fact))
    }
}

/// The allowlisted fact of one refused conversation turn. The status is the chat
/// response the owner saw; the engine's typed refusal code is folded into the
/// fixed reason token, and the message bytes are never read or written.
fn conversation_refusal(error: &OxaError) -> RefusalFact {
    let fact = |stage, reason, fix| RefusalFact {
        status: 503,
        stage,
        reason,
        fix,
    };
    match error.code {
        "budget_not_configured" => fact(
            Stage::Limits,
            "budget_invalid",
            "set oxa.cloud.daily_budget_usd to a positive number, or unset it for no USD limit",
        ),
        "cost_unknown_history" => fact(
            Stage::Limits,
            "cost_unknown_history",
            "unset oxa.cloud.daily_budget_usd, or wait for the next spend day; earlier uncapped requests today have an unknown cost",
        ),
        "budget_exceeded" | codes::BUDGET_EXHAUSTED => fact(
            Stage::Limits,
            "metered_budget_exhausted",
            "wait for the next spend day or raise the daily USD budget",
        ),
        // The bounded literal turn's own local output bound is not metering: a
        // partial answer that ran out of its shared budget keeps its own reason.
        codes::LITERAL_OUTPUT_EXHAUSTED => fact(
            Stage::Reply,
            "literal_output_exhausted",
            "ask for a shorter answer; the bounded Lead reply stopped at its output limit",
        ),
        "subscription_limit_exceeded" => fact(
            Stage::Limits,
            "subscription_limits_exhausted",
            "wait for the next spend day or adjust the positive subscription limits",
        ),
        "subscription_limits_not_configured" => fact(
            Stage::Limits,
            "subscription_limits_absent",
            "set positive daily subscription limits in user settings",
        ),
        "subscription_limit_reached"
        | "subscription_usage_limit_reached"
        | "subscription_usage_not_included"
        | "subscription_private"
        | "region_blocked" => fact(
            Stage::Limits,
            "subscription_plan_refused",
            "check the subscription plan limits with the provider",
        ),
        "lead_reply_invalid" => fact(
            Stage::Reply,
            "invalid",
            "retry the chat turn; the reply did not meet the contract",
        ),
        // A citation the bounded input never offered is the reply's own failure:
        // it stays a fixed reason, without any reply or input bytes.
        "lead_evidence_invalid" => fact(
            Stage::Reply,
            "evidence_invalid",
            "cite only the observations offered in the bounded input, each at most once",
        ),
        "lead_reply_incomplete" => fact(
            Stage::Reply,
            "incomplete",
            "retry the chat turn; the reply did not complete",
        ),
        codes::USAGE => fact(
            Stage::Body,
            "message",
            "keep the chat message within the served bound",
        ),
        "lead_owner_unavailable" => fact(
            Stage::Recheck,
            "binding",
            "check oxa.lead.sessionId and the original dedicated Lead session file",
        ),
        codes::BUSY => fact(
            Stage::Busy,
            "session",
            "wait for the current Lead session turn to finish",
        ),
        codes::NOT_FOUND => fact(
            Stage::Provider,
            "model",
            "set oxa.lead.model to an exact model id from the engine's list",
        ),
        codes::OFFLINE => fact(
            Stage::Provider,
            "offline",
            "disable offline mode for lead chat",
        ),
        codes::ENGINE_DOWN => fact(
            Stage::Provider,
            "engine_down",
            "start or repair the engine endpoint",
        ),
        "cloud_rate_limited" => fact(
            Stage::Provider,
            "rate_limited",
            "wait for the provider rate limit to reset",
        ),
        // Gateway admission refusals that are safe to name: fixed text only.
        "cloud_provider_forbidden" => fact(
            Stage::Provider,
            "provider_forbidden",
            "this subscription provider serves only the owner agent",
        ),
        "cloud_not_shared" => fact(
            Stage::Provider,
            "not_shared",
            "cloud models are never served to shared guests",
        ),
        _ => fact(
            Stage::Provider,
            "refused",
            "check the engine endpoint and provider status",
        ),
    }
}

/// The allowlisted identity fact of one refused request, from the typed access
/// refusal: known identity reasons stay distinguishable in the journal.
fn identity_fact(refusal: lead_access::Refusal) -> RefusalFact {
    let fact = |reason, fix| RefusalFact {
        status: 403,
        stage: Stage::Identity,
        reason,
        fix,
    };
    match refusal {
        lead_access::Refusal::Disabled => fact("disabled", "enable oxa.lead.serve in the user settings"),
        lead_access::Refusal::LocalOnly => {
            fact("non_loopback", "reach the lead listener from a loopback peer")
        }
        lead_access::Refusal::Unauthenticated => {
            fact("unverified", "verify the configured identity provider and login")
        }
        lead_access::Refusal::NotAllowed => fact(
            "not_allowed",
            "use the configured owner identity and exact authority",
        ),
        lead_access::Refusal::ReadOnly => fact(
            "not_allowed",
            "use the configured owner identity and exact authority",
        ),
    }
}

enum Proof<'a> {
    Local(bool),
    Proxy(proxy::Proxy<'a>),
}
impl IdentityProvider for Proof<'_> {
    fn verified(&self) -> Option<VerifiedIdentity<'_>> {
        match self {
            Self::Local(owner) => owner.then_some(VerifiedIdentity::LocalOwner),
            Self::Proxy(proof) => proof.verified(),
        }
    }
}

fn authorized(
    config: &Config,
    uid: u32,
    stream: &TcpStream,
    request: &crate::lead_http::Request<'_>,
    journal: &ServiceJournal,
) -> bool {
    let allowed = (|| {
        let peer = stream
            .peer_addr()
            .map_err(|_| lead_access::Refusal::Unauthenticated)?;
        let proof = match config.mode() {
            Mode::Local => Proof::Local(crate::lead_peer::owner_uid(stream) == Ok(uid)),
            Mode::Tailscale => Proof::Proxy(proxy::verify(config, stream, request.forwarded_login)?),
        };
        if request.method == "POST" && request.path == "/approvals" {
            lead_access::approval_post(config, peer.ip(), request, &proof)
        } else if request.method == "POST" {
            lead_access::chat_post(config, peer.ip(), request, &proof)
        } else {
            lead_access::read_only(
                config,
                request.method,
                peer.ip(),
                request.authority,
                request.forwarded_login,
                &proof,
                || (),
            )
        }
    })();
    if let Err(refused) = allowed {
        if crate::lead_http::chat_path(request.path) || crate::lead_http::approvals_path(request.path) {
            journal.log(identity_fact(refused));
        }
    }
    allowed.is_ok()
}

struct ChatReply {
    reply: ConversationReply,
    proposal: Option<(String, serde_json::Value)>,
    confirmed: bool,
}

struct OwnerChat {
    binding: LeadOwnerBinding,
    csrf: String,
    latest: Mutex<Option<ChatReply>>,
    journal: ServiceJournal,
}

impl OwnerChat {
    fn post(&self, submission: OwnerSubmission, env: &Env) -> Result<(), &'static str> {
        let mut state = self.latest.try_lock().map_err(|_| {
            self.journal.log(RefusalFact {
                status: 409,
                stage: Stage::Busy,
                reason: "locked",
                fix: "wait for the in-flight chat turn to finish",
            });
            "409 Conflict"
        })?;
        self.binding.recheck().map_err(|_| {
            self.journal.log(RefusalFact {
                status: 503,
                stage: Stage::Recheck,
                reason: "binding",
                fix: "check oxa.lead.sessionId and the original dedicated Lead session file",
            });
            "503 Service Unavailable"
        })?;
        match submission {
            OwnerSubmission::Message(message) => {
                let reply =
                    crate::lead_chat::converse_bound(&message, &self.binding, env).map_err(|error| {
                        self.journal.log(conversation_refusal(&error));
                        "503 Service Unavailable"
                    })?;
                self.binding.recheck().map_err(|_| {
                    self.journal.log(RefusalFact {
                        status: 503,
                        stage: Stage::Recheck,
                        reason: "binding",
                        fix: "check oxa.lead.sessionId and the original dedicated Lead session file",
                    });
                    "503 Service Unavailable"
                })?;
                let proposal = reply
                    .action
                    .as_ref()
                    .map(proposal_for_confirmation)
                    .transpose()
                    .map_err(|_| {
                        self.journal.log(RefusalFact {
                            status: 503,
                            stage: Stage::Reply,
                            reason: "proposal",
                            fix: "confirm the currently displayed proposal id",
                        });
                        "503 Service Unavailable"
                    })?;
                *state = Some(ChatReply {
                    reply,
                    proposal,
                    confirmed: false,
                });
            }
            OwnerSubmission::Confirm(id) => {
                let refusal = || {
                    self.journal.log(RefusalFact {
                        status: 409,
                        stage: Stage::Reply,
                        reason: "proposal",
                        fix: "confirm the currently displayed proposal id",
                    });
                    "409 Conflict"
                };
                let current = state.as_mut().ok_or_else(refusal)?;
                let (expected, proposal) = current.proposal.as_ref().ok_or_else(refusal)?;
                if id != *expected || current.confirmed {
                    self.journal.log(RefusalFact {
                        status: 409,
                        stage: Stage::Reply,
                        reason: "proposal",
                        fix: "confirm the currently displayed proposal id",
                    });
                    return Err("409 Conflict");
                }
                self.binding
                    .record_confirmation(&id, proposal.clone())
                    .map_err(|_| {
                        self.journal.log(RefusalFact {
                            status: 503,
                            stage: Stage::Recheck,
                            reason: "session",
                            fix: "recheck the dedicated Lead Session state",
                        });
                        "503 Service Unavailable"
                    })?;
                current.confirmed = true;
            }
            // The run closure answers approvals itself, so they never wait on the chat turn lock.
            OwnerSubmission::Approve { .. } => return Err("409 Conflict"),
        }
        Ok(())
    }
}

/// The owner's answers to approval requests (#1512 S6). The writes are the ones `oxa send
/// --approve` makes, through the same path, so a web answer and a CLI answer are equivalent.
struct OwnerApprovals {
    binding: LeadOwnerBinding,
    dirs: Dirs,
    cwd: PathBuf,
    desk: ApprovalDesk,
    journal: ServiceJournal,
}

impl OwnerApprovals {
    fn post(&self, id: &str, answer: &str) -> Result<(), &'static str> {
        let refuse = |status: u16, text: &'static str, reason: &'static str| -> Result<(), &'static str> {
            self.journal.log(RefusalFact {
                status,
                stage: Stage::Approval,
                reason,
                fix: "answer a request still listed as waiting for approval",
            });
            Err(text)
        };
        if self.binding.recheck().is_err() {
            return refuse(503, "503 Service Unavailable", "binding");
        }
        let waiting = match self.desk.pending() {
            Ok(waiting) => waiting,
            Err(_) => return refuse(503, "503 Service Unavailable", "desk"),
        };
        // Unknown and already answered ids are the same refusal: only pending requests answer.
        let Some(found) = waiting.iter().find(|rec| rec.id == id) else {
            return refuse(404, "404 Not Found", "not_pending");
        };
        let session = found.session.clone();
        if crate::approve_cmd::approve(&self.dirs, &self.cwd, &session, id, answer, &mut std::io::sink())
            .is_err()
        {
            return refuse(409, "409 Conflict", "answer");
        }
        Ok(())
    }
}

fn proposal_for_confirmation(
    draft: &crate::lead_chat::conversation::ProposedAction,
) -> Result<(String, serde_json::Value), &'static str> {
    let unavailable = "503 Service Unavailable";
    let id = oxa_mcp::pkce_pair(&oxa_mcp::random_bytes().map_err(|_| unavailable)?).0;
    let proposal = serde_json::to_value(draft).map_err(|_| unavailable)?;
    Ok((id, proposal))
}

struct Source {
    uid: u32,
    board: Option<PathBuf>,
    form: Option<crate::lead_http::FixedForm>,
    owner: Option<Arc<OwnerChat>>,
    desk: Option<ApprovalDesk>,
}
impl Source {
    fn new(env: &Env, uid: u32) -> Result<Self, OxaError> {
        let board = env.get("OXA_LEAD_BOARD_LOG").map(PathBuf::from);
        if board.as_ref().is_some_and(|path| !path.is_absolute()) {
            return Err(invalid("lead board sample path must be absolute"));
        }
        Ok(Self {
            uid,
            board,
            form: None,
            owner: None,
            desk: None,
        })
    }

    fn render(&self, page: crate::lead_page::Page, fragment: bool) -> String {
        // The existing bounded reader revalidates owner-only files/directories on every read.
        // Missing, stale and refused data remain explicit in the shared renderer.
        let sample = self
            .board
            .as_deref()
            .and_then(|path| crate::lead_input::board_and_app_quota(path, self.uid).ok());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let github_quotas = sample.as_ref().and_then(|(board, app_quota)| {
            crate::lead_quota::GitHubQuotas::from_app_sample((*app_quota)?, board.observed_at, now)
        });
        use crate::lead_page::Page;
        let approvals_page = matches!(page, Page::Home | Page::Board | Page::BoardQuery(_));
        let chat_page = matches!(page, Page::Chat(_));
        // Owner chat is configured only for a single verified Tailscale identity.
        let bound_owner = self
            .owner
            .as_ref()
            .filter(|owner| (approvals_page || chat_page) && owner.binding.recheck().is_ok());
        let owner = bound_owner.filter(|_| chat_page);
        let latest = owner.and_then(|owner| owner.latest.try_lock().ok());
        let waiting = if approvals_page && bound_owner.is_some() {
            self.desk.as_ref().and_then(|desk| desk.pending().ok())
        } else {
            None
        };
        let view = crate::lead_page::View {
            approvals: waiting.as_deref(),
            approval_csrf: bound_owner.map(|owner| owner.csrf.as_str()),
            owner_chat: owner
                .zip(latest.as_deref())
                .map(|(owner, latest)| crate::lead_page::OwnerChatView {
                    csrf: &owner.csrf,
                    reply: latest.as_ref().map(|latest| &latest.reply),
                    proposal: latest.as_ref().and_then(|latest| latest.proposal.as_ref()),
                    confirmed: latest.as_ref().is_some_and(|latest| latest.confirmed),
                }),
            board: sample.as_ref().map(|(board, _)| board),
            github_quotas: github_quotas.as_ref(),
            fixed_prompt_csrf: self.form.as_ref().map(|form| form.csrf.as_str()),
            ..Default::default()
        };
        if fragment {
            crate::lead_page::content_for(page, &view, now)
        } else {
            crate::lead_page::document_for(page, &view, now)
        }
    }
}

/// With no `oxa.cloud.daily_budget_usd` there is no budget check: a metered Lead model is admitted
/// and its cost recorded, not limited. The owner is told once at startup.
fn no_budget_warning(settings: &Settings) -> Option<&'static str> {
    let capped = settings
        .get("oxa.cloud.daily_budget_usd")
        .is_some_and(|cap| !cap.is_null());
    (!capped).then_some(
        "lead chat: no oxa.cloud.daily_budget_usd is set, so metered cloud use is recorded but not limited by USD",
    )
}

/// Called only by a registry-resolved handler; this module declares no command or flag.
pub fn run(dirs: &Dirs, cwd: &Path, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    if oxa_proto::offline() {
        return Err(invalid("lead serve is unavailable in offline mode"));
    }
    let config = Config::load(dirs, cwd).map_err(invalid)?;
    if !config.enabled() {
        return Err(invalid("lead serve is disabled in user settings"));
    }
    proxy::pre_bind(&config).map_err(invalid)?;
    let uid = std::fs::metadata("/proc/self")
        .map_err(|_| invalid("OS owner identity unavailable"))?
        .uid();
    let mut source = Source::new(env, uid)?;
    // The approval desk is read on every board render; a desk that cannot open shows as unavailable.
    source.desk = ApprovalDesk::open(&dirs.agent_dir).ok();
    // The one real service journal: fixed allowlisted lines on stderr, carried to
    // the system journal by the operator's existing service unit.
    let journal = ServiceJournal::stderr();
    if config.fixed_prompts() {
        source.form = Some(crate::lead_http::FixedForm {
            csrf: oxa_mcp::pkce_pair(&oxa_mcp::random_bytes()?).0,
            scheme: if config.mode() == Mode::Local {
                "http"
            } else {
                "https"
            },
        });
    }
    let owner = if config.owner_chat() {
        let project = cwd
            .canonicalize()
            .map_err(|_| invalid("Lead project unavailable"))?;
        let binding = LeadOwnerBinding::load(dirs, &project, uid)?
            .ok_or_else(|| invalid("Lead owner chat needs its selected Session"))?;
        let chat = Arc::new(OwnerChat {
            binding,
            csrf: oxa_mcp::pkce_pair(&oxa_mcp::random_bytes()?).0,
            latest: Mutex::new(None),
            journal: journal.clone(),
        });
        source.owner = Some(chat.clone());
        let env = env.clone();
        // Answers use the owner form's CSRF and origin checks, then the desk. They never wait on
        // the chat turn lock, so a running Lead turn does not hold up an approval.
        let approvals = source.desk.clone().map(|desk| OwnerApprovals {
            binding: chat.binding.clone(),
            dirs: dirs.clone(),
            cwd: cwd.to_path_buf(),
            desk,
            journal: journal.clone(),
        });
        Some(OwnerForm {
            csrf: chat.csrf.clone(),
            post: Arc::new(move |submission| match submission {
                OwnerSubmission::Approve { id, answer } => match &approvals {
                    Some(approvals) => approvals.post(&id, &answer),
                    None => Err("503 Service Unavailable"),
                },
                other => chat.post(other, &env),
            }),
        })
    } else {
        None
    };
    let form = source.form.clone();
    let identity_journal = journal.clone();
    let refusal_log = journal.sink();
    let listener = TcpListener::bind(config.endpoint()).map_err(|_| invalid("cannot bind lead endpoint"))?;
    if owner.is_some() {
        if let Some(warning) = no_budget_warning(&Settings::load(dirs, cwd, false)) {
            writeln!(out, "{warning}").map_err(|_| invalid("cannot report lead startup"))?;
        }
    }
    writeln!(out, "lead board: listening on configured loopback endpoint")
        .and_then(|()| out.flush())
        .map_err(|_| invalid("cannot report lead startup"))?;
    crate::lead_http::serve_fixed(
        listener,
        move |stream, request| authorized(&config, uid, stream, request, &identity_journal),
        move |page, fragment| source.render(page, fragment),
        form,
        owner,
        Some(refusal_log),
    )
    .map_err(|_| invalid("lead listener failed"))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::PermissionsExt;

    /// An in-memory stand-in for the process's standard error, so the journal's
    /// exact lines can be asserted without any new file or dependency.
    struct SharedSink(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write_all(bytes)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn captured_journal() -> (ServiceJournal, Arc<Mutex<Vec<u8>>>) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        (
            ServiceJournal {
                out: Arc::new(Mutex::new(Box::new(SharedSink(buffer.clone())))),
            },
            buffer,
        )
    }

    fn recorded(buffer: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
        String::from_utf8(buffer.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn owner_chat_records_only_the_displayed_proposal_and_renders_text() {
        use oxa_fake::{FakeOpts, FakeServer, ScriptTurn};
        use oxa_session::SessionStore;

        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        let project = test.work().canonicalize().unwrap();
        let uid = std::fs::metadata(&project).unwrap().uid();
        // The whole reply arrives in one JSON Markdown fence: the only harmless
        // formatting variation #1448 admits, with unchanged validation.
        let fenced = format!(
            "```json\n{}\n```",
            r#"{"text":"fixture <b>reply</b>","facts":[],"action":{"kind":"rule","rule":"note","scope":{"kind":"repository"},"value":{"kind":"text","value":"fixture note"}}}"#
        );
        let fake = FakeServer::start(vec![ScriptTurn::text(&fenced)], FakeOpts::default()).unwrap();
        let pairs = test.envs(&fake.url());
        let borrowed: Vec<_> = pairs
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let env = Env::from_pairs(&borrowed);
        std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = LeadOwnerBinding::create(&dirs, &project, uid).unwrap();
        let settings = dirs.settings_file();
        std::fs::write(
            &settings,
            serde_json::json!({"oxa":{"lead":{"sessionId":id}}}).to_string(),
        )
        .unwrap();
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (journal, buffer) = captured_journal();
        let owner = Arc::new(OwnerChat {
            binding: LeadOwnerBinding::load(&dirs, &project, uid).unwrap().unwrap(),
            csrf: "fixture".into(),
            latest: Mutex::new(None),
            journal,
        });
        let mut source = Source::new(&env, uid).unwrap();
        source.owner = Some(owner.clone());
        {
            let _busy = owner.latest.lock().unwrap();
            assert_eq!(
                owner.post(OwnerSubmission::Message("blocked".into()), &env),
                Err("409 Conflict")
            );
        }
        owner
            .post(OwnerSubmission::Message("/goal is literal".into()), &env)
            .unwrap();
        let page = crate::lead_page::Page::Chat(crate::lead_page::Prompt::Status);
        let html = source.render(page, false);
        assert!(html.contains("fixture &lt;b&gt;reply&lt;/b&gt;"));
        assert!(!html.contains("fixture <b>reply</b>"));
        assert!(html.contains("Confirm proposal"));
        assert_eq!(
            owner.post(OwnerSubmission::Confirm("wrong".into()), &env),
            Err("409 Conflict")
        );
        let store = oxa_session::FsStore::new(dirs.sessions_dir.clone());
        let selected = store.resolve(&id).unwrap();
        let before = store.load(&selected).unwrap();
        let proposal_id = owner
            .latest
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .proposal
            .as_ref()
            .unwrap()
            .0
            .clone();
        owner
            .post(OwnerSubmission::Confirm(proposal_id.clone()), &env)
            .unwrap();
        assert_eq!(
            owner.post(OwnerSubmission::Confirm(proposal_id.clone()), &env),
            Err("409 Conflict")
        );
        let after = store.load(&selected).unwrap();
        assert_eq!(after.len(), before.len() + 1);
        assert!(
            matches!(&after.last().unwrap().body, oxa_proto::EntryBody::Custom { custom_type, data } if custom_type == "oxa.lead.proposal_confirmed" && data["proposal_id"] == proposal_id)
        );
        assert!(source
            .render(page, true)
            .contains("Confirmation recorded. Application is pending."));
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|request| request.max_tokens > 0)
                .count(),
            1
        );
        // Every real block of the turns above left one allowlisted line, and
        // nothing else: no message text, no CSRF, no identity, no reply bytes.
        let lines = recorded(&buffer);
        assert_eq!(
            lines,
            [
                "lead chat refusal status=409 stage=busy reason=locked fix=wait for the in-flight chat turn to finish",
                "lead chat refusal status=409 stage=reply reason=proposal fix=confirm the currently displayed proposal id",
                "lead chat refusal status=409 stage=reply reason=proposal fix=confirm the currently displayed proposal id",
            ],
            "{lines:?}"
        );
        let text = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(!text.contains("blocked"));
        assert!(!text.contains("fixture"));
        assert!(!text.contains("```"));
    }

    #[test]
    fn lead_chat_no_budget_warns() {
        let none = Settings::from_values(serde_json::json!({"oxa":{"cloud":{}}}), serde_json::json!({}));
        let warning = no_budget_warning(&none).unwrap();
        assert!(warning.contains("not limited by USD"));
        let capped = Settings::from_values(
            serde_json::json!({"oxa":{"cloud":{"daily_budget_usd":5}}}),
            serde_json::json!({}),
        );
        assert!(no_budget_warning(&capped).is_none());
    }

    #[test]
    fn refusal_reasons_are_allowlisted_and_carry_no_error_bytes() {
        let budget = conversation_refusal(&OxaError::new(
            "budget_not_configured",
            "oxa.cloud.daily_budget_usd must be a positive number in oxa user settings, or unset for no USD limit.",
        ));
        assert_eq!(
            budget,
            RefusalFact {
                status: 503,
                stage: Stage::Limits,
                reason: "budget_invalid",
                fix: "set oxa.cloud.daily_budget_usd to a positive number, or unset it for no USD limit",
            }
        );
        for (code, stage, reason) in [
            (codes::BUDGET_EXHAUSTED, Stage::Limits, "metered_budget_exhausted"),
            ("cost_unknown_history", Stage::Limits, "cost_unknown_history"),
            (
                "subscription_limits_not_configured",
                Stage::Limits,
                "subscription_limits_absent",
            ),
            (
                "subscription_limit_exceeded",
                Stage::Limits,
                "subscription_limits_exhausted",
            ),
            ("lead_reply_invalid", Stage::Reply, "invalid"),
            ("lead_evidence_invalid", Stage::Reply, "evidence_invalid"),
            ("lead_reply_incomplete", Stage::Reply, "incomplete"),
            ("cloud_provider_forbidden", Stage::Provider, "provider_forbidden"),
            ("cloud_not_shared", Stage::Provider, "not_shared"),
            (
                codes::LITERAL_OUTPUT_EXHAUSTED,
                Stage::Reply,
                "literal_output_exhausted",
            ),
            (codes::USAGE, Stage::Body, "message"),
            ("lead_owner_unavailable", Stage::Recheck, "binding"),
            (codes::BUSY, Stage::Busy, "session"),
            (codes::NOT_FOUND, Stage::Provider, "model"),
            (codes::OFFLINE, Stage::Provider, "offline"),
            (codes::ENGINE_DOWN, Stage::Provider, "engine_down"),
            ("cloud_rate_limited", Stage::Provider, "rate_limited"),
            ("unclassified_engine_failure", Stage::Provider, "refused"),
        ] {
            let fact = conversation_refusal(&OxaError::new(code, "raw engine error bytes"));
            assert_eq!(fact.status, 503, "{code}");
            assert_eq!(fact.stage, stage, "{code}");
            assert_eq!(fact.reason, reason, "{code}");
            assert!(!fact.fix.is_empty(), "{code}");
        }
    }

    #[test]
    fn request_proof_composes_actual_kernel_owner_and_unchanged_authority() {
        let test = TestEnv::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let port = listener.local_addr().unwrap().port();
        let dirs = Dirs::under(test.root());
        std::fs::create_dir_all(&dirs.agent_dir).unwrap();
        std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            dirs.settings_file(),
            serde_json::json!({"oxa":{"lead":{"serve":{
            "enabled":true,"port":port}}}})
            .to_string(),
        )
        .unwrap();
        let config = Config::load(&dirs, &test.work()).unwrap();
        let uid = std::fs::metadata("/proc/self").unwrap().uid();
        let (journal, buffer) = captured_journal();
        let authority = format!("127.0.0.1:{port}");
        let mut request = crate::lead_http::Request {
            method: "GET",
            path: "/",
            authority: &authority,
            forwarded_login: None,
        };
        assert!(authorized(&config, uid, &stream, &request, &journal));
        assert!(!authorized(
            &config,
            uid.wrapping_add(1),
            &stream,
            &request,
            &journal
        ));
        request.authority = "attacker.invalid";
        assert!(!authorized(&config, uid, &stream, &request, &journal));
        request.authority = &authority;
        request.forwarded_login = Some("owner@example.invalid");
        assert!(!authorized(&config, uid, &stream, &request, &journal));
        request.forwarded_login = None;
        request.method = "POST";
        assert!(!authorized(&config, uid, &stream, &request, &journal));
        // None of those refusals was a chat request: identity facts name the chat
        // pipeline only.
        assert!(recorded(&buffer).is_empty());
        request.path = "/chat";
        request.method = "GET";
        request.authority = "attacker.invalid";
        assert!(!authorized(&config, uid, &stream, &request, &journal));
        assert_eq!(
            recorded(&buffer),
            ["lead chat refusal status=403 stage=identity reason=not_allowed fix=use the configured owner identity and exact authority"]
        );
        // The known identity reasons stay distinguishable in the journal.
        assert_eq!(request.path, "/chat");
        request.authority = &authority;
        assert!(!authorized(
            &config,
            uid.wrapping_add(1),
            &stream,
            &request,
            &journal
        ));
        assert!(recorded(&buffer)[1].contains("reason=unverified"), "{buffer:?}");
        drop(client);
    }

    #[test]
    fn local_source_revalidates_private_sample_and_never_reflects_errors() {
        let test = TestEnv::new();
        let directory = test.root().join("board-input");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("board.jsonl");
        std::fs::write(
            &path,
            b"{\"schema_version\":1,\"observed_at\":10,\"board\":{\"observed_at\":10,\"entries\":[]}}\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let uid = std::fs::metadata(&directory).unwrap().uid();
        let path_text = path.to_str().unwrap();
        let env = Env::from_pairs(&[("OXA_LEAD_BOARD_LOG", path_text)]);
        let source = Source::new(&env, uid).unwrap();
        assert!(source
            .render(crate::lead_page::Page::Home, false)
            .contains("No open PRs in this snapshot"));
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        for (observed_at, expected) in [
            (now, "123 of 5000 remaining"),
            (now.saturating_sub(601), "Unknown"),
        ] {
            let sample = serde_json::json!({
                "schema_version": 1,
                "observed_at": observed_at,
                "board": {"observed_at": observed_at, "entries": []},
                "app_quota": {
                    "graphql": {"limit": 5000, "remaining": 123, "resets_at": now + 600},
                },
            });
            std::fs::write(&path, format!("{sample}\n")).unwrap();
            let usage = source.render(crate::lead_page::Page::Usage, true);
            assert!(usage.contains(&format!("GitHub App · GraphQL</dt><dd>{expected}")));
            assert!(usage.contains("GitHub Owner · GraphQL</dt><dd>Unknown"));
            assert!(usage.contains("Primary model window</dt><dd>Unknown"));
            assert!(usage.contains("Secondary model window</dt><dd>Unknown"));
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let refused = source.render(crate::lead_page::Page::Home, true);
        assert!(refused.contains("Board unavailable"));
        assert!(!refused.contains(path_text));
        assert!(Source::new(
            &Env::from_pairs(&[("OXA_LEAD_BOARD_LOG", "relative-private-marker")]),
            uid
        )
        .is_err());
    }
}

#[cfg(test)]
mod approval_answer_tests {
    use super::*;
    use oxa_config::Settings;
    use oxa_fake::TestEnv;
    use oxa_perm::{ApprovalDesk, DangerHit};
    use std::os::unix::fs::PermissionsExt;

    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write_all(bytes)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn captured() -> (ServiceJournal, Arc<Mutex<Vec<u8>>>) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        (
            ServiceJournal {
                out: Arc::new(Mutex::new(Box::new(Sink(buffer.clone())))),
            },
            buffer,
        )
    }

    fn recorded(buffer: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
        String::from_utf8(buffer.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn fixture() -> (TestEnv, Dirs, ApprovalDesk) {
        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        let desk = ApprovalDesk::open(&dirs.agent_dir).unwrap();
        (test, dirs, desk)
    }

    fn hit() -> DangerHit {
        DangerHit {
            rule: "rm-recursive".into(),
            reason: "recursive delete outside the workspace".into(),
            segment: "rm -rf /srv/data".into(),
        }
    }

    fn file(test: &TestEnv, desk: &ApprovalDesk) -> String {
        desk.record("s1", &test.work(), "rm -rf /srv/data", &[hit()], "pending")
            .unwrap()
    }

    fn owner_answers(
        test: &TestEnv,
        dirs: &Dirs,
        desk: &ApprovalDesk,
        journal: ServiceJournal,
    ) -> OwnerApprovals {
        let project = test.work().canonicalize().unwrap();
        let uid = std::fs::metadata(&project).unwrap().uid();
        std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner_id = LeadOwnerBinding::create(dirs, &project, uid).unwrap();
        std::fs::write(
            dirs.settings_file(),
            serde_json::json!({"oxa":{"lead":{"sessionId":owner_id}}}).to_string(),
        )
        .unwrap();
        std::fs::set_permissions(dirs.settings_file(), std::fs::Permissions::from_mode(0o600)).unwrap();
        OwnerApprovals {
            binding: LeadOwnerBinding::load(dirs, &project, uid).unwrap().unwrap(),
            dirs: dirs.clone(),
            cwd: test.work(),
            desk: desk.clone(),
            journal,
        }
    }

    #[test]
    fn board_without_owner_binding_does_not_render_approval_details() {
        let (test, _dirs, desk) = fixture();
        file(&test, &desk);
        let mut source = Source::new(&Env::from_pairs(&[]), 0).unwrap();
        source.desk = Some(desk);
        for (page, fragment) in [
            (crate::lead_page::Page::Home, false),
            (crate::lead_page::Page::Home, true),
            (crate::lead_page::Page::Board, false),
        ] {
            let html = source.render(page, fragment);
            assert!(!html.contains("Waiting for your approval"));
            assert!(!html.contains("/srv/data"));
        }
    }

    #[test]
    fn approval_post_once_answers_pending_request() {
        let (test, dirs, desk) = fixture();
        let id = file(&test, &desk);
        let (journal, buffer) = captured();
        let answers = owner_answers(&test, &dirs, &desk, journal);
        answers.post(&id, "once").unwrap();
        assert!(desk.pending().unwrap().is_empty());
        assert!(recorded(&buffer).is_empty());
    }

    #[test]
    fn approval_post_always_writes_user_settings_only() {
        let (test, dirs, desk) = fixture();
        let id = file(&test, &desk);
        let keys = desk.pending().unwrap()[0].keys.clone();
        assert!(!keys.is_empty());
        let (journal, buffer) = captured();
        let answers = owner_answers(&test, &dirs, &desk, journal);
        answers.post(&id, "always").unwrap();
        // The keys land in the user settings file and nowhere else.
        let user = std::fs::read_to_string(dirs.settings_file()).unwrap();
        for key in &keys {
            assert!(user.contains(key.as_str()), "{user}");
        }
        assert_eq!(
            Settings::load(&dirs, &test.work(), false).list("oxa.approval.always"),
            keys
        );
        assert!(recorded(&buffer).is_empty());
    }

    #[test]
    fn approval_post_unknown_or_answered_id_is_refused() {
        let (test, dirs, desk) = fixture();
        let id = file(&test, &desk);
        let (journal, buffer) = captured();
        let answers = owner_answers(&test, &dirs, &desk, journal);
        assert_eq!(answers.post("0123456789abcdef", "once"), Err("404 Not Found"));
        answers.post(&id, "deny").unwrap();
        // An answered request is no longer pending, so a second answer is refused the same way.
        assert_eq!(answers.post(&id, "once"), Err("404 Not Found"));
        assert_eq!(
            recorded(&buffer),
            [
                "lead chat refusal status=404 stage=approval reason=not_pending fix=answer a request still listed as waiting for approval",
                "lead chat refusal status=404 stage=approval reason=not_pending fix=answer a request still listed as waiting for approval",
            ]
        );
    }

    #[test]
    fn approval_post_rechecks_a_stale_owner_binding_before_answering() {
        let (test, dirs, desk) = fixture();
        let id = file(&test, &desk);
        let (journal, buffer) = captured();
        let answers = owner_answers(&test, &dirs, &desk, journal);
        std::fs::remove_file(dirs.settings_file()).unwrap();

        assert_eq!(answers.post(&id, "once"), Err("503 Service Unavailable"));
        assert_eq!(desk.pending().unwrap().len(), 1);
        assert_eq!(
            recorded(&buffer),
            ["lead chat refusal status=503 stage=approval reason=binding fix=answer a request still listed as waiting for approval"]
        );
    }

    #[test]
    fn cli_and_web_answers_share_the_desk_lock() {
        use std::sync::{mpsc, Barrier};
        use std::time::Duration;

        let (test, dirs, desk) = fixture();
        let id = file(&test, &desk);
        let keys = desk.pending().unwrap()[0].keys.clone();
        let cwd = test.work();
        let (journal, _) = captured();
        let web = owner_answers(&test, &dirs, &desk, journal);
        let guard = std::fs::File::open(dirs.agent_dir.join("approvals")).unwrap();
        guard.lock().unwrap();
        let start = Barrier::new(3);

        std::thread::scope(|scope| {
            let (tx, rx) = mpsc::channel();
            let web_tx = tx.clone();
            let start_ref = &start;
            let request = &id;
            scope.spawn(move || {
                start_ref.wait();
                web_tx
                    .send(("session", web.post(request, "session").is_ok()))
                    .unwrap();
            });
            let cli_dirs = &dirs;
            let cli_cwd = &cwd;
            scope.spawn(move || {
                start_ref.wait();
                let result =
                    crate::approve_cmd::approve(cli_dirs, cli_cwd, "s1", request, "deny", &mut Vec::new());
                tx.send(("deny", result.is_ok())).unwrap();
            });
            start.wait();
            let early = rx.recv_timeout(Duration::from_millis(400));
            drop(guard);
            assert!(matches!(early, Err(mpsc::RecvTimeoutError::Timeout)));
            let results = [rx.recv().unwrap(), rx.recv().unwrap()];
            assert_eq!(results.iter().filter(|(_, ok)| *ok).count(), 1);
            assert_eq!(
                desk.granted("s1", &cwd, &keys),
                results.contains(&("session", true)),
                "the stored decision is the winning answer"
            );
        });
    }

    #[test]
    fn approval_post_refusal_is_journaled() {
        // Local mode has no owner chat, so the approval gate refuses the POST; the refusal is
        // journaled as an identity fact on the approval path, like a refused chat submission.
        let test = TestEnv::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let port = listener.local_addr().unwrap().port();
        let dirs = Dirs::under(test.root());
        std::fs::create_dir_all(&dirs.agent_dir).unwrap();
        std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            dirs.settings_file(),
            serde_json::json!({"oxa":{"lead":{"serve":{"enabled":true,"port":port}}}}).to_string(),
        )
        .unwrap();
        let config = Config::load(&dirs, &test.work()).unwrap();
        let uid = std::fs::metadata("/proc/self").unwrap().uid();
        let (journal, buffer) = captured();
        let authority = format!("127.0.0.1:{port}");
        let request = crate::lead_http::Request {
            method: "POST",
            path: "/approvals",
            authority: &authority,
            forwarded_login: None,
        };
        assert!(!authorized(&config, uid, &stream, &request, &journal));
        assert_eq!(
            recorded(&buffer),
            ["lead chat refusal status=403 stage=identity reason=not_allowed fix=use the configured owner identity and exact authority"]
        );
        drop(client);
    }
}
