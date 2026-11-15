//! Immutable operator authority for session worker lanes, never model arguments.
use crate::workspace::RelativePath;
use crate::{goals, LocalInference};
use oxa_core::Deps;
use serde::{Deserialize, Serialize};
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
    #[serde(default)]
    pub allow_cloud_workers: bool,
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
    #[serde(default)]
    pub route: Route,
    #[serde(default)]
    pub auto_compaction: bool,
    #[serde(default)]
    pub response_tokens: Option<u32>,
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
#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Route {
    #[default]
    Local,
    CloudGateway,
}
impl Route {
    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::CloudGateway => "cloud",
        }
    }
}
impl Profile {
    pub fn inference(&self) -> io::Result<LocalInference> {
        let route = match self.route {
            Route::Local => LocalInference::register_verified_local(self.engine, self.model.clone()),
            Route::CloudGateway => LocalInference::register_operator_gateway(self.engine, self.model.clone()),
        }?;
        let route = route.with_compaction(self.auto_compaction);
        match self.response_tokens {
            Some(tokens) => route.with_response_tokens(tokens),
            None => Ok(route),
        }
    }
    pub fn check_policy(&self) -> io::Result<()> {
        if self.route == Route::CloudGateway && oxa_proto::offline() {
            return Err(io::Error::other("offline mode refuses cloud worker inference"));
        }
        Ok(())
    }
    pub fn tokens(&self) -> u64 {
        self.budget.tokens.or(self.reported_tokens).unwrap_or(200_000)
    }
}
fn invalid() -> io::Error {
    io::Error::other(
        "invalid private worker profiles; explicit route authorization and bounded grants are required",
    )
}
pub(crate) fn load(path: &Path, deps: &Deps) -> io::Result<Config> {
    load_private(path, &deps.dirs.agent_dir, &deps.cwd)
}
fn load_private(path: &Path, agent_dir: &Path, cwd: &Path) -> io::Result<Config> {
    if !path.is_absolute() {
        return Err(invalid());
    }
    let parent_path = path.parent().ok_or_else(invalid)?;
    let parent = parent_path.canonicalize()?;
    let agent = agent_dir.canonicalize()?;
    if parent != parent_path
        || agent != agent_dir
        || !parent.starts_with(&agent)
        || parent.starts_with(cwd.canonicalize()?)
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
    let cloud = config
        .profiles
        .iter()
        .any(|profile| profile.route == Route::CloudGateway);
    if !matches!(config.version, 1 | 2)
        || (config.version == 1 && (cloud || config.allow_cloud_workers))
        || config.verified_local_only == cloud
        || config.allow_cloud_workers != cloud
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
            || profile.response_tokens == Some(0)
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
        LocalInference::validate_route(profile.engine, &profile.model)?;
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
    fn private_profile_authority_rejects_links_public_modes_and_workspace_paths() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let env = oxa_fake::TestEnv::new();
        let agent = oxa_config::Dirs::under(env.root()).agent_dir;
        let private = agent.join("profiles");
        std::fs::create_dir_all(&private).unwrap();
        let path = private.join("workers.json");
        std::fs::write(&path, config().to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_private(&path, &agent, &env.work()).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_private(&path, &agent, &env.work()).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let hardlink = private.join("hardlink.json");
        std::fs::hard_link(&path, &hardlink).unwrap();
        assert!(load_private(&path, &agent, &env.work()).is_err());
        std::fs::remove_file(&hardlink).unwrap();
        let alias = private.join("alias.json");
        symlink(&path, &alias).unwrap();
        assert!(load_private(&alias, &agent, &env.work()).is_err());
        let parent_alias = agent.join("alias");
        symlink(&private, &parent_alias).unwrap();
        assert!(load_private(&parent_alias.join("workers.json"), &agent, &env.work()).is_err());
        assert!(load_private(&path, &agent, &agent).is_err());
        let outside = env.work().join("workers.json");
        std::fs::copy(&path, &outside).unwrap();
        assert!(load_private(&outside, &agent, &env.work()).is_err());
        assert!(load_private(Path::new("workers.json"), &agent, &env.work()).is_err());
        assert!(load_private(&path, &agent, &env.work()).is_ok());
    }
    #[test]
    fn response_allowance_is_optional_positive_operator_authority() {
        let legacy: Config = serde_json::from_value(config()).unwrap();
        assert_eq!(legacy.profiles[0].response_tokens, None);
        let mut limited = config();
        limited["profiles"][0]["response_tokens"] = json!(2048);
        assert!(accepted(limited.clone()));
        let profile: Config = serde_json::from_value(limited).unwrap();
        assert!(profile.profiles[0].inference().is_ok());
        for value in [json!(0), json!(-1), json!("512"), json!(u64::MAX)] {
            let mut invalid = config();
            invalid["profiles"][0]["response_tokens"] = value;
            assert!(!accepted(invalid));
        }
    }
    #[test]
    fn cloud_routes_require_explicit_versioned_operator_authorization() {
        let mut cloud = config();
        cloud["version"] = json!(2);
        cloud["verified_local_only"] = json!(false);
        cloud["allow_cloud_workers"] = json!(true);
        cloud["profiles"][0]["route"] = json!("cloud_gateway");
        assert!(accepted(cloud.clone()));
        for (field, value) in [
            ("version", json!(1)),
            ("verified_local_only", json!(true)),
            ("allow_cloud_workers", json!(false)),
        ] {
            let mut missing = cloud.clone();
            missing[field] = value;
            assert!(!accepted(missing), "{field}");
        }
        cloud["profiles"][0]["engine"] = json!("192.0.2.1:8080");
        assert!(!accepted(cloud));
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
            ("response_tokens", json!(0)),
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
