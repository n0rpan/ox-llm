//! W0-FROZEN (architect-owned).
//!
//! # oxa-config
//! `settings.json` (user + project merge), `keybindings.json`, `models.json`, the `OXA_*`
//! environment table and the directory layout.
//!
//! ## Contracts
//! * **Test isolation (P56).** No code in the workspace reads `HOME`, `XDG_*` or `OXA_*` from the
//!   process environment except [`Env::from_process`], called once in `oxa-cli`'s `main`.
//!   Everything else receives [`Dirs`] and [`Env`] values. Tests build them with [`Dirs::under`]
//!   and [`Env::from_pairs`].
//! * **Never fail on configuration.** An unknown key, a wrong type or an unreadable file is a
//!   [`Diagnostic`]; the value falls back to its default. Only an explicit CLI argument can be
//!   a hard error.
//! * **W3.** Settings are read once per prefix epoch. Nothing here watches files.
//! * Keys and precedence are pi's (settings.md); oxa's own keys live under `oxa.*`.
#![forbid(unsafe_code)]

mod descriptor;
mod dirs;
mod env;
mod keybindings;
mod models;
mod profile;
mod schema;
mod settings;

pub use descriptor::{
    descriptor, descriptors, fields_of, in_group, Apply, Control, Descriptor, Group, Item, MapKind, Options,
    Range, Unit, WebEdit,
};
pub use dirs::dirs_from_env;
pub use env::{env_flag, Env, EnvFlag, EnvVar, ENV_TABLE, FALSE_WORDS, TRUE_WORDS};
pub use keybindings::{ActionDef, Keymap};
pub use models::{
    resolve_model, resolve_scoped, valid_cloud_name, CloudApi, CloudModel, CloudPrice, CloudProvider,
    Endpoint, ModelRef, ModelsConfig,
};
pub use profile::{ThinkingPolicy, BASH_TIMEOUT_DEFAULT, SUMMARY_TARGET_DEFAULT};
pub use schema::{apply_list_patch, merge_settings, schema, validate, KeyDef, Kind};
pub use settings::{CompactionSettings, ProviderRetrySettings, QueueMode, RetrySettings, Settings, Source};

use oxa_proto::Level;
use std::path::{Path, PathBuf};

/// A non-fatal configuration problem, shown once at startup and by `/debug`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: Level,
    /// File path or flag the problem came from.
    pub source: String,
    pub message: String,
}

/// Every directory oxa uses, resolved once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    /// The user's home directory.
    pub home: PathBuf,
    /// `~/.oxa/agent` or `$OXA_AGENT_DIR`.
    pub agent_dir: PathBuf,
    /// `<agent_dir>/sessions` or `--session-dir` / `sessionDir`.
    pub sessions_dir: PathBuf,
    /// Spill files, full bash outputs, debug files. Private (0700).
    pub tmp_dir: PathBuf,
    /// O-152: the system skills directory read as the lowest-precedence skill source (Omarchy's
    /// [`OMARCHY_SKILLS_DIR`], or `$OXA_SYSTEM_SKILLS_DIR`; empty = none). `None` in
    /// [`Dirs::under`], so tests never read the real system directory.
    pub system_skills_dir: Option<PathBuf>,
}

/// Omarchy's shared agent skills (`/usr/share/omarchy/default/agents/skills`), read after every
/// other skill source when it exists.
pub const OMARCHY_SKILLS_DIR: &str = "/usr/share/omarchy/default/agents/skills";

impl Dirs {
    /// A self-contained layout under `root` (tests, and `oxa-fake`'s `TestEnv`).
    pub fn under(root: &Path) -> Dirs {
        let home = root.join("home");
        let agent_dir = home.join(oxa_proto::names::AGENT_DIR);
        Dirs {
            sessions_dir: agent_dir.join("sessions"),
            tmp_dir: root.join("tmp"),
            agent_dir,
            home,
            system_skills_dir: None,
        }
    }
    pub fn settings_file(&self) -> PathBuf {
        self.agent_dir.join("settings.json")
    }
    pub fn keybindings_file(&self) -> PathBuf {
        self.agent_dir.join("keybindings.json")
    }
    pub fn models_file(&self) -> PathBuf {
        self.agent_dir.join("models.json")
    }
    pub fn mcp_file(&self) -> PathBuf {
        self.agent_dir.join("mcp.json")
    }
    pub fn trust_file(&self) -> PathBuf {
        self.agent_dir.join("trust.json")
    }
    /// `<cwd>/.oxa`.
    pub fn project_dir(&self, cwd: &Path) -> PathBuf {
        cwd.join(oxa_proto::names::PROJECT_DIR)
    }
    pub fn project_settings_file(&self, cwd: &Path) -> PathBuf {
        self.project_dir(cwd).join("settings.json")
    }
}
