//! Minimal local board caller. Transport, policy, peer proof and rendering stay shared.
use crate::lead_access::{self, proxy, Config, IdentityProvider, Mode, VerifiedIdentity};
use oxa_config::{Dirs, Env};
use oxa_proto::{codes, OxaError};
use std::{
    io::Write,
    net::{TcpListener, TcpStream},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
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
        return lead_access::fixed_prompt(config, peer.ip(), request, &proof);
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

struct Source {
    uid: u32,
    board: Option<PathBuf>,
    situation: Option<PathBuf>,
    form: Option<crate::lead_http::FixedForm>,
}
impl Source {
    fn new(env: &Env, uid: u32) -> Result<Self, OxaError> {
        let board = env.get("OXA_LEAD_BOARD_LOG").map(PathBuf::from);
        let situation = env.get("OXA_LEAD_SITUATION").map(PathBuf::from);
        if board
            .iter()
            .chain(situation.iter())
            .any(|path| !path.is_absolute())
        {
            return Err(invalid("lead board sample path must be absolute"));
        }
        Ok(Self {
            uid,
            board,
            situation,
            form: None,
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
        let situation = self
            .situation
            .as_deref()
            .and_then(|path| crate::lead_input::situation(path, self.uid).ok());
        let view = crate::lead_page::View {
            board: sample.as_ref().map(|(board, _)| board),
            github_quotas: github_quotas.as_ref(),
            situation: situation.as_ref(),
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
            csrf: oxa_mcp::oauth::pkce_pair(&oxa_mcp::oauth::random_bytes()?).0,
            scheme: if config.mode() == Mode::Local {
                "http"
            } else {
                "https"
            },
        });
    }
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
