//! Minimal local board caller. Transport, policy, peer proof and rendering stay shared.
use crate::lead_access::{self, Config, IdentityProvider, Mode, VerifiedIdentity};
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

struct LocalProof(bool);
impl IdentityProvider for LocalProof {
    fn verified(&self) -> Option<VerifiedIdentity<'_>> {
        self.0.then_some(VerifiedIdentity::LocalOwner)
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
    let proof = LocalProof(crate::lead_peer::owner_uid(stream) == Ok(uid));
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
}
impl Source {
    fn new(env: &Env, uid: u32) -> Result<Self, OxaError> {
        let board = env.get("OXA_LEAD_BOARD_LOG").map(PathBuf::from);
        if board.as_ref().is_some_and(|path| !path.is_absolute()) {
            return Err(invalid("lead board sample path must be absolute"));
        }
        Ok(Self { uid, board })
    }

    fn render(&self, fragment: bool) -> String {
        // The existing bounded reader revalidates owner-only files/directories on every read.
        // Missing, stale and refused data remain explicit in the shared renderer.
        let board = self
            .board
            .as_deref()
            .and_then(|path| crate::lead_input::board(path, self.uid).ok());
        let view = crate::lead_page::View {
            board: board.as_ref(),
            ..Default::default()
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if fragment {
            crate::lead_page::content(&view, now)
        } else {
            crate::lead_page::document(&view, now)
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
    // A future provider must supply independently verified proxy authority and identity.
    if config.mode() != Mode::Local {
        return Err(invalid("lead proxy identity provider is unavailable"));
    }
    let uid = std::fs::metadata("/proc/self")
        .map_err(|_| invalid("OS owner identity unavailable"))?
        .uid();
    let source = Source::new(env, uid)?;
    let listener = TcpListener::bind(config.endpoint()).map_err(|_| invalid("cannot bind lead endpoint"))?;
    writeln!(out, "lead board: listening on configured loopback endpoint")
        .and_then(|()| out.flush())
        .map_err(|_| invalid("cannot report lead startup"))?;
    crate::lead_http::serve(
        listener,
        move |stream, request| authorized(&config, uid, stream, request),
        move |fragment| source.render(fragment),
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
        assert!(source.render(false).contains("No open PRs in this snapshot"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let refused = source.render(true);
        assert!(refused.contains("Board unavailable"));
        assert!(!refused.contains(path_text));
        assert!(Source::new(
            &Env::from_pairs(&[("OXA_LEAD_BOARD_LOG", "relative-private-marker")]),
            uid
        )
        .is_err());
    }
}
