//! Numbers-only health digests, sharing the reference ledger and stored-day formats.
use crate::{paths, request_ledger::number};
use chrono::{Datelike, Days, Local, NaiveDate, TimeZone};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
    time::Duration,
};

const SCHEMA: &str = "local-ai-digest/1";
const STATUS_SCHEMA: &str = "local-ai-digest-status/1";
const BANDS: [&str; 5] = ["0-8k", "8-32k", "32-64k", "64-96k", "96k+"];
const ERRORS: [&str; 10] = [
    "backend_died",
    "client_disconnected",
    "start_failed",
    "model_busy",
    "busy",
    "unavailable",
    "bad_request",
    "local_controller_error",
    "preempted",
    "other",
];
const MISSES: [&str; 9] = [
    "append-ok",
    "retry",
    "history-edit",
    "reasoning-drop",
    "tool-rerender",
    "history-rewrite",
    "compaction",
    "template-drift",
    "unknown",
];
const REWRITES: [&str; 7] = [
    "identical",
    "ws-trailing",
    "ws-inner",
    "args-reserialised",
    "reasoning-dropped",
    "id-changed",
    "other",
];
const COMPLETENESS: [&str; 4] = ["complete", "closed-after-done", "incomplete", "errored"];
const REPLAYS: [&str; 3] = ["cut-served", "cut-done", "cut-aborted"];

fn directory() -> PathBuf {
    std::env::var_os("LOCAL_AI_DIGEST_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::xdg("XDG_STATE_HOME", ".local/state").join("local-ai/digest"))
}
fn ledger() -> PathBuf {
    std::env::var_os("LOCAL_AI_LEDGER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home().join(".local/state/local-ai/ledger"))
}
fn engine_log() -> PathBuf {
    std::env::var_os("LOCAL_AI_ENGINE_LOG")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            paths::xdg("XDG_STATE_HOME", ".local/state").join("local-ai/engine-stderr.log")
        })
}
fn stamp(day: NaiveDate) -> String {
    day.format("%Y%m%d").to_string()
}
fn lines(path: &Path) -> Vec<Vec<u8>> {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(path)
    else {
        return Vec::new();
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Vec::new();
    }
    let mut data = Vec::new();
    if file.take(256 * 1024 * 1024).read_to_end(&mut data).is_err() {
        return Vec::new();
    }
    data.split(|b| *b == b'\r' || *b == b'\n')
        .filter(|s| !s.is_empty() && s.len() <= 1024 * 1024)
        .map(<[u8]>::to_vec)
        .collect()
}
fn object(data: &[u8]) -> Option<Value> {
    serde_json::from_slice::<Value>(data.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(data))
        .ok()
        .filter(Value::is_object)
}
fn ledger_object(data: &[u8]) -> Option<Value> {
    if let Some(value) = object(data) {
        return Some(value);
    }
    let data = data.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(data);
    if data.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return None;
    }
    // Python accepts these JSON atoms. Keep the record while its numeric
    // reducers ignore them; a non-null enum input still becomes "other".
    let mut out = Vec::with_capacity(data.len());
    let (mut index, mut quoted, mut escaped) = (0, false, false);
    while index < data.len() {
        let byte = data[index];
        if quoted {
            out.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            quoted = true;
            out.push(byte);
            index += 1;
            continue;
        }
        let atom = [b"-Infinity".as_slice(), b"Infinity", b"NaN"]
            .into_iter()
            .find(|s| data[index..].starts_with(s));
        if let Some(atom) = atom.filter(|s| {
            data[index + s.len()..]
                .iter()
                .find(|b| !b.is_ascii_whitespace())
                != Some(&b':')
        }) {
            // The reducer sees a nonnumeric, non-null, non-string value.
            out.extend_from_slice(b"{}");
            index += atom.len();
        } else {
            out.push(byte);
            index += 1;
        }
    }
    object(&out)
}
fn stored(day: NaiveDate) -> Option<Value> {
    object(&lines(&directory().join(format!("digest-{}.json", stamp(day)))).join(&b'\n'))
        .filter(|v| v["schema"] == SCHEMA)
}
fn rows(day: NaiveDate) -> Vec<Value> {
    lines(&ledger().join(format!("requests-{}.jsonl", stamp(day))))
        .iter()
        .filter_map(|s| ledger_object(s))
        .collect()
}
fn events(day: NaiveDate) -> (Vec<Value>, u64) {
    let start = Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight"))
        .earliest()
        .map(|t| t.timestamp() as f64)
        .unwrap_or(0.0);
    let mut events = Vec::new();
    let mut undated = 0;
    for line in lines(&engine_log())
        .into_iter()
        .filter(|l| l.first() == Some(&b'{'))
    {
        if let Some(event) = ledger_object(&line).filter(|v| v["event"] == "engine_died") {
            if let Some(t) = number(&event["t"]) {
                if start <= t && t < start + 86400.0 {
                    events.push(event);
                }
            } else {
                undated += 1;
            }
        }
    }
    (events, undated)
}
fn round(v: f64, n: usize) -> f64 {
    format!("{v:.n$}").parse().unwrap_or(v)
}
fn rounded(value: &Value) -> Value {
    if value.is_i64() || value.is_u64() {
        value.clone()
    } else {
        number(value)
            .map(|n| json!(round(n, 2)))
            .unwrap_or(Value::Null)
    }
}
fn percentile(values: &[Value], q: f64) -> Value {
    let mut values: Vec<_> = values.iter().filter(|v| number(v).is_some()).collect();
    values.sort_by(|a, b| {
        number(a)
            .partial_cmp(&number(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if values.is_empty() {
        Value::Null
    } else {
        rounded(values[((q / 100.0 * values.len() as f64).ceil() as usize).max(1) - 1])
    }
}
fn median(values: &[Value]) -> Value {
    let mut values: Vec<_> = values.iter().filter(|v| number(v).is_some()).collect();
    values.sort_by(|a, b| {
        number(a)
            .partial_cmp(&number(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let n = values.len();
    if n == 0 {
        Value::Null
    } else if n % 2 == 1 {
        rounded(values[n / 2])
    } else {
        json!(round(
            (number(values[n / 2 - 1]).unwrap_or(0.0) + number(values[n / 2]).unwrap_or(0.0)) / 2.0,
            2
        ))
    }
}
fn summary(v: &[Value]) -> Value {
    json!({"p50":percentile(v,50.0),"p95":percentile(v,95.0),"n":v.len()})
}
fn rate(part: f64, whole: f64) -> Value {
    if whole != 0.0 {
        json!(round(part / whole, 4))
    } else {
        Value::Null
    }
}
fn label<'a>(v: &'a Value, allowed: &[&str]) -> Option<&'a str> {
    if v.is_null() {
        None
    } else {
        Some(
            v.as_str()
                .filter(|s| allowed.contains(s))
                .unwrap_or("other"),
        )
    }
}
fn inc(map: &mut Map<String, Value>, name: &str) {
    let n = map.get(name).and_then(Value::as_u64).unwrap_or(0);
    map.insert(name.into(), json!(n + 1));
}
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(_) => number(v).is_some_and(|v| v != 0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}
fn alias(v: &Value) -> &str {
    if v.is_null() {
        "unknown"
    } else {
        v.as_str()
            .filter(|v| {
                !v.is_empty()
                    && v.len() <= 64
                    && v.bytes().enumerate().all(|(i, c)| {
                        c.is_ascii_lowercase()
                            || c.is_ascii_digit()
                            || (i > 0 && b"._-".contains(&c))
                    })
            })
            .unwrap_or("other")
    }
}
fn band(ctx: f64) -> Option<usize> {
    if ctx < 0.0 {
        None
    } else {
        Some(
            [8192.0, 32768.0, 65536.0, 98304.0]
                .iter()
                .position(|v| ctx < *v)
                .unwrap_or(4),
        )
    }
}
fn toks(n: &Value, ms: &Value) -> Option<Value> {
    let n = number(n)?;
    let ms = number(ms)?;
    (ms > 0.0 && n >= 8.0).then(|| json!(n * 1000.0 / ms))
}
#[derive(Default)]
struct Series {
    requests: u64,
    ttft: Vec<Value>,
    compaction: Vec<Value>,
    prefill: [Vec<Value>; 5],
    decode: [Vec<Value>; 5],
}
impl Series {
    fn value(&self) -> Value {
        let metric = |v: &[Vec<Value>; 5]| {
            Value::Object(
                BANDS
                    .iter()
                    .enumerate()
                    .map(|(i, b)| {
                        (
                            b.to_string(),
                            json!({"p50":percentile(&v[i],50.0),"n":v[i].len()}),
                        )
                    })
                    .collect(),
            )
        };
        json!({"requests":self.requests,"ttft_hit_ms":summary(&self.ttft),"append_prefill_tok_s":metric(&self.prefill),"decode_tok_s":metric(&self.decode),"compaction_wall_ms_median":median(&self.compaction)})
    }
}
fn store(value: Option<&Value>) -> Value {
    let Some(v) = value.filter(|v| v.is_object()) else {
        return Value::Null;
    };
    let used = number(&v["used_bytes"]);
    let budget = number(&v["budget_bytes"]);
    let mut out = json!({"used_bytes":used.map(|_|v["used_bytes"].clone()),"budget_bytes":budget.map(|_|v["budget_bytes"].clone()),"used_fraction":used.zip(budget).map(|(u,b)|rate(u,b)).unwrap_or(Value::Null)});
    if number(&v["young_evictions"]).is_some() {
        out["young_evictions"] = v["young_evictions"].clone();
    }
    out
}
fn compute(
    day: NaiveDate,
    mut rows: Vec<Value>,
    events: Vec<Value>,
    undated: u64,
    cache: Option<&Value>,
    versions: Option<&Value>,
) -> Value {
    let mut origin = Map::new();
    rows.retain(|v| {
        let label = v["origin"].as_str().unwrap_or("organic");
        if ["lab", "gate"].contains(&label) {
            inc(&mut origin, "excluded");
            false
        } else {
            inc(
                &mut origin,
                if ["organic", "remote"].contains(&label) {
                    label
                } else {
                    "other"
                },
            );
            true
        }
    });
    rows.sort_by(|a, b| {
        number(&a["t"])
            .unwrap_or(0.0)
            .total_cmp(&number(&b["t"]).unwrap_or(0.0))
    });
    let mut status = json!({"2xx":0,"4xx":0,"5xx":0,"other":0});
    let (mut errors, mut completeness, mut miss, mut rewrites_map, mut replays) =
        (Map::new(), Map::new(), Map::new(), Map::new(), Map::new());
    let (mut retries, mut duplicates, mut eligible, mut prefix_hits, mut c3_miss, mut unknown) =
        (0_u64, 0_u64, 0_u64, 0_u64, 0_u64, 0_u64);
    let (mut cached_sum, mut ctx_sum) = (0.0, 0.0);
    let (
        mut rewrites,
        mut folds,
        mut compactions,
        mut queued,
        mut switches,
        mut stream_cut,
        mut backend_died,
    ) = (0_u64, 0_u64, 0_u64, 0_u64, 0_u64, 0_u64, 0_u64);
    let (mut ttft, mut ttft_hit, mut restore, mut compaction, mut queue_wait) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut series: BTreeMap<String, Series> = BTreeMap::new();
    for row in &rows {
        let model = alias(&row["model"]);
        let key = if row["mode"]
            .as_str()
            .is_some_and(|m| ["fast", "deep", "max"].contains(&m))
        {
            format!("{model}/{}", row["mode"].as_str().unwrap_or(""))
        } else {
            model.into()
        };
        let ser = series.entry(key).or_default();
        ser.requests += 1;
        let code = number(&row["status"]);
        let bucket = match code {
            Some(v) if (200.0..300.0).contains(&v) => "2xx",
            Some(v) if (400.0..500.0).contains(&v) => "4xx",
            Some(v) if (500.0..600.0).contains(&v) => "5xx",
            _ => "other",
        };
        inc(status.as_object_mut().expect("status counts"), bucket);
        let error = label(&row["error"], &ERRORS);
        if let Some(error) = error {
            inc(&mut errors, error);
            backend_died += u64::from(error == "backend_died");
        }
        if let Some(done) = label(&row["completeness"], &COMPLETENESS) {
            inc(&mut completeness, done);
            stream_cut += u64::from(done == "incomplete" && error != Some("client_disconnected"));
        }
        let class = label(&row["miss_class"], &MISSES);
        if let Some(class) = class {
            inc(&mut miss, class);
            retries += u64::from(class == "retry");
            unknown += u64::from(class == "unknown");
        }
        duplicates += u64::from(row["history_change"] == "same");
        let cached = number(row.get("cached_tokens").unwrap_or(&row["cache_n"]));
        let ctx = number(&row["ctx_tokens"])
            .or_else(|| cached.zip(number(&row["prompt_n"])).map(|(c, p)| c + p));
        if let (Some(c), Some(ctx)) = (cached, ctx.filter(|v| *v != 0.0)) {
            cached_sum += c;
            ctx_sum += ctx;
        }
        if row["history_change"] == "append" && row["previous_reply"] == "present" {
            eligible += 1;
            prefix_hits += u64::from(
                (class.is_none() || class == Some("append-ok")) && cached.unwrap_or(0.0) > 0.0,
            );
            c3_miss += u64::from(class.is_some() && class != Some("append-ok"));
        }
        rewrites += u64::from(
            row["history_rewrite"] == true
                || [json!("edited"), json!("truncated")].contains(&row["history_change"]),
        );
        folds += u64::from(row["fold_suspect"] == true);
        if let Some(v) = label(&row["rewrite_class"], &REWRITES) {
            inc(&mut rewrites_map, v);
        }
        if let Some(v) = label(&row["replay"], &REPLAYS) {
            inc(&mut replays, v);
        }
        if row["turn_kind"] == "compaction" {
            compactions += 1;
            if number(&row["wall_ms"]).is_some() {
                compaction.push(row["wall_ms"].clone());
                ser.compaction.push(row["wall_ms"].clone());
            }
        }
        if number(&row["queue_wait_ms"]).is_some() {
            queue_wait.push(row["queue_wait_ms"].clone());
        }
        queued += u64::from(number(&row["queue_pos"]).is_some());
        switches += u64::from(row["session_switch"] == true);
        let answered = code.is_some_and(|v| (200.0..300.0).contains(&v));
        if answered && number(&row["ttfb_ms"]).is_some() {
            ttft.push(row["ttfb_ms"].clone());
            if cached.unwrap_or(0.0) > 0.0 && (class.is_none() || class == Some("append-ok")) {
                ttft_hit.push(row["ttfb_ms"].clone());
                ser.ttft.push(row["ttfb_ms"].clone());
            }
        }
        if number(&row["resume_ms"]).is_some()
            && row["resume_src"]
                .as_str()
                .is_some_and(|s| ["ram", "disk"].contains(&s))
        {
            restore.push(row["resume_ms"].clone());
        }
        if let Some(band) = ctx.filter(|v| answered && *v != 0.0).and_then(band) {
            if cached.unwrap_or(0.0) > 0.0 {
                if let Some(v) = toks(&row["prompt_n"], &row["prompt_ms"]) {
                    ser.prefill[band].push(v);
                }
            }
            if let Some(v) = toks(&row["predicted_n"], &row["predicted_ms"]) {
                ser.decode[band].push(v);
            }
        }
    }
    let mut groups = json!({"oom":0,"psi":0,"memory_floor":0,"swap":0,"pids":0,"other":0});
    for event in &events {
        if let Some(reason) = event["guard_failure"].as_str().filter(|s| !s.is_empty()) {
            let group = match reason {
                "cgroup_oom" | "host_oom_kill" => "oom",
                "host_memory_pressure_sustained" | "host_memory_pressure_full_sustained" => "psi",
                "host_available_below_floor"
                | "host_available_emergency"
                | "host_memory_and_swap_exhausted" => "memory_floor",
                "own_swap_nonzero" | "own_swap_limit_not_zero" => "swap",
                "cgroup_pids_limit" => "pids",
                _ => "other",
            };
            inc(groups.as_object_mut().expect("guard groups"), group);
        }
    }
    let trips: u64 = groups
        .as_object()
        .expect("guard groups")
        .values()
        .filter_map(Value::as_u64)
        .sum();
    json!({"schema":SCHEMA,"day":day.to_string(),"generated_at":Local::now().timestamp(),"partial":day>=Local::now().date_naive(),"requests":rows.len(),"by_origin":origin,"status":status,"retries":retries,"duplicate_bodies":duplicates,
        "cache":{"token_hit_rate":rate(cached_sum,ctx_sum),"eligible_turns":eligible,"prefix_hits":prefix_hits,"prefix_hit_rate":rate(prefix_hits as f64,eligible as f64),"c3_misses":c3_miss,"unknown_misses":unknown,"unknown_miss_rate":rate(unknown as f64,eligible as f64),"miss_classes":miss},
        "history":{"history_rewrites":rewrites,"fold_suspects":folds,"rewrite_classes":rewrites_map,"client_compactions":compactions,"compaction_wall_ms_median":median(&compaction)},
        "queue":{"queued":queued,"session_switches":switches,"wait_ms":summary(&queue_wait),"replays":replays},
        "guard":{"trips":trips,"by_group":groups,"engine_deaths":events.len(),"undated_engine_events":undated},
        "latency":{"ttft_ms":summary(&ttft),"ttft_hit_ms":summary(&ttft_hit),"restore_ms":summary(&restore)},
        "series":series.iter().map(|(k,v)|(k.clone(),v.value())).collect::<Map<_,_>>(),"errors":{"by_label":errors,"http_5xx":status["5xx"],"stream_cut":stream_cut,"backend_died":backend_died,"completeness":completeness},
        "store":store(cache),"versions":versions,"alerts":[],"trend_basis_days":0})
}

fn add_alert(alerts: &mut Vec<Value>, id: &str, value: Value, threshold: Value) {
    alerts.push(json!({"id":id,"value":value,"threshold":threshold}));
}
fn alerts(digest: &mut Value, trailing: &[Value]) {
    let mut out = Vec::new();
    let d = &*digest;
    let g = &d["guard"];
    let e = &d["errors"];
    let c = &d["cache"];
    let count = |v: &Value| v.as_u64().unwrap_or(0);
    for (id, value) in [
        ("client_retries", count(&d["retries"])),
        ("duplicate_prefill", count(&d["duplicate_bodies"])),
        ("http_5xx", count(&e["http_5xx"])),
        ("stream_cut", count(&e["stream_cut"])),
        (
            "engine_death",
            count(&e["backend_died"])
                .max(count(&g["engine_deaths"]).saturating_sub(count(&g["trips"]))),
        ),
        (
            "guard_trip",
            count(&g["trips"]) - count(&g["by_group"]["oom"]) - count(&g["by_group"]["swap"]),
        ),
        ("scope_oom", count(&g["by_group"]["oom"])),
        ("swap", count(&g["by_group"]["swap"])),
    ] {
        if value > 0 {
            add_alert(&mut out, id, json!(value), json!(0));
        }
    }
    if truthy(&c["eligible_turns"]) {
        if number(&c["unknown_miss_rate"]).is_some_and(|v| v > 0.01) {
            add_alert(
                &mut out,
                "unknown_cache_misses",
                c["unknown_miss_rate"].clone(),
                json!(0.01),
            );
        }
        if number(&c["prefix_hit_rate"]).is_some_and(|v| v < 0.90) {
            add_alert(
                &mut out,
                "prefix_hits_low",
                c["prefix_hit_rate"].clone(),
                json!(0.90),
            );
        }
    }
    if number(&d["latency"]["restore_ms"]["p95"]).is_some_and(|v| v > 5000.0) {
        add_alert(
            &mut out,
            "restore_slow",
            d["latency"]["restore_ms"]["p95"].clone(),
            json!(5000.0),
        );
    }
    if number(&d["store"]["used_fraction"]).is_some_and(|v| v > 0.90) {
        add_alert(
            &mut out,
            "store_full",
            d["store"]["used_fraction"].clone(),
            json!(0.90),
        );
    }
    if number(&d["store"]["young_evictions"]).is_some_and(|v| v > 0.0) {
        add_alert(
            &mut out,
            "store_young_eviction",
            d["store"]["young_evictions"].clone(),
            json!(0),
        );
    }
    let trailing: Vec<_> = trailing
        .iter()
        .filter(|v| v["schema"] == SCHEMA)
        .take(7)
        .collect();
    if let Some(series) = d["series"].as_object() {
        for (key, cur) in series {
            for (metric, id) in [
                ("decode_tok_s", "decode_slower"),
                ("append_prefill_tok_s", "prefill_slower"),
            ] {
                for band in BANDS {
                    let today = &cur[metric][band];
                    let Some(value) =
                        number(&today["p50"]).filter(|_| number(&today["n"]).unwrap_or(0.0) >= 5.0)
                    else {
                        continue;
                    };
                    let past: Vec<_> = trailing
                        .iter()
                        .map(|d| &d["series"][key][metric][band])
                        .filter(|p| {
                            number(&p["p50"]).is_some() && number(&p["n"]).unwrap_or(0.0) >= 5.0
                        })
                        .map(|p| p["p50"].clone())
                        .collect();
                    if past.len() >= 3 {
                        if let Some(base) = number(&median(&past)).filter(|v| *v != 0.0) {
                            if value < (1.0 - 0.10) * base {
                                add_alert(
                                    &mut out,
                                    &format!("{id}:{key}:{band}"),
                                    today["p50"].clone(),
                                    json!(round((1.0 - 0.10) * base, 2)),
                                );
                            }
                        }
                    }
                }
            }
            for (value, metric, id) in [
                (&cur["ttft_hit_ms"]["p95"], "ttft_hit_ms", "ttft_hit_slower"),
                (
                    &cur["compaction_wall_ms_median"],
                    "compaction_wall_ms_median",
                    "compaction_slower",
                ),
            ] {
                let past: Vec<_> = trailing
                    .iter()
                    .map(|d| {
                        if metric == "ttft_hit_ms" {
                            &d["series"][key][metric]["p95"]
                        } else {
                            &d["series"][key][metric]
                        }
                    })
                    .filter(|v| number(v).is_some())
                    .cloned()
                    .collect();
                if past.len() >= 3 {
                    if let (Some(numeric_value), Some(base)) =
                        (number(value), number(&median(&past)).filter(|v| *v != 0.0))
                    {
                        if numeric_value > 1.5 * base {
                            add_alert(
                                &mut out,
                                &format!("{id}:{key}"),
                                value.clone(),
                                json!(round(1.5 * base, 2)),
                            );
                        }
                    }
                }
            }
        }
    }
    digest["trend_basis_days"] = json!(trailing.len());
    digest["green"] = json!(out.is_empty());
    digest["idle"] = json!(digest["requests"] == 0);
    digest["alerts"] = json!(out);
}
struct Digester {
    today: NaiveDate,
    store: Option<Value>,
    versions: Value,
    cache: HashMap<NaiveDate, Value>,
}
impl Digester {
    fn day(&mut self, day: NaiveDate) -> Value {
        if let Some(v) = self.cache.get(&day) {
            return v.clone();
        }
        let value = if let Some(v) = if day < self.today { stored(day) } else { None } {
            v
        } else {
            let (events, undated) = events(day);
            let recent = day >= self.today - Days::new(1);
            let mut value = compute(
                day,
                rows(day),
                events,
                undated,
                self.store.as_ref().filter(|_| recent),
                recent.then_some(&self.versions),
            );
            value["partial"] = json!(day >= self.today);
            alerts(&mut value, &self.trailing(day));
            value
        };
        self.cache.insert(day, value.clone());
        value
    }
    fn trailing(&self, day: NaiveDate) -> Vec<Value> {
        (1..=7)
            .filter_map(|back| {
                let day = day - Days::new(back);
                let value = if let Some(value) = stored(day) {
                    value
                } else {
                    let rows = rows(day);
                    if rows.is_empty() {
                        return None;
                    }
                    let (events, undated) = events(day);
                    compute(day, rows, events, undated, None, None)
                };
                truthy(&value["requests"]).then_some(value)
            })
            .collect()
    }
    fn soak(&mut self, last: NaiveDate) -> Value {
        let mut window = Vec::new();
        for back in 0..30 {
            let value = self.day(last - Days::new(back));
            if !truthy(&value["requests"]) {
                continue;
            }
            if truthy(&value["alerts"]) {
                break;
            }
            window.push(value);
            if window.len() >= 7 {
                break;
            }
        }
        let requests: u64 = window.iter().filter_map(|v| v["requests"].as_u64()).sum();
        let rewrites: u64 = window
            .iter()
            .filter_map(|v| v["history"]["history_rewrites"].as_u64())
            .sum();
        json!({"green_days":window.len(),"days":window.iter().map(|v|v["day"].clone()).collect::<Vec<_>>(),"requests":requests,"history_rewrites":rewrites,"ship_ready":window.len()>=3,"pass":window.len()>=7&&requests>=300&&rewrites>=10,"needs":{"green_days":7,"requests":300,"history_rewrites":10,"ship_green_days":3}})
    }
}
fn alert_text(id: &str) -> &str {
    match id.split(':').next().unwrap_or("") {
        "client_retries" => "client retries",
        "duplicate_prefill" => "repeated requests",
        "http_5xx" => "server errors",
        "stream_cut" => "answers cut off",
        "engine_death" => "engine stopped unexpectedly",
        "guard_trip" => "memory guard stopped a model",
        "scope_oom" => "out-of-memory stop",
        "swap" => "model memory swapped",
        "unknown_cache_misses" => "unexplained cache misses",
        "prefix_hits_low" => "low cache reuse",
        "restore_slow" => "slow session restore",
        "store_full" => "cache storage nearly full",
        "store_young_eviction" => "recent conversation evicted",
        "decode_slower" => "slower replies",
        "prefill_slower" => "slower prompt reading",
        "ttft_hit_slower" => "slower first token",
        "compaction_slower" => "slower compaction",
        _ => "check the digest",
    }
}
fn status_line(d: &Value, soak: &Value) -> Value {
    let empty = Vec::new();
    let alerts = d["alerts"].as_array().unwrap_or(&empty);
    // write_day passes the digest's own generation time, including for a
    // previously stored day. read_status applies wall-clock staleness later.
    let now = number(&d["generated_at"]).unwrap_or_else(|| Local::now().timestamp() as f64);
    let (state, line) = if now - number(&d["generated_at"]).unwrap_or(0.0) > 36.0 * 3600.0 {
        ("stale", "Health check has not run lately".into())
    } else if !truthy(&d["requests"]) {
        ("no_data", "No use recorded yet".into())
    } else if alerts.is_empty() {
        ("healthy", "Healthy".into())
    } else {
        (
            "attention",
            if alerts.len() == 1 {
                "1 thing to look at".into()
            } else {
                format!("{} things to look at", alerts.len())
            },
        )
    };
    let mut value = json!({"schema":STATUS_SCHEMA,"day":d["day"],"state":state,"line":line,"alerts":alerts.iter().map(|a|a["id"].clone()).collect::<Vec<_>>(),"alert_text":alerts.iter().map(|a|alert_text(a["id"].as_str().unwrap_or(""))).collect::<Vec<_>>(),"requests":d.get("requests").cloned().unwrap_or(json!(0)),"generated_at":d["generated_at"]});
    value["soak"] = Value::Object(
        [
            "green_days",
            "ship_ready",
            "pass",
            "requests",
            "history_rewrites",
        ]
        .into_iter()
        .map(|k| (k.into(), soak[k].clone()))
        .collect(),
    );
    value
}
fn read_status() -> Value {
    let value = object(&lines(&directory().join("status.json")).join(&b'\n'))
        .filter(|v| v["schema"] == STATUS_SCHEMA);
    if let Some(mut value) = value {
        if Local::now().timestamp() as f64 - number(&value["generated_at"]).unwrap_or(0.0)
            > 36.0 * 3600.0
        {
            value["state"] = json!("stale");
            value["line"] = json!("Health check has not run lately");
        }
        value
    } else {
        json!({"schema":STATUS_SCHEMA,"day":null,"state":"no_data","line":"No health check yet","alerts":[],"alert_text":[],"requests":0,"generated_at":null})
    }
}
fn pretty(value: &Value, depth: usize) -> String {
    let indent = " ".repeat(depth);
    let next = " ".repeat(depth + 1);
    match value {
        Value::Object(v) if !v.is_empty() => {
            let mut keys: Vec<_> = v.keys().collect();
            keys.sort();
            format!(
                "{{\n{}\n{indent}}}",
                keys.into_iter()
                    .map(|k| format!(
                        "{next}{}: {}",
                        paths::python_json(&json!(k)),
                        pretty(&v[k], depth + 1)
                    ))
                    .collect::<Vec<_>>()
                    .join(",\n")
            )
        }
        Value::Array(v) if !v.is_empty() => format!(
            "[\n{}\n{indent}]",
            v.iter()
                .map(|v| format!("{next}{}", pretty(v, depth + 1)))
                .collect::<Vec<_>>()
                .join(",\n")
        ),
        _ => paths::python_json(value),
    }
}
fn write_private(path: &Path, value: &Value) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce)
        .map_err(|_| io::Error::other("Digest temporary-file entropy is unavailable."))?;
    let nonce = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let temporary = parent.join(format!(
        ".{}.{nonce}",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        writeln!(file, "{}", pretty(value, 0))?;
        file.sync_all()?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
fn fmt(v: &Value, unit: &str) -> String {
    number(v)
        .map(|v| format!("{v:.0}{unit}"))
        .unwrap_or_else(|| "-".into())
}
fn text(d: &Value) -> String {
    let empty = Vec::new();
    let alerts = d["alerts"].as_array().unwrap_or(&empty);
    let cache = &d["cache"];
    let history = &d["history"];
    let queue = &d["queue"];
    let lat = &d["latency"];
    let health = if alerts.is_empty() {
        "healthy".into()
    } else {
        format!(
            "{} to look at: {}",
            alerts.len(),
            alerts
                .iter()
                .map(|a| {
                    let id = a["id"].as_str().unwrap_or("");
                    let text = alert_text(id);
                    if text == "check the digest" {
                        id
                    } else {
                        text
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let hit = number(&cache["token_hit_rate"])
        .map(|v| json!(v * 100.0))
        .unwrap_or(Value::Null);
    let replays: u64 = queue["replays"]
        .as_object()
        .into_iter()
        .flat_map(|v| v.values())
        .filter_map(Value::as_u64)
        .sum();
    let mut lines = vec![
        format!(
            "Day {}{}: {} requests, {health}",
            d["day"].as_str().unwrap_or(""),
            if d["partial"] == true {
                " (so far)"
            } else {
                ""
            },
            paths::python_json(&d["requests"])
        ),
        format!(
            "  cache reuse {} of tokens, {} misses in {} follow-up turns ({} unexplained)",
            fmt(&hit, "%"),
            cache["c3_misses"],
            cache["eligible_turns"],
            cache["unknown_misses"]
        ),
        format!(
            "  history rewrites {} (fold suspects {}, client compactions {})",
            history["history_rewrites"], history["fold_suspects"], history["client_compactions"]
        ),
        format!(
            "  queued {} (wait p95 {}), replays {replays}",
            queue["queued"],
            fmt(&queue["wait_ms"]["p95"], " ms")
        ),
        format!(
            "  first token p50 {}, p95 {}; errors 5xx {}, cut {}, guard stops {}",
            fmt(&lat["ttft_ms"]["p50"], " ms"),
            fmt(&lat["ttft_ms"]["p95"], " ms"),
            d["errors"]["http_5xx"],
            d["errors"]["stream_cut"],
            d["guard"]["trips"]
        ),
    ];
    if let Some(versions) = d["versions"].as_object().filter(|v| !v.is_empty()) {
        let mut keys: Vec<_> = versions.keys().collect();
        keys.sort();
        lines.push(format!(
            "  installed: {}",
            keys.into_iter()
                .map(|k| format!("{k} {}", versions[k].as_str().unwrap_or("")))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(series) = d["series"].as_object() {
        for (key, cur) in series {
            let decode: Vec<_> = BANDS
                .iter()
                .filter_map(|b| {
                    number(&cur["decode_tok_s"][b]["p50"]).map(|v| format!("{b} {v:.1}"))
                })
                .collect();
            if !decode.is_empty() {
                lines.push(format!(
                    "  {key} reply speed tok/s by depth: {}",
                    decode.join(", ")
                ));
            }
        }
    }
    lines.join("\n")
}
fn parse_day(text: Option<&str>, yesterday: bool) -> Option<NaiveDate> {
    let today = Local::now().date_naive();
    let text =
        text.filter(|v| !v.is_empty())
            .unwrap_or(if yesterday { "yesterday" } else { "today" });
    if text == "today" {
        return Some(today);
    }
    if text == "yesterday" {
        return Some(today - Days::new(1));
    }
    static DAY: OnceLock<Regex> = OnceLock::new();
    if !DAY.get_or_init(||Regex::new(r"\A(?:[0-9]{4}-[0-9]{2}-[0-9]{2}|[0-9]{8}|[0-9]{4}-W[0-9]{2}(?:-[1-7])?|[0-9]{4}W[0-9]{2}[1-7]?)\z").expect("ISO day grammar")).is_match(text) {return None;}
    let completed;
    let text = if text.len() == 8 && text.as_bytes().get(5) == Some(&b'W') {
        completed = format!("{text}-1");
        completed.as_str()
    } else if text.len() == 7 && text.as_bytes().get(4) == Some(&b'W') {
        completed = format!("{text}1");
        completed.as_str()
    } else {
        text
    };
    ["%Y-%m-%d", "%Y%m%d", "%G-W%V-%u", "%GW%V%u"]
        .into_iter()
        .find_map(|format| NaiveDate::parse_from_str(text, format).ok())
        .filter(|d| (1..=9999).contains(&d.year()))
}
async fn versions() -> Value {
    let path = paths::home().join(".local/bin/oxa");
    if !path.is_file() {
        return json!({"oxa":"absent"});
    }
    let mut command = tokio::process::Command::new(path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let version = match tokio::time::timeout(Duration::from_secs(5), command.output()).await {
        Ok(Ok(output)) if output.status.success() => String::from_utf8(output.stdout)
            .ok()
            .zip(String::from_utf8(output.stderr).ok())
            .and_then(|(out, err)| {
                static VERSION: OnceLock<Regex> = OnceLock::new();
                let regex = VERSION.get_or_init(|| {
                    Regex::new(r"\b(\d{1,4}\.\d{1,4}\.\d{1,6}(?:-[0-9A-Za-z.]{1,32})?)\b")
                        .expect("version grammar")
                });
                regex
                    .captures(&format!("{out} {err}"))
                    .and_then(|v| v.get(1))
                    .map(|v| v.as_str().to_owned())
                    .filter(|v| {
                        regex
                            .find(v)
                            .is_some_and(|m| m.start() == 0 && m.end() == v.len())
                    })
            }),
        _ => None,
    };
    json!({"oxa":version.unwrap_or_else(||"unknown".into())})
}
pub async fn run(day: Option<String>, as_json: bool, write: bool, status: bool, soak: bool) -> i32 {
    if status {
        let value = read_status();
        if as_json {
            println!("{}", paths::python_json(&value));
        } else {
            let suffix = value["alert_text"]
                .as_array()
                .filter(|v| !v.is_empty())
                .map(|v| {
                    format!(
                        ": {}",
                        v.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
                .unwrap_or_default();
            println!("{}{suffix}", value["line"].as_str().unwrap_or(""));
        }
        return 0;
    }
    let Some(day) = parse_day(day.as_deref(), write || soak) else {
        eprintln!("local-ai: Use a day like 2026-11-13, today or yesterday.");
        return 2;
    };
    let store = crate::cli::cache_status().await;
    let versions = versions().await;
    tokio::task::spawn_blocking(move || {
        let mut digester = Digester {
            today: Local::now().date_naive(),
            store: (store["cache_available"] == true).then_some(store),
            versions,
            cache: HashMap::new(),
        };
        if soak {
            let value = digester.soak(day);
            if as_json {
                println!("{}", paths::python_json(&value));
            } else {
                println!(
                    "Soak: {} of {} green days, {} requests, {} history rewrites{}",
                    value["green_days"],
                    value["needs"]["green_days"],
                    value["requests"],
                    value["history_rewrites"],
                    if value["pass"] == true {
                        " - passed"
                    } else if value["ship_ready"] == true {
                        " - ship bar met"
                    } else {
                        ""
                    }
                );
            }
            return 0;
        }
        let value = digester.day(day);
        if write {
            let saved = (|| {
                write_private(
                    &directory().join(format!("digest-{}.json", stamp(day))),
                    &value,
                )?;
                let soak = digester.soak(day);
                write_private(
                    &directory().join("status.json"),
                    &status_line(&value, &soak),
                )
            })();
            if let Err(e) = saved {
                let name = match e.kind() {
                    io::ErrorKind::NotADirectory => "NotADirectoryError",
                    io::ErrorKind::AlreadyExists => "FileExistsError",
                    io::ErrorKind::PermissionDenied => "PermissionError",
                    io::ErrorKind::NotFound => "FileNotFoundError",
                    _ => "OSError",
                };
                eprintln!("local-ai: the digest could not be saved ({name}).");
                return 1;
            }
        }
        println!(
            "{}",
            if as_json {
                paths::python_json(&value)
            } else {
                text(&value)
            }
        );
        0
    })
    .await
    .unwrap_or(1)
}
