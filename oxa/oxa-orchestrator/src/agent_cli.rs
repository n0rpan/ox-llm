//! Explicit foreground host. No startup discovery, daemon install or task replay.
use crate::{
    registration::AgentHost, rpc_agent::RpcAgentConfig, LocalInference, PermissionMode, WorkerBudget,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

const HELP: &str = "Usage: oxa orchestrate-agent --engine <loopback:port> --model <id> --local-only --directory <new-path> [options]\n\nHost a foreground local offline A2A agent independently of its controllers.\nWrite private authentication to <directory>/registration.json; never print it.\nTools are disabled. Ctrl-C/TERM stops and joins owned tasks. No service is installed.\n\nOptions:\n  --parallel <1..16>            Concurrent tasks (default 4)\n  --capacity <1..128>           Total retained tasks (default 32)\n  --worker-timeout <1..600>     Seconds per task (default 30)\n  --reported-tokens <n>         Observed per-task cap (default 200000; not money)\n  --lifetime <1..86400>         Seconds before host shutdown (default 300)\n  --offline                    Always enabled\n  --help                       Inert help\n";

struct Options {
    inference: LocalInference,
    directory: PathBuf,
    parallel: usize,
    capacity: usize,
    worker_timeout: Duration,
    tokens: u64,
    lifetime: Duration,
}
fn parse(args: &[String]) -> Result<Option<Options>, &'static str> {
    let mut values = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut local = false;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        if flag == "--help" || flag == "-h" {
            return Ok(None);
        }
        if flag == "--offline" {
            continue;
        }
        if flag == "--local-only" {
            local = true;
            continue;
        }
        if !matches!(
            flag,
            "--engine"
                | "--model"
                | "--directory"
                | "--parallel"
                | "--capacity"
                | "--worker-timeout"
                | "--reported-tokens"
                | "--lifetime"
        ) {
            return Err("unsupported agent option");
        }
        if !seen.insert(flag) {
            return Err("duplicate agent option");
        }
        let value = args
            .get(i)
            .filter(|s| !s.is_empty())
            .ok_or("missing agent option value")?;
        i += 1;
        values.insert(flag, value.as_str());
    }
    if !local {
        return Err("--local-only is required; loopback alone does not exclude cloud routing");
    }
    let required = |key| {
        values
            .get(key)
            .copied()
            .ok_or("--engine, --model and --directory are required")
    };
    let address: SocketAddr = required("--engine")?
        .parse()
        .map_err(|_| "engine must be a literal loopback address:port")?;
    let inference = LocalInference::register_verified_local(address, required("--model")?.into())
        .map_err(|_| "invalid verified local inference route")?;
    let number = |key, default, max| -> Result<u64, &'static str> {
        let value = values
            .get(key)
            .map(|v| v.parse::<u64>())
            .transpose()
            .map_err(|_| "invalid integer agent option")?
            .unwrap_or(default);
        if value == 0 || value > max {
            return Err("agent option outside supported bounds");
        }
        Ok(value)
    };
    let parallel = number("--parallel", 4, 16)? as usize;
    let capacity = number("--capacity", 32, 128)? as usize;
    if capacity < parallel {
        return Err("capacity must cover parallel tasks");
    }
    Ok(Some(Options {
        inference,
        directory: required("--directory")?.into(),
        parallel,
        capacity,
        worker_timeout: Duration::from_secs(number("--worker-timeout", 30, 600)?),
        tokens: number("--reported-tokens", 200_000, 100_000_000)?,
        lifetime: Duration::from_secs(number("--lifetime", 300, 86400)?),
    }))
}
pub(crate) fn run(args: &[String]) -> i32 {
    let options = match parse(args) {
        Ok(None) => {
            let _ = io::stdout().write_all(HELP.as_bytes());
            return 0;
        }
        Ok(Some(options)) => options,
        Err(error) => {
            crate::cli::diagnostic(format!("oxa orchestrate-agent: {error}\n").as_bytes());
            return 2;
        }
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let signals = match crate::cli::Signals::install(cancel.clone()) {
        Ok(signals) => signals,
        Err(_) => {
            crate::cli::diagnostic(b"oxa orchestrate-agent: cannot install cancellation handlers\n");
            return 1;
        }
    };
    let code = execute(options, cancel).map_or_else(
        |error| {
            crate::cli::diagnostic(format!("oxa orchestrate-agent: {error}\n").as_bytes());
            1
        },
        |_| 0,
    );
    let signal = crate::cli::received_signal();
    drop(signals);
    if signal == 0 {
        code
    } else {
        128 + signal
    }
}
fn execute(options: Options, cancel: Arc<AtomicBool>) -> io::Result<()> {
    let deadline = crate::resume::boot_time_ns()?
        .checked_add(options.lifetime.as_nanos() as u64)
        .ok_or_else(|| io::Error::other("invalid host lifetime"))?;
    let host = AgentHost::start(RpcAgentConfig {
        worker_binary: std::env::current_exe()?,
        bwrap_binary: "/usr/bin/bwrap".into(),
        directory: options.directory,
        role: "Handle explicitly delegated local task data".into(),
        inference: options.inference,
        mode: PermissionMode::Plan,
        workspace: None,
        budget: WorkerBudget {
            wall_time: options.worker_timeout,
            output_bytes: 1024 * 1024,
            reported_tokens: options.tokens,
        },
        max_live: options.parallel,
        max_tasks: options.capacity,
        journal_bytes: 1024 * 1024,
    })?;
    crate::cli::diagnostic(b"oxa orchestrate-agent: ready; private registration.json saved\n");
    while !cancel.load(Ordering::Acquire) && crate::resume::boot_time_ns()? < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(host);
    Ok(())
}
