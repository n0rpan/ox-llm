//! Immutable operator authority for session worker lanes, never model arguments.
use crate::workspace::RelativePath;
use crate::{goals, LocalInference};
use oxa_core::Deps;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io,
    net::SocketAddr,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub version: u32,
    pub verified_local_only: bool,
    pub parallel: usize,
    pub total_tokens: u64,
    pub total_output_bytes: usize,
    pub profiles: Vec<Profile>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub name: String,
    pub role: String,
    pub engine: SocketAddr,
    pub model: String,
    pub budget: goals::Budget,
    pub turn_ms: u64,
    pub output_bytes: usize,
    #[serde(default)]
    pub reported_tokens: Option<u64>,
    #[serde(default)]
    pub files: Vec<RelativePath>,
    #[serde(default)]
    pub edit: bool,
    #[serde(default)]
    pub allow_new: Vec<RelativePath>,
}
impl Profile {
    pub fn tokens(&self) -> u64 {
        self.budget.tokens.or(self.reported_tokens).unwrap_or(200_000)
    }
}
fn invalid() -> io::Error {
    io::Error::other("invalid private worker profiles; verified local routes and bounded grants are required")
}
pub(crate) fn load(path: &Path, deps: &Deps) -> io::Result<Config> {
    if !path.is_absolute() {
        return Err(invalid());
    }
    let parent = path.parent().ok_or_else(invalid)?.canonicalize()?;
    let agent = deps.dirs.agent_dir.canonicalize()?;
    if agent != deps.dirs.agent_dir
        || !parent.starts_with(&agent)
        || parent.starts_with(deps.cwd.canonicalize()?)
    {
        return Err(invalid());
    }
    let _namespace = crate::namespace::protected_directory(&parent)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent.join(path.file_name().ok_or_else(invalid)?))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 65536
    {
        return Err(invalid());
    }
    let bytes = crate::recovery::read_handle(&file, 65536)?;
    let config: Config = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    validate(config)
}
fn validate(config: Config) -> io::Result<Config> {
    if config.version != 1
        || !config.verified_local_only
        || !(1..=4).contains(&config.parallel)
        || !(1..=64_000_000).contains(&config.total_tokens)
        || !(65536..=64 * 1024 * 1024).contains(&config.total_output_bytes)
        || config.profiles.is_empty()
        || config.profiles.len() > 8
    {
        return Err(invalid());
    }
    let mut names = BTreeSet::new();
    for profile in &config.profiles {
        if profile.name.is_empty()
            || profile.name.len() > 64
            || !profile
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            || !names.insert(profile.name.clone())
            || profile.role.trim().is_empty()
            || profile.role.len() > 4096
            || profile.role.contains('\0')
            || profile.model.len() > 256
            || profile.turn_ms == 0
            || profile.turn_ms > 600_000
            || profile.turn_ms > profile.budget.wall_ms
            || !(65536..=4 * 1024 * 1024).contains(&profile.output_bytes)
            || profile.output_bytes > config.total_output_bytes
            || profile.tokens() == 0
            || profile.tokens() > config.total_tokens
            || profile
                .budget
                .tokens
                .zip(profile.reported_tokens)
                .is_some_and(|(a, b)| a != b)
            || profile.files.len() > 32
            || profile.allow_new.len() > 32
            || (profile.edit && profile.files.is_empty())
            || (!profile.edit && !profile.allow_new.is_empty())
        {
            return Err(invalid());
        }
        goals::Goal::new("preflight".into(), "worker goal".into(), profile.budget.clone())
            .map_err(|_| invalid())?;
        LocalInference::register_verified_local(profile.engine, profile.model.clone())?;
        let unique: BTreeSet<_> = profile.files.iter().map(|p| p.as_str()).collect();
        let added: BTreeSet<_> = profile.allow_new.iter().map(|p| p.as_str()).collect();
        if unique.len() != profile.files.len()
            || added.len() != profile.allow_new.len()
            || !unique.is_disjoint(&added)
        {
            return Err(invalid());
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    fn config() -> Value {
        json!({"version":1,"verified_local_only":true,"parallel":2,"total_tokens":100000,"total_output_bytes":1048576,"profiles":[{"name":"author","role":"fixed role","engine":"127.0.0.1:8080","model":"fake","budget":{"wall_ms":5000,"tokens":1000,"turns":8},"turn_ms":1000,"output_bytes":65536}]})
    }
    fn accepted(value: Value) -> bool {
        serde_json::from_value(value)
            .ok()
            .is_some_and(|config| validate(config).is_ok())
    }
    #[test]
    fn operator_routes_and_pool_bounds_are_validated_before_installation() {
        assert!(accepted(config()));
        for (field, value) in [
            ("verified_local_only", json!(false)),
            ("parallel", json!(5)),
            ("total_tokens", json!(0)),
            ("total_output_bytes", json!(65535)),
        ] {
            let mut config = config();
            config[field] = value;
            assert!(!accepted(config), "{field}");
        }
        for (field, value) in [
            ("engine", json!("8.8.8.8:8080")),
            ("model", json!("fake\nmodel")),
            ("turn_ms", json!(6000)),
            ("reported_tokens", json!(999)),
            ("files", json!([".env"])),
            ("allow_new", json!(["new.rs"])),
            ("output_bytes", json!(1048577)),
        ] {
            let mut config = config();
            config["profiles"][0][field] = value;
            assert!(!accepted(config), "{field}");
        }
        let mut duplicate = config();
        let profile = duplicate["profiles"][0].clone();
        duplicate["profiles"].as_array_mut().unwrap().push(profile);
        assert!(!accepted(duplicate));
        let mut unknown = config();
        unknown["profiles"][0]["cloud_key"] = json!("never accepted");
        assert!(!accepted(unknown));
    }
}
