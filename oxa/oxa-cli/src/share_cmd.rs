//! Foreground opt-in native LAN listener. Only user settings can select its engine.
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{codes, CancelToken, ChatRequest, Engine, OxaError, StreamEvent};
use oxc_gateway::share::{
    keys::Store,
    listener::{Access, Binding, Config, Failure, Model},
    native,
    tailnet::LocalApi,
};
use std::{
    io::Write,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const TAILSCALE_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";

pub(crate) fn is_agent(env: &Env) -> bool {
    // Marker presence, including an empty value, is enough to refuse.
    env.pairs().iter().any(|(name, _)| {
        matches!(
            name.as_str(),
            "OXA_CODING_AGENT" | "AI_AGENT" | "OXA_SUBAGENT_DEPTH"
        )
    })
}

pub(crate) struct Prepared {
    pub local: Arc<oxc_local::native::LocalEngine>,
    keys: Arc<Store>,
    ledger: oxc_gateway::ledger::Ledger,
    models: Vec<Model>,
    tailnet: Arc<LocalApi>,
    config: Config,
}

fn prepare(
    settings: &Settings,
    dirs: &Dirs,
    config: Config,
    aliases: Vec<String>,
    socket: PathBuf,
) -> Result<Prepared, OxaError> {
    oxa_proto::check_online("starting the LAN sharing listener")?;
    if aliases.is_empty() || aliases.len() > 64 {
        return Err(usage());
    }
    if !settings.bool(oxc_local::LOCAL_SETTING, false) {
        return Err(OxaError::new(
            codes::INVALID,
            "Enable the native local engine in user settings before serving models.",
        ));
    }
    let keys = Arc::new(
        Store::open(&dirs.home.join(".oxa/share"))
            .map_err(|error| OxaError::new(codes::INVALID, error.to_string()))?,
    );
    let local = oxc_local::configured_local_engine(
        settings.get("oxa.local"),
        dirs.agent_dir.join("local-engine"),
        oxa_engine::StreamOpts {
            silence_limit_ms: crate::dispatch::stream_silence_timeout(&settings)
                .unwrap_or(365 * 24 * 3600 * 1000),
            ..oxa_engine::StreamOpts::default()
        },
    )?;
    let catalog = local.info(None)?;
    let models = aliases
        .into_iter()
        .map(|alias| {
            if !catalog.models.contains(&alias) {
                return Err(OxaError::new(
                    codes::NOT_FOUND,
                    "The selected guest model is unavailable in the native local catalog.",
                ));
            }
            let info = local.info(Some(&alias))?;
            if info.caps.remote || info.limits_for != alias {
                return Err(OxaError::new(
                    codes::INVALID,
                    "The selected guest model has no local limits.",
                ));
            }
            Ok(Model {
                alias,
                context_window: info.n_ctx,
                max_output: info.max_output,
                access: Access::Local,
            })
        })
        .collect::<Result<Vec<_>, OxaError>>()?;
    let tailnet = Arc::new(
        LocalApi::new(socket, Duration::from_secs(5))
            .map_err(|error| OxaError::new(codes::INVALID, error.to_string()))?,
    );
    let ledger = oxc_gateway::ledger::Ledger::open(&dirs.agent_dir.join("cloud-ledger"))
        .map_err(|_| OxaError::new(codes::INVALID, "The private request ledger is unavailable."))?;
    Ok(Prepared {
        local,
        keys,
        ledger,
        models,
        tailnet,
        config,
    })
}

pub(crate) fn prepare_session(
    settings: &Settings,
    dirs: &Dirs,
    aliases: Vec<String>,
    bind: Option<IpAddr>,
) -> Result<Prepared, OxaError> {
    let config = Config {
        binding: bind.map_or(Binding::Tailnet, Binding::Explicit),
        ..Config::default()
    };
    prepare(settings, dirs, config, aliases, PathBuf::from(TAILSCALE_SOCKET))
}

impl Prepared {
    fn serve(
        self,
        stop: CancelToken,
        ready: impl FnOnce(SocketAddr) -> Result<(), Failure>,
    ) -> Result<(), OxaError> {
        let Self {
            local,
            keys,
            ledger,
            models,
            tailnet,
            config,
        } = self;
        let answer = native::Native::new(
            models,
            move |request: &ChatRequest<'_>, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)| {
                local.chat_guest(request, cancel, on)
            },
        )
        .map_err(service_error)?
        .with_ledger(ledger);
        native::serve(keys, Arc::new(answer), tailnet, config, stop, ready).map_err(service_error)
    }

    pub(super) fn start(self) -> Result<(Serving, SocketAddr), OxaError> {
        let stop = CancelToken::new();
        let thread_stop = stop.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .spawn(move || {
                self.serve(thread_stop, |address| {
                    send.send(address).map_err(|_| Failure::Failed)
                })
            })
            .map_err(|error| OxaError::io("cannot start the guest listener", &error))?;
        match receive.recv() {
            Ok(address) => Ok((
                Serving {
                    stop,
                    thread: Some(thread),
                },
                address,
            )),
            Err(_) => match thread.join() {
                Ok(Err(error)) => Err(error),
                _ => Err(service_error(Failure::Failed)),
            },
        }
    }
}

pub(super) struct Serving {
    stop: CancelToken,
    thread: Option<std::thread::JoinHandle<Result<(), OxaError>>>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) fn run(args: &[String], dirs: &Dirs, cwd: &Path, out: &mut dyn Write) -> Result<(), OxaError> {
    let (config, aliases, socket) = options(args)?;
    let settings = Settings::load(dirs, cwd, false);
    let prepared = prepare(&settings, dirs, config, aliases, socket)?;
    let stop = CancelToken::new();
    let mut signals =
        signal_hook::iterator::Signals::new([signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM])
            .map_err(|error| OxaError::io("cannot subscribe to listener shutdown signals", &error))?;
    let signal_handle = signals.handle();
    let signal_stop = stop.clone();
    let signal_thread = std::thread::Builder::new()
        .spawn(move || {
            if signals.forever().next().is_some() {
                signal_stop.cancel();
            }
        })
        .map_err(|error| OxaError::io("cannot start listener shutdown handler", &error))?;

    let result = prepared.serve(stop, |address| {
        writeln!(out, "Guest OpenAI API listening at http://{address}/v1")
            .and_then(|()| out.flush())
            .map_err(|_| Failure::Failed)
    });
    signal_handle.close();
    let _ = signal_thread.join();
    result
}

fn options(args: &[String]) -> Result<(Config, Vec<String>, PathBuf), OxaError> {
    let mut config = Config::default();
    let mut aliases = Vec::new();
    let mut socket = PathBuf::from(TAILSCALE_SOCKET);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--same-owner" {
            config.same_owner = true;
            continue;
        }
        let value = args.next().filter(|value| !value.is_empty()).ok_or_else(usage)?;
        match arg.as_str() {
            "--model" if !aliases.contains(value) && aliases.len() < 64 => aliases.push(value.clone()),
            "--bind" => config.binding = Binding::Explicit(value.parse().map_err(|_| usage())?),
            "--port" => config.port = value.parse().map_err(|_| usage())?,
            "--tailscale-socket" => socket = PathBuf::from(value),
            _ => return Err(usage()),
        }
    }
    if aliases.is_empty() {
        return Err(usage());
    }
    Ok((config, aliases, socket))
}

fn usage() -> OxaError {
    OxaError::new(
        codes::USAGE,
        "Choose at least one native local guest model with --model ALIAS. See oxa share serve --help.",
    )
}

fn service_error(error: Failure) -> OxaError {
    let message = match error {
        Failure::Forbidden => "No eligible guest model or usable tailnet address.",
        Failure::BadRequest => "Invalid guest listener configuration.",
        Failure::Busy => "Guest listener could not bind or tailscaled is unavailable.",
        _ => "Guest listener stopped unexpectedly.",
    };
    OxaError::new(codes::INVALID, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_local::native::LocalEngine;

    #[test]
    fn owner_router_and_listener_use_the_same_local_engine() {
        let home = oxa_fake::TestEnv::new();
        let dirs = Dirs::under(home.root());
        let settings = Settings::from_values(
            serde_json::json!({"oxa":{
                "engine":{"inProcessLocal":true},
                "local":{"binary":"/synthetic/oxe-server",
                         "models":{"synthetic":{"model":"/synthetic/model.gguf",
                                               "arguments":["--engine","stub"]}},
                         "limits":{"synthetic":{"context_window":4096,"max_output":64,
                            "allocation_budget":{"kv_bytes_per_position":1,"scratch_bytes":0,
                                                 "mtp_state_bytes":0,"margin_bytes":1044480}}}}
            }}),
            serde_json::json!({}),
        );
        Store::initialize(&dirs.home.join(".oxa/share")).unwrap();
        let prepared = prepare_session(
            &settings,
            &dirs,
            vec!["synthetic".into()],
            Some(std::net::Ipv4Addr::LOCALHOST.into()),
        )
        .unwrap();
        assert!(dirs.agent_dir.join("cloud-ledger/.salt").is_file());
        let local: Arc<LocalEngine> = prepared.local.clone();
        let owner = crate::in_process::select_observed(
            &settings,
            &settings,
            &dirs,
            Some("synthetic"),
            None,
            Some(local.clone()),
            || panic!("owner should use the shared native engine"),
        )
        .unwrap();
        let local_api: Arc<dyn Engine> = local;
        assert!(Arc::ptr_eq(&owner, &local_api));
        let (running, address) = prepared.start().unwrap();
        assert!(std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_ok());
        drop(running);
        assert!(std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_err());
    }
}
