//! Explicit local projection of the existing Claims store; never admits or starts work.
use super::{refill, refill_input::text};
use oxa_work::board::{Claims, Policy};
use serde_json::json;
use std::{collections::BTreeMap, io::Write, path::Path};

pub(super) fn run(
    args: &[String],
    root: &Path,
    common: &Path,
    env: &oxa_config::Env,
    dirs: &oxa_config::Dirs,
) -> Result<i32, String> {
    let [mode, flag, config] = args else {
        return Err("usage: oxa work board --claims-readback --config PATH".into());
    };
    if mode != "--claims-readback" || flag != "--config" {
        return Err("invalid Claims readback options".into());
    }
    let (value, _, _) = refill::configuration(Path::new(config), root, env, dirs)?;
    let path = Path::new(text(&value, "claims")?);
    crate::work_cmd::protected_storage(path, root, common)?;
    let claims = Claims::open(path)?;
    let (worker, host) = crate::work_cmd::registered_native_worker(&value)?;
    let partition = refill::partition(&value)?;
    partition.validate(Policy::default().max_attempts_per_day)?;
    let assignments = claims.assignments()?;
    let membership: BTreeMap<_, _> = assignments
        .keys()
        .map(|number| (*number, partition.contains(*number)))
        .collect();
    let receipt = json!({
        "schema_version":1,"source":"Claims::assignments",
        "worker":worker,"host_id":host,"refill_enabled":value["enabled"] == true,
        "partition":{"modulus":partition.modulus,"residues":partition.residues},
        "partition_source":"protected-owner-configuration","partition_membership":membership,
        "assignments":assignments,"legacy_coverage":"unknown"
    });
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &receipt).map_err(|_| "cannot write Claims readback")?;
    writeln!(out).map_err(|_| "cannot write Claims readback")?;
    Ok(0)
}
