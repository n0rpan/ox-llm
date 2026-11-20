//! O-152: `oxa identity` (architect-owned). Writes `<agent dir>/identity.md`, the identity
//! section of the frozen prefix, and the built-in skills. Run by the ox-llm install and update.
use oxa_config::{Dirs, Env, ModelsConfig};
use oxa_core::prompt::{identity_text, IdentityFacts, IdentityModel, IDENTITY_FALLBACK};
use oxa_proto::{codes, Engine, OxaError};
use oxa_resources::{identity_path, install_builtin_skills, read_identity, write_atomic};
use std::io::Write;
use std::path::Path;

const USAGE: &str = "usage: oxa identity [--refresh] [--offline]";

/// `oxa identity`: print the installed identity (or say that the fallback is in use).
/// `oxa identity --refresh`: probe this machine and the engine endpoint, write the identity
/// file (only when its bytes change) and the built-in skills. `--offline` skips the endpoint
/// (the model list then says it is not known). Returns the exit code.
pub fn run_identity(words: &[String], dirs: &Dirs, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let mut refresh = false;
    let mut offline = false;
    for w in words {
        match w.as_str() {
            "--refresh" => refresh = true,
            "--offline" => offline = true,
            other => {
                return Err(
                    OxaError::new(codes::USAGE, format!("unknown identity option: {other}")).with_hint(USAGE),
                )
            }
        }
    }
    let write_err = |e: std::io::Error| OxaError::io("cannot write the identity", &e);
    if !refresh {
        match read_identity(dirs) {
            Some(text) => writeln!(out, "{text}").map_err(write_err)?,
            None => writeln!(
                out,
                "No identity installed at {} (the generic text is used):\n{IDENTITY_FALLBACK}",
                identity_path(dirs).display()
            )
            .map_err(write_err)?,
        }
        return Ok(0);
    }
    let engine = if offline { None } else { endpoint_engine(dirs, env) };
    let facts = identity_facts(Path::new("/"), engine.as_deref());
    let changed = write_identity(dirs, &facts)?;
    install_builtin_skills(dirs)?;
    writeln!(
        out,
        "{} {}",
        if changed { "wrote" } else { "unchanged:" },
        identity_path(dirs).display()
    )
    .map_err(write_err)?;
    Ok(0)
}

/// Write the identity text of `facts`; `false` when the file already holds these bytes (so a
/// refresh that finds nothing new keeps every session's prefix, W3).
pub fn write_identity(dirs: &Dirs, facts: &IdentityFacts) -> Result<bool, OxaError> {
    let text = identity_text(facts) + "\n";
    let path = identity_path(dirs);
    if std::fs::read(&path).is_ok_and(|b| b == text.as_bytes()) {
        return Ok(false);
    }
    write_atomic(&path, text.as_bytes())
        .map_err(|e| OxaError::io(&format!("cannot write {}", path.display()), &e))?;
    Ok(true)
}

/// The engine of the default endpoint, `None` when none is configured.
fn endpoint_engine(dirs: &Dirs, env: &Env) -> Option<std::sync::Arc<dyn Engine>> {
    let (models, _) = ModelsConfig::load(dirs, env);
    let ep = crate::dispatch::default_endpoint(&models).ok()?;
    crate::dispatch::engine_for(&ep).ok()
}

/// What this machine and the engine say about themselves. `root` is the file system root the
/// machine files are read under (`/`; a test tree in tests). No date, host name, user name or
/// path is collected. An unreachable engine leaves `models` empty.
pub fn identity_facts(root: &Path, engine: Option<&dyn Engine>) -> IdentityFacts {
    let read = |rel: &str| std::fs::read_to_string(root.join(rel)).ok();
    let clean = |s: String| s.trim_matches(char::from(0)).trim().to_string();
    let machine = read("sys/firmware/devicetree/base/model")
        .or_else(|| read("sys/devices/virtual/dmi/id/product_name"))
        .map(clean)
        .filter(|s| !s.is_empty());
    let chip = read("sys/firmware/devicetree/base/compatible")
        .and_then(|c| chip_from_compatible(&c))
        .or_else(|| read("proc/cpuinfo").and_then(|c| cpu_model_name(&c)));
    let memory_gib = read("proc/meminfo").and_then(|m| mem_total_gib(&m));
    let os_release = read("etc/os-release").or_else(|| read("usr/lib/os-release"));
    let os = os_release
        .as_deref()
        .and_then(|t| os_release_value(t, "PRETTY_NAME"));
    let omarchy = os_release
        .as_deref()
        .and_then(|t| os_release_value(t, "ID"))
        .as_deref()
        == Some("omarchy")
        || root.join("usr/share/omarchy").is_dir();
    IdentityFacts {
        oxa_version: oxa_proto::BUILD_VERSION.to_string(),
        machine,
        chip,
        memory_gib,
        os,
        omarchy,
        models: engine.map(engine_models).unwrap_or_default(),
    }
}

/// Every model the endpoint serves with the limits it publishes for it, sorted by alias.
fn engine_models(engine: &dyn Engine) -> Vec<IdentityModel> {
    let Ok(info) = engine.info(None) else {
        return Vec::new();
    };
    let mut aliases = info.models.clone();
    if !info.model.is_empty() && !aliases.contains(&info.model) {
        aliases.push(info.model.clone());
    }
    aliases.sort();
    aliases.dedup();
    aliases
        .into_iter()
        .map(|alias| {
            let (context, max_output, remote, remote_provider) = match engine.info(Some(&alias)) {
                Ok(i) if i.limits_for == alias => (
                    i.n_ctx,
                    i.max_output,
                    i.caps.remote,
                    i.caps.remote_provider.map(str::to_string),
                ),
                _ => (0, 0, false, None),
            };
            IdentityModel {
                alias,
                context,
                max_output,
                remote,
                remote_provider,
            }
        })
        .collect()
}

/// `qcom,x1e80100` -> `Snapdragon X Elite (X1E80100)`; `qcom,x1p..` -> X Plus.
fn chip_from_compatible(compatible: &str) -> Option<String> {
    compatible
        .split(['\0', '\n'])
        .filter_map(|c| c.trim().strip_prefix("qcom,"))
        .find_map(|soc| {
            let family = if soc.starts_with("x1e") {
                "Snapdragon X Elite"
            } else if soc.starts_with("x1p") {
                "Snapdragon X Plus"
            } else {
                return None;
            };
            Some(format!("{family} ({})", soc.to_uppercase()))
        })
}

fn cpu_model_name(cpuinfo: &str) -> Option<String> {
    cpuinfo
        .lines()
        .find_map(|l| l.strip_prefix("model name"))
        .and_then(|rest| rest.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn mem_total_gib(meminfo: &str) -> Option<u32> {
    let kib: u64 = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let gib = (kib + 512 * 1024) / (1024 * 1024);
    u32::try_from(gib).ok().filter(|g| *g > 0)
}

fn os_release_value(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip_and_os_parsing() {
        assert_eq!(
            chip_from_compatible("lenovo,yoga-slim7x\0qcom,x1e80100\0").as_deref(),
            Some("Snapdragon X Elite (X1E80100)")
        );
        assert_eq!(chip_from_compatible("qcom,sm8550\0"), None);
        assert_eq!(mem_total_gib("MemTotal:       32212254 kB\n"), Some(31));
        assert_eq!(
            os_release_value("NAME=\"Omarchy\"\nID=omarchy\n", "ID").as_deref(),
            Some("omarchy")
        );
        assert_eq!(
            cpu_model_name("model name\t: Some CPU\n").as_deref(),
            Some("Some CPU")
        );
    }
}
