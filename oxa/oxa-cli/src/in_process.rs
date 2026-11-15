//! Lazy composition boundary for the opt-in, cloud-only gateway.
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{Engine, OxaError};
use std::sync::Arc;

/// User-only opt-in; a project cannot choose a new cloud transport.
pub const SETTING_IN_PROCESS_CLOUD: &str = "oxa.engine.inProcessCloud";

/// The binary supplies the shared gateway runtime/Engine bridge here.
/// Called only after the user's explicit opt-in and the offline check.
pub trait InProcessEngineFactory: Send + Sync {
    fn open(&self, env: &Env, dirs: &Dirs) -> Result<Arc<dyn Engine>, OxaError>;
}

pub(crate) fn select_engine(
    user: &Settings,
    env: &Env,
    dirs: &Dirs,
    factory: Option<&dyn InProcessEngineFactory>,
    http: impl FnOnce() -> Result<Arc<dyn Engine>, OxaError>,
) -> Result<Arc<dyn Engine>, OxaError> {
    if !user.bool(SETTING_IN_PROCESS_CLOUD, false) {
        return http();
    }
    if oxa_proto::offline() {
        return Err(oxa_proto::offline_error("the in-process cloud gateway"));
    }
    let factory =
        factory.ok_or_else(|| OxaError::unsupported("in-process cloud gateway without a linked runtime"))?;
    factory.open(env, dirs)
}
