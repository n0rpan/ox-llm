//! Protected selection of the native Session shared by the owner chat callers.
use super::{open_named, prepare, Args, RunMode};
use oxa_config::{Dirs, Env};
use oxa_core::Session;
use oxa_proto::{codes, OxaError, Value};
use oxa_session::{decode_header, session_dir_name, FsStore, SessionHeader, SessionRef, SessionStore};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_SETTINGS: u64 = 1_048_576;
const MAX_HEADER: u64 = 65_536;

/// One explicitly selected Session. Command and HTTP input cannot change its identity.
pub struct LeadOwnerBinding {
    dirs: Dirs,
    project: PathBuf,
    uid: u32,
    session: SessionRef,
    file: (u64, u64),
}

fn unavailable() -> OxaError {
    OxaError::new(
        "lead_owner_unavailable",
        "The selected Lead session is unavailable.",
    )
}

fn canonical(path: &Path) -> Result<(), OxaError> {
    if path.canonicalize().ok().as_deref() != Some(path) {
        return Err(unavailable());
    }
    Ok(())
}

fn private_file(path: &Path, uid: u32) -> Result<File, OxaError> {
    canonical(path)?;
    crate::lead_input::open(path, uid).map_err(|_| unavailable())
}

fn private_directory(path: &Path, uid: u32) -> Result<(), OxaError> {
    canonical(path)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| unavailable())?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(unavailable());
    }
    Ok(())
}

fn selection(dirs: &Dirs, uid: u32) -> Result<Option<String>, OxaError> {
    let path = dirs.settings_file();
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(unavailable()),
        Ok(_) => {}
    }
    let mut bytes = Vec::new();
    private_file(&path, uid)?
        .take(MAX_SETTINGS + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable())?;
    if bytes.len() as u64 > MAX_SETTINGS {
        return Err(unavailable());
    }
    let settings: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
    let Some(id) = settings.pointer("/oxa/lead/sessionId") else {
        return Ok(None);
    };
    let id = id
        .as_str()
        .filter(|id| id.len() <= 128 && crate::args::valid_session_id(id))
        .ok_or_else(unavailable)?;
    Ok(Some(id.to_owned()))
}

fn header(path: &Path, uid: u32) -> Result<(SessionHeader, (u64, u64)), OxaError> {
    let file = private_file(path, uid)?;
    let metadata = file.metadata().map_err(|_| unavailable())?;
    let mut line = Vec::new();
    BufReader::new(file)
        .take(MAX_HEADER + 1)
        .read_until(b'\n', &mut line)
        .map_err(|_| unavailable())?;
    if line.len() as u64 > MAX_HEADER || line.last() != Some(&b'\n') {
        return Err(unavailable());
    }
    let header = decode_header(&line).map_err(|_| unavailable())?;
    Ok((header, (metadata.dev(), metadata.ino())))
}

impl LeadOwnerBinding {
    /// Load after verifying the transport's owner. The project is a trusted caller value.
    pub fn load(dirs: &Dirs, canonical_project: &Path, owner_uid: u32) -> Result<Option<Self>, OxaError> {
        let Some(id) = selection(dirs, owner_uid)? else {
            return Ok(None);
        };
        canonical(canonical_project)?;
        private_directory(&dirs.sessions_dir, owner_uid)?;
        let directory = dirs.sessions_dir.join(session_dir_name(canonical_project));
        private_directory(&directory, owner_uid)?;
        let mut selected = None;
        for entry in fs::read_dir(&directory).map_err(|_| unavailable())? {
            let path = entry.map_err(|_| unavailable())?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let (header, file) = header(&path, owner_uid)?;
            if header.id == id {
                if header.cwd != canonical_project || selected.is_some() {
                    return Err(unavailable());
                }
                selected = Some((path, file));
            }
        }
        let (path, file) = selected.ok_or_else(unavailable)?;
        Ok(Some(Self {
            dirs: dirs.clone(),
            project: canonical_project.to_owned(),
            uid: owner_uid,
            session: SessionRef { path, id },
            file,
        }))
    }

    fn recheck(&self) -> Result<(), OxaError> {
        canonical(&self.project)?;
        private_directory(&self.dirs.sessions_dir, self.uid)?;
        let (header, identity) = header(&self.session.path, self.uid)?;
        if identity != self.file
            || header.id != self.session.id
            || header.cwd != self.project
            || selection(&self.dirs, self.uid)?.as_deref() != Some(self.session.id.as_str())
        {
            return Err(unavailable());
        }
        Ok(())
    }
}

/// Use the exact native Session under its writer lock. The trusted Lead operation
/// sends chat with `Session::prompt_literal`.
pub fn with_lead_owner<T>(
    binding: &LeadOwnerBinding,
    env: &Env,
    run: impl FnOnce(&mut Session) -> Result<T, OxaError>,
) -> Result<T, OxaError> {
    let dirs = &binding.dirs;
    let cwd = &binding.project;
    binding.recheck()?;
    let store = Arc::new(FsStore::new(dirs.sessions_dir.clone()));
    // Stage the very same lock Session::open will adopt, before provider I/O.
    store.acquire(&binding.session).map_err(|error| {
        if error.code == oxa_session::LOCKED {
            OxaError::new(codes::BUSY, "The selected Lead session is busy.")
        } else {
            unavailable()
        }
    })?;
    binding.recheck()?;
    let mode = RunMode::Print;
    let home_workspace =
        crate::home_cwd::check(mode, cwd, env, dirs, &mut std::io::empty(), &mut std::io::sink())?;
    let mut args = Args {
        no_tools: true,
        no_skills: true,
        ..Args::default()
    };
    let prep = prepare(
        &mut args,
        env,
        dirs,
        cwd,
        mode,
        home_workspace,
        false,
        false,
        &mut |_, _| false,
        Default::default(),
        Some((&binding.session, store)),
    )
    .map_err(|_| unavailable())?;
    let mut session = open_named(&prep.built, None, prep.open, None).map_err(|_| unavailable())?;
    binding.recheck()?;
    if session.inner().sref != binding.session {
        return Err(unavailable());
    }
    let result = run(&mut session);
    session.shutdown(&mut oxa_proto::HeadlessUi::default());
    result
}
