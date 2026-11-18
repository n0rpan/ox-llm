//! Minimal local board caller. Transport, policy, peer proof and rendering stay shared.
use crate::dispatch::LeadOwnerBinding;
use crate::lead_access::{self, proxy, Config, IdentityProvider, Mode, VerifiedIdentity};
use crate::lead_chat::conversation::ConversationReply;
use crate::lead_http::{OwnerForm, OwnerSubmission};
use oxa_config::{Dirs, Env};
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
) -> bool {
    let Ok(peer) = stream.peer_addr() else {
        return false;
    };
    let proof = match config.mode() {
        Mode::Local => Proof::Local(crate::lead_peer::owner_uid(stream) == Ok(uid)),
        Mode::Tailscale => match proxy::verify(config, stream, request.forwarded_login) {
            Ok(proof) => Proof::Proxy(proof),
            Err(_) => return false,
        },
    };
    if request.method == "POST" {
        return lead_access::chat_post(config, peer.ip(), request, &proof);
    }
    lead_access::read_only(
        config,
        request.method,
        peer.ip(),
        request.authority,
        request.forwarded_login,
        &proof,
        || (),
    )
    .is_ok()
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
}

impl OwnerChat {
    fn post(&self, submission: OwnerSubmission, env: &Env) -> Result<(), &'static str> {
        let mut state = self.latest.try_lock().map_err(|_| "409 Conflict")?;
        self.binding.recheck().map_err(|_| "503 Service Unavailable")?;
        match submission {
            OwnerSubmission::Message(message) => {
                let reply = crate::lead_chat::converse_bound(&message, &self.binding, env)
                    .map_err(|_| "503 Service Unavailable")?;
                self.binding.recheck().map_err(|_| "503 Service Unavailable")?;
                let proposal = reply.action.as_ref().map(proposal_for_confirmation).transpose()?;
                *state = Some(ChatReply {
                    reply,
                    proposal,
                    confirmed: false,
                });
            }
            OwnerSubmission::Confirm(id) => {
                let current = state.as_mut().ok_or("409 Conflict")?;
                let (expected, proposal) = current.proposal.as_ref().ok_or("409 Conflict")?;
                if id != *expected || current.confirmed {
                    return Err("409 Conflict");
                }
                self.binding
                    .record_confirmation(&id, proposal.clone())
                    .map_err(|_| "503 Service Unavailable")?;
                current.confirmed = true;
            }
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
        let owner = self.owner.as_ref().filter(|owner| {
            matches!(page, crate::lead_page::Page::Chat(_)) && owner.binding.recheck().is_ok()
        });
        let latest = owner.and_then(|owner| owner.latest.try_lock().ok());
        let view = crate::lead_page::View {
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
        });
        source.owner = Some(chat.clone());
        let env = env.clone();
        Some(OwnerForm {
            csrf: chat.csrf.clone(),
            post: Arc::new(move |submission| chat.post(submission, &env)),
        })
    } else {
        None
    };
    let form = source.form.clone();
    let listener = TcpListener::bind(config.endpoint()).map_err(|_| invalid("cannot bind lead endpoint"))?;
    writeln!(out, "lead board: listening on configured loopback endpoint")
        .and_then(|()| out.flush())
        .map_err(|_| invalid("cannot report lead startup"))?;
    crate::lead_http::serve_fixed(
        listener,
        move |stream, request| authorized(&config, uid, stream, request),
        move |page, fragment| source.render(page, fragment),
        form,
        owner,
    )
    .map_err(|_| invalid("lead listener failed"))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn owner_chat_records_only_the_displayed_proposal_and_renders_text() {
        use oxa_fake::{FakeOpts, FakeServer, ScriptTurn};
        use oxa_session::SessionStore;

        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        let project = test.work().canonicalize().unwrap();
        let uid = std::fs::metadata(&project).unwrap().uid();
        let fake = FakeServer::start(vec![ScriptTurn::text(
            r#"{"text":"fixture <b>reply</b>","facts":[],"action":{"kind":"rule","rule":"note","scope":{"kind":"repository"},"value":{"kind":"text","value":"fixture note"}}}"#,
        )], FakeOpts::default()).unwrap();
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
        let owner = Arc::new(OwnerChat {
            binding: LeadOwnerBinding::load(&dirs, &project, uid).unwrap().unwrap(),
            csrf: "fixture".into(),
            latest: Mutex::new(None),
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
        std::fs::write(
            dirs.settings_file(),
            serde_json::json!({"oxa":{"lead":{"serve":{
            "enabled":true,"port":port}}}})
            .to_string(),
        )
        .unwrap();
        let config = Config::load(&dirs, &test.work()).unwrap();
        let uid = std::fs::metadata("/proc/self").unwrap().uid();
        let authority = format!("127.0.0.1:{port}");
        let mut request = crate::lead_http::Request {
            method: "GET",
            path: "/",
            authority: &authority,
            forwarded_login: None,
        };
        assert!(authorized(&config, uid, &stream, &request));
        assert!(!authorized(&config, uid.wrapping_add(1), &stream, &request));
        request.authority = "attacker.invalid";
        assert!(!authorized(&config, uid, &stream, &request));
        request.authority = &authority;
        request.forwarded_login = Some("owner@example.invalid");
        assert!(!authorized(&config, uid, &stream, &request));
        request.forwarded_login = None;
        request.method = "POST";
        assert!(!authorized(&config, uid, &stream, &request));
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
