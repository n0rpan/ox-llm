//! Architect-owned (W0, implemented): `CodemodeHooks`, the `Hooks` source that offers the
//! codemode and tool-search tools, and the shared state the tasks' modules use.
use crate::{CallToolTool, CodemodeTool, ToolSearchTool, WorkerPool};
use oxa_proto::{codes, CommandSpec, HookPoint, HookReply, Host, OxaError, Tool, ToolSpec, Ui, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Returns every tool a script may call right now (built-ins, extension tools, MCP tools with
/// any exposure except `Hidden`), sorted by name. Given by the embedder (`oxa-cli`); called at
/// script start and at each search, so MCP servers that connected later are visible without a
/// new epoch.
pub type Catalog = Arc<dyn Fn() -> Vec<ToolSpec> + Send + Sync>;

/// One namespace of the catalog (an MCP server), for `describeNamespace`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Namespace {
    pub name: String,
    pub description: Option<String>,
    pub instructions: Option<String>,
}

/// Returns the namespaces known right now, sorted by name.
pub type Namespaces = Arc<dyn Fn() -> Vec<Namespace> + Send + Sync>;

/// Which of the tools are offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CodemodeConfig {
    /// Offer `codemode`.
    pub codemode: bool,
    /// Offer `tool_search` and `call_tool`.
    pub tool_search: bool,
    pub max_workers: usize,
}

/// State shared by the tools of this crate.
pub struct Shared {
    pub catalog: Catalog,
    pub namespaces: Namespaces,
    /// Set by `Hooks::attach`.
    pub host: Mutex<Option<Arc<dyn Host>>>,
    /// `store()` values, key -> JSON. Loaded from the newest `codemode-store` entry of the
    /// active branch when the host attaches; written back as a new entry by each successful
    /// script that stored something.
    pub store: Mutex<BTreeMap<String, Value>>,
    /// Names of deferred tools a `tool_search` of this session has matched; only these may be
    /// called through `call_tool`.
    pub matched: Mutex<BTreeSet<String>>,
    pub pool: WorkerPool,
    /// Where truncated script output is spilled.
    pub spill_dir: PathBuf,
}

impl Shared {
    pub fn host(&self) -> Result<Arc<dyn Host>, OxaError> {
        self.host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| OxaError::new(codes::BUSY, "no session is attached"))
    }
}

/// codemode + tool search as one `Hooks` source.
pub struct CodemodeHooks {
    pub(crate) config: CodemodeConfig,
    pub(crate) shared: Arc<Shared>,
}

impl CodemodeHooks {
    pub fn new(
        config: CodemodeConfig,
        catalog: Catalog,
        namespaces: Namespaces,
        spill_dir: PathBuf,
    ) -> CodemodeHooks {
        let workers = if config.max_workers == 0 {
            crate::DEFAULT_MAX_WORKERS
        } else {
            config.max_workers
        };
        CodemodeHooks {
            config,
            shared: Arc::new(Shared {
                catalog,
                namespaces,
                host: Mutex::new(None),
                store: Mutex::new(BTreeMap::new()),
                matched: Mutex::new(BTreeSet::new()),
                pool: WorkerPool::new(workers),
                spill_dir,
            }),
        }
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }
}

impl oxa_proto::Hooks for CodemodeHooks {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    /// `codemode` when enabled; `tool_search` and `call_tool` when enabled. Constant specs (W3).
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut v: Vec<Arc<dyn Tool>> = Vec::new();
        if self.config.codemode {
            v.push(Arc::new(CodemodeTool::new(self.shared.clone())));
        }
        if self.config.tool_search {
            v.push(Arc::new(ToolSearchTool::new(self.shared.clone())));
            v.push(Arc::new(CallToolTool::new(self.shared.clone())));
        }
        v
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Err(OxaError::new(
            codes::NOT_FOUND,
            format!("unknown command: /{name}"),
        ))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// No hook point is used (`fire` always continues).
    fn wants(&self, _p: HookPoint) -> bool {
        false
    }
    fn name(&self) -> &str {
        "codemode"
    }
    /// Keeps the host and reloads the store from the active branch
    /// ([`crate::api::load_store`]).
    fn attach(&self, host: Arc<dyn Host>) {
        *self.shared.host.lock().unwrap_or_else(|p| p.into_inner()) = Some(host.clone());
        self.shared
            .matched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        let loaded = crate::api::load_store(host.as_ref());
        *self.shared.store.lock().unwrap_or_else(|p| p.into_inner()) = loaded;
    }
    fn shutdown(&self) {
        self.shared.pool.shutdown();
    }
}
