//! Foreground opt-in native LAN listener. Only user settings can select its engine.
use oxa_config::{Dirs, Settings};
use oxa_proto::{codes, CancelToken, ChatRequest, Engine, OxaError, StreamEvent};
use oxc_gateway::share::{
    keys::Store,
    listener::{Access, Binding, Config, Failure, Model},
    native,
    tailnet::LocalApi,
};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub(crate) fn run(args: &[String], dirs: &Dirs, cwd: &Path, out: &mut dyn Write) -> Result<(), OxaError> {
    oxa_proto::check_online("starting the LAN sharing listener")?;
    let (config, aliases, socket) = options(args)?;
    let settings = Settings::load(dirs, cwd, false);
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
    let answer = native::Native::new(
        models,
        move |request: &ChatRequest<'_>, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)| {
            local.chat_guest(request, cancel, on)
        },
    )
    .map_err(service_error)?;
    let tailnet = LocalApi::new(socket, Duration::from_secs(5))
        .map_err(|error| OxaError::new(codes::INVALID, error.to_string()))?;

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

    let result = native::serve(
        keys,
        Arc::new(answer),
        Arc::new(tailnet),
        config,
        stop,
        |address| {
            writeln!(out, "Guest OpenAI API listening at http://{address}/v1")
                .and_then(|()| out.flush())
                .map_err(|_| Failure::Failed)
        },
    );
    signal_handle.close();
    let _ = signal_thread.join();
    result.map_err(service_error)
}

fn options(args: &[String]) -> Result<(Config, Vec<String>, PathBuf), OxaError> {
    let mut config = Config::default();
    let mut aliases = Vec::new();
    let mut socket = PathBuf::from("/var/run/tailscale/tailscaled.sock");
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
        Failure::Forbidden => "Use loopback or an assigned Tailscale address for LAN sharing.",
        Failure::BadRequest => "Invalid guest listener configuration.",
        Failure::Busy => "Guest listener could not bind or tailscaled is unavailable.",
        _ => "Guest listener stopped unexpectedly.",
    };
    OxaError::new(codes::INVALID, message)
}
