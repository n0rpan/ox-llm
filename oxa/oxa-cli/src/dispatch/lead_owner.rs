//! Protected selection of the native Session shared by the owner chat callers.
use super::{open_named, prepare, Args, RunMode};
use oxa_config::{Dirs, Env, Settings};
use oxa_core::Session;
use oxa_proto::{codes, Entry, EntryBody, OxaError, Value};
use oxa_session::{
    decode_entry, decode_header, new_entry_id, session_dir_name, FsStore, SessionRef, SessionStore,
};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_SETTINGS: u64 = 1_048_576;
const MAX_LINE: u64 = 65_536;
const ORIGIN: &str = "oxa.lead.owner";

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

fn read_line(reader: &mut impl BufRead) -> Result<Vec<u8>, OxaError> {
    let mut line = Vec::new();
    reader
        .take(MAX_LINE + 1)
        .read_until(b'\n', &mut line)
        .map_err(|_| unavailable())?;
    if line.len() as u64 > MAX_LINE || line.last() != Some(&b'\n') {
        return Err(unavailable());
    }
    Ok(line)
}

fn dedicated_file(path: &Path, uid: u32, id: &str, project: &Path) -> Result<Option<(u64, u64)>, OxaError> {
    let file = private_file(path, uid)?;
    let metadata = file.metadata().map_err(|_| unavailable())?;
    let mut reader = BufReader::new(file);
    let header = decode_header(&read_line(&mut reader)?).map_err(|_| unavailable())?;
    if header.id != id {
        return Ok(None);
    }
    let origin = decode_entry(&read_line(&mut reader)?).map_err(|_| unavailable())?;
    if header.cwd != project
        || header.parent_session.is_some()
        || origin.parent.is_some()
        || !matches!(origin.body, EntryBody::Custom { custom_type, data }
            if custom_type == ORIGIN && data.as_str() == Some(id))
    {
        return Err(unavailable());
    }
    Ok(Some((metadata.dev(), metadata.ino())))
}

impl LeadOwnerBinding {
    /// Explicit operator action after owner authentication. Returns a new ID for
    /// user settings. Opens no provider and imports no history, model, or goal.
    pub fn create(dirs: &Dirs, canonical_project: &Path, owner_uid: u32) -> Result<String, OxaError> {
        canonical(canonical_project)?;
        if !canonical_project.is_dir() {
            return Err(unavailable());
        }
        let parent = dirs.sessions_dir.parent().ok_or_else(unavailable)?;
        private_directory(parent, owner_uid)?;
        let directory = dirs.sessions_dir.join(session_dir_name(canonical_project));
        for path in [&dirs.sessions_dir, &directory] {
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(unavailable()),
            }
            private_directory(path, owner_uid)?;
        }
        let store = FsStore::new(dirs.sessions_dir.clone());
        let session = store
            .create(canonical_project, None, None)
            .map_err(|_| unavailable())?;
        store.acquire(&session).map_err(|_| unavailable())?;
        store
            .append(
                &session,
                &Entry {
                    id: new_entry_id(&|_| false),
                    parent: None,
                    ts_ms: oxa_proto::time::now_ms(),
                    body: EntryBody::Custom {
                        custom_type: ORIGIN.into(),
                        data: Value::String(session.id.clone()),
                    },
                },
            )
            .and_then(|()| store.sync(&session))
            .map_err(|_| unavailable())?;
        for directory in [directory.as_path(), dirs.sessions_dir.as_path(), parent] {
            File::open(directory)
                .and_then(|file| file.sync_all())
                .map_err(|_| unavailable())?;
        }
        Ok(session.id)
    }

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
            if let Some(file) = dedicated_file(&path, owner_uid, &id, canonical_project)? {
                if selected.is_some() {
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
        if dedicated_file(&self.session.path, self.uid, &self.session.id, &self.project)? != Some(self.file)
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
    // The serving process owns the normal startup policy. Refuse a mismatch
    // before provider preparation, without changing policy for other requests.
    let (offline, remote) = crate::netguard::policy_from(false, env, &Settings::load(dirs, cwd, false));
    if offline && !oxa_proto::offline() {
        return Err(oxa_proto::offline_error("Lead chat"));
    }
    if !remote && oxa_proto::remote_engine_allowed() {
        return Err(unavailable());
    }
    let mode = RunMode::Print;
    let home_workspace =
        crate::home_cwd::check(mode, cwd, env, dirs, &mut std::io::empty(), &mut std::io::sink())?;
    let mut args = Args {
        no_extensions: true,
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
