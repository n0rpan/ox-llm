//! Studio model metadata, user settings and hash-bound size-gate consumption.
use super::Studio;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

fn read(path: &Path) -> Value {
    fs::read(path)
        .ok()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or(Value::Null)
}
fn parsed(path: &Path) -> Option<Value> {
    fs::read(path)
        .ok()
        .and_then(|v| serde_json::from_slice(&v).ok())
}
fn truthy(value: &Value) -> bool {
    crate::release_files::truthy(value)
}
fn number(value: &Value) -> Option<f64> {
    value.as_f64()
}
fn bounded(value: &Value, low: f64, high: f64) -> Option<f64> {
    number(value).filter(|v| *v >= low && *v <= high)
}
fn integer(value: &Value) -> Option<i64> {
    value.as_i64()
}
fn rounded(value: f64, places: usize) -> f64 {
    format!("{value:.places$}").parse().unwrap_or(value)
}
fn executable(path: &Path) -> bool {
    path.is_file() && nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
}
fn variable(name: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(name).map(PathBuf::from).unwrap_or(default)
}
impl Studio {
    pub(super) fn cpu_optimized(
        &self,
        model: &str,
        width: i64,
        height: i64,
        steps: i64,
        edit: bool,
    ) -> bool {
        self.cpu_profile(model, width, height, steps, edit)
            .is_ok_and(|v| v.is_some())
    }
    pub(super) fn cpu_profile(
        &self,
        model: &str,
        width: i64,
        height: i64,
        steps: i64,
        edit: bool,
    ) -> Result<Option<Value>, String> {
        if !["q4", "q8"].contains(&model) || steps != 8 {
            return Ok(None);
        }
        let path = self.project.join("cpu-profile.json");
        if !path.is_file() {
            return Ok(None);
        }
        let incomplete = || "CPU optimization profile is incomplete".to_owned();
        let bytes = fs::read(path).map_err(|_| incomplete())?;
        let profile: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                return if model == "q8" {
                    Ok(None)
                } else {
                    Err(incomplete())
                }
            }
        };
        let fields = profile.as_object().ok_or_else(incomplete)?;
        let models = fields
            .get("models")
            .cloned()
            .unwrap_or_else(|| json!(["q4"]));
        let listed = match &models {
            Value::String(s) => s.contains(model),
            Value::Array(a) => a.contains(&json!(model)),
            Value::Object(m) => m.contains_key(model),
            _ => return Err(incomplete()),
        };
        if !listed || !truthy(&profile["enabled"]) {
            return Ok(None);
        }
        if !truthy(&profile["validated"]) {
            return Err("CPU optimization has not passed validation".into());
        }
        let key = if edit {
            if profile["img2img_validated"] != true {
                return Ok(None);
            }
            "img2img_dimensions"
        } else {
            "dimensions"
        };
        let dimensions = fields.get(key).cloned().unwrap_or_else(|| json!([]));
        let matched = dimensions
            .as_array()
            .ok_or_else(incomplete)?
            .iter()
            .any(|v| {
                v.as_array().is_some_and(|p| {
                    p.len() == 2
                        && p[0].as_f64() == Some(width as f64)
                        && p[1].as_f64() == Some(height as f64)
                })
            });
        if !matched {
            return Ok(None);
        }
        let binary = fields
            .get("binary")
            .and_then(Value::as_str)
            .ok_or_else(incomplete)?;
        let binary = fs::canonicalize(binary)
            .map_err(|_| "Installed CPU optimization is unavailable".to_owned())?;
        let releases = fs::canonicalize(self.stack.join("releases"))
            .map_err(|_| "Installed CPU optimization is unavailable".to_owned())?;
        if !binary.starts_with(releases) || !executable(&binary) {
            return Err("Installed CPU optimization is unavailable".into());
        }
        let expected = fields.get("sha256").ok_or_else(incomplete)?;
        let sha = crate::bootstrap::sha(&binary).map_err(|_| incomplete())?;
        if json!(sha) != *expected {
            return Err("Installed CPU binary failed its checksum".into());
        }
        Ok(Some(profile))
    }
    fn zimage_estimates(&self) -> Value {
        let mut estimate = json!({"calibrated":false,"q8":{"fixed_s":12.0,"s_per_mpx_step":59.9,"pixel_exponent":1.5},"q4":{"fixed_s":12.0,"s_per_mpx_step":70.7,"pixel_exponent":1.35},"unoptimized_factor":1.59,"vae_encode_s_per_mpx":10.0,"esrgan_fixed_s":3.0,"esrgan_s_per_output_mpx":20.0});
        if let Some(values) = read(&self.project.join("upscale.json"))["estimate"].as_object() {
            for (key, value) in values {
                if key == "calibrated" && value.is_boolean() {
                    estimate[key] = value.clone();
                } else if ["q4", "q8"].contains(&key.as_str()) && value.is_object() {
                    for name in ["fixed_s", "s_per_mpx_step", "pixel_exponent"] {
                        if let Some(n) = bounded(&value[name], 0.0, 10000.0) {
                            estimate[key][name] = json!(n);
                        }
                    }
                } else if estimate.get(key).is_some_and(|v| v.is_number()) {
                    if let Some(n) = bounded(value, 0.0, 10000.0) {
                        estimate[key] = json!(n);
                    }
                }
            }
        }
        estimate
    }
    pub(super) fn estimate(&self, query: &str) -> Result<Value, ()> {
        let url = reqwest::Url::parse(&format!("http://localhost/?{query}")).map_err(|_| ())?;
        let mut params = std::collections::HashMap::new();
        for (key, value) in url.query_pairs().filter(|(_, v)| !v.is_empty()) {
            params
                .entry(key.into_owned())
                .or_insert_with(|| value.into_owned());
        }
        let get =
            |key: &str, default: &str| params.get(key).cloned().unwrap_or_else(|| default.into());
        let model = get("model", "");
        let klein = ["klein-q8", "klein-q4"].contains(&model.as_str());
        let width = python_int(&get("width", if klein { "512" } else { "256" })).ok_or(())?;
        let height = python_int(&get("height", if klein { "512" } else { "256" })).ok_or(())?;
        let count = python_int(&get("variations", "1")).ok_or(())?;
        if !(1..=4).contains(&count) {
            return Err(());
        }
        if klein {
            if ![width, height]
                .iter()
                .all(|n| (256..=1024).contains(n) && n % 16 == 0)
            {
                return Err(());
            }
            let setting = self.engine_setting();
            let gated = (width, height) != (512, 512);
            let missing = self.oxi_missing();
            let (oxi, reason) = if setting == "sdcpp" {
                (false, "engine setting: sd.cpp".into())
            } else if model != "klein-q8" {
                (
                    false,
                    "oxi runs FLUX.2 klein Q8 only; this model runs on sd.cpp".into(),
                )
            } else if gated && !self.size_gate(width, height) {
                (false,format!("oxi fast path is 512 × 512 only; {width} × {height} runs on sd.cpp (slower)"))
            } else if !missing.is_empty() {
                (
                    false,
                    format!(
                        "oxi is not installed ({}); using sd.cpp",
                        missing.join(", ")
                    ),
                )
            } else if gated {
                (
                    true,
                    format!("oxi ({width} × {height} passed its size gate)"),
                )
            } else {
                (true, "oxi fast path (512 × 512)".into())
            };
            let seconds = if oxi {
                rounded(
                    8.0 + count as f64
                        * 43.0
                        * (width as f64 * height as f64 / (512.0 * 512.0)).powf(0.75),
                    1,
                )
            } else {
                self.edit_seconds(width, height, true, count)
            };
            return Ok(
                json!({"seconds":seconds,"optimized":oxi,"calibrated":if oxi{json!(false)}else{self.edit_settings()["calibrated"].clone()},"engine":if oxi{"oxi"}else{"sd.cpp"},"engine_reason":reason}),
            );
        }
        let model = get("model", "q8");
        let steps = python_int(&get("steps", "8")).ok_or(())?;
        if !["q4", "q8"].contains(&model.as_str())
            || ![width, height].iter().all(|n| (256..=1536).contains(n))
            || !(1..=50).contains(&steps)
        {
            return Err(());
        }
        let edit = get("edit", "0") == "1";
        let strength = if edit {
            Some(python_float(&get("strength", "0.3")).ok_or(())?)
        } else {
            None
        };
        let sampled = match strength {
            Some(v) if v.is_nan() => return Err(()),
            Some(v) if v < 1.0 => {
                let n = (steps as f64 * v).trunc();
                if !n.is_finite() {
                    return Err(());
                }
                n + if n == steps as f64 { 0.0 } else { 1.0 }
            }
            _ => steps as f64,
        };
        let optimized = self.cpu_optimized(&model, width, height, steps, edit);
        let estimate = self.zimage_estimates();
        let e = &estimate[&model];
        let mpx = width as f64 * height as f64 / 1e6;
        let mut per_image = e["s_per_mpx_step"].as_f64().ok_or(())?
            * mpx.powf(e["pixel_exponent"].as_f64().ok_or(())?)
            * sampled;
        if edit {
            per_image += estimate["vae_encode_s_per_mpx"].as_f64().ok_or(())? * mpx;
        }
        let mut seconds = e["fixed_s"].as_f64().ok_or(())? + per_image * count as f64;
        if !optimized {
            seconds *= estimate["unoptimized_factor"].as_f64().ok_or(())?;
        }
        Ok(
            json!({"seconds":rounded(seconds,1),"optimized":optimized,"calibrated":estimate["calibrated"]}),
        )
    }
    pub(super) fn image_models(&self) -> PathBuf {
        self.stack.join("models/images")
    }
    pub(super) fn klein_dir(&self) -> PathBuf {
        self.image_models().join("flux2-klein")
    }
    pub(super) fn klein_missing(&self, model: &str) -> Vec<String> {
        [
            self.klein_dir().join(if model == "q4" {
                "flux-2-klein-4b-Q4_0.gguf"
            } else {
                "flux-2-klein-4b-Q8_0.gguf"
            }),
            self.klein_dir().join("Qwen3-4B-Q8_0.gguf"),
            self.klein_dir().join("flux2-vae.safetensors"),
        ]
        .into_iter()
        .filter(|p| !p.is_file())
        .map(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
    }
    fn oxi_binary(&self) -> PathBuf {
        variable("OXI_BIN", self.stack.join("releases/oxi/oxi"))
    }
    pub(super) fn oxi_missing(&self) -> Vec<String> {
        let mut missing = Vec::new();
        for path in [
            self.oxi_binary(),
            variable("SD_CLI_PROBE", self.stack.join("releases/sd-probe/sd-cli")),
        ] {
            if !executable(&path) {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                missing.push(if name == "sd-cli" {
                    "sd-cli (probe)".into()
                } else {
                    name.into_owned()
                });
            }
        }
        missing.extend(self.klein_missing("q8"));
        missing
    }
    pub(super) fn engine_setting(&self) -> String {
        let v = read(&self.project.join("engine.json"));
        v["engine"]
            .as_str()
            .filter(|s| ["auto", "oxi", "sdcpp"].contains(s))
            .unwrap_or("auto")
            .into()
    }
    pub(super) fn set_engine(&self, value: &str) -> std::io::Result<()> {
        fs::create_dir_all(&self.project)?;
        let temporary = self
            .project
            .join(format!(".engine.json.{}.tmp", std::process::id()));
        fs::write(
            &temporary,
            crate::paths::python_json(&json!({"engine":value})) + "\n",
        )?;
        fs::rename(temporary, self.project.join("engine.json"))
    }
    fn vae_conv(&self) -> String {
        let value = read(&self.project.join("runtime-settings.json"));
        value["oxi"]["vae_conv"]
            .as_str()
            .filter(|s| ["f32", "winograd", "int8"].contains(s))
            .unwrap_or("f32")
            .into()
    }
    fn oxi_sha(&self) -> Option<String> {
        let path = self.oxi_binary();
        let info = fs::metadata(&path).ok()?;
        let stamp = info.mtime() as i128 * 1_000_000_000 + info.mtime_nsec() as i128;
        let mut cache = self.sha_cache.lock().ok()?;
        if let Some((p, size, mtime, sha)) = &*cache {
            if p == &path && *size == info.len() && *mtime == stamp {
                return Some(sha.clone());
            }
        }
        let digest = crate::bootstrap::sha(&path).ok()?;
        *cache = Some((path, info.len(), stamp, digest.clone()));
        Some(digest)
    }
    fn size_gate(&self, width: i64, height: i64) -> bool {
        (|| -> Option<bool> {
            let path = variable("OXI_SIZE_GATE_DIR", self.project.join("oxi-size-gates"))
                .join(format!("{width}x{height}.json"));
            let info = fs::symlink_metadata(&path).ok()?;
            if !info.is_file()
                || info.uid() != nix::unistd::getuid().as_raw()
                || info.len() > 1024 * 1024
            {
                return Some(false);
            }
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
                .open(path)
                .ok()?;
            let mut raw = Vec::new();
            Read::by_ref(&mut file)
                .take(1024 * 1024 + 1)
                .read_to_end(&mut raw)
                .ok()?;
            if raw.len() > 1024 * 1024 {
                return Some(false);
            }
            let record: Value = serde_json::from_slice(&raw).ok()?;
            if record["schema"] != "oxi-size-gate/1"
                || integer(&record["width"]) != Some(width)
                || integer(&record["height"]) != Some(height)
                || record["verdict"] != "PASS"
            {
                return Some(false);
            }
            if !record["seeds"].as_array().is_some_and(|a| {
                !a.is_empty() && a.iter().all(|v| v.is_object() && v["pass"] == true)
            }) {
                return Some(false);
            }
            let expected = record["oxi_sha256"].as_str()?;
            if expected.len() != 64
                || !expected
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Some(false);
            }
            let settings = record
                .get("oxi_settings")
                .filter(|v| truthy(v))
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !settings.is_object() {
                return Some(false);
            }
            for (key, current) in [
                ("chunk1", "32".into()),
                ("chunk2", "32".into()),
                ("attn_prec", "f32".into()),
                ("vae_conv", self.vae_conv()),
            ] {
                if let Some(value) = settings.get(key).filter(|v| !v.is_null()) {
                    let supplied = value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string());
                    if supplied != current {
                        return Some(false);
                    }
                }
            }
            Some(self.oxi_sha().as_deref() == Some(expected))
        })()
        .unwrap_or(false)
    }
    fn gated_sizes(&self) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(variable(
            "OXI_SIZE_GATE_DIR",
            self.project.join("oxi-size-gates"),
        ))
        .into_iter()
        .flatten()
        .filter_map(|v| v.ok())
        .map(|v| v.file_name().to_string_lossy().into_owned())
        .collect();
        names.sort();
        names
            .into_iter()
            .filter_map(|name| {
                let raw = name.strip_suffix(".json")?;
                let (w, h) = raw.split_once('x')?;
                if !(2..=4).contains(&w.len())
                    || !(2..=4).contains(&h.len())
                    || !w.bytes().chain(h.bytes()).all(|b| b.is_ascii_digit())
                {
                    return None;
                }
                let width = w.parse().ok()?;
                let height = h.parse().ok()?;
                ((width, height) != (512, 512) && self.size_gate(width, height))
                    .then(|| format!("{width}x{height}"))
            })
            .collect()
    }
    pub(super) fn engine_info(&self) -> Value {
        let missing = self.oxi_missing();
        json!({"engine":self.engine_setting(),"choices":["auto","oxi","sdcpp"],"labels":{
            "auto":"auto: oxi at 512 × 512 (and sizes with a passed size gate), sd.cpp otherwise or if oxi fails",
            "oxi":"oxi: 512 × 512 (and gated sizes) on oxi, no fallback when oxi fails (other sizes still sd.cpp)",
            "sdcpp":"sd.cpp: every size on sd-cli (slower)"},"oxi_ready":missing.is_empty(),"missing":missing,"oxi_sizes":["512x512"],"oxi_gated_sizes":self.gated_sizes(),"note":"1024 px and other sizes stay on sd.cpp (slower); edits, refine and upscale use sd.cpp"})
    }
    fn edit_settings(&self) -> Value {
        let mut estimate = json!({"calibrated":false,"fixed_s":30.0,"s_per_mpx_step":80.0,"pixel_exponent":1.5,"vae_encode_s_per_mpx":10.0,"composite_s_per_mpx":3.0});
        if let Some(values) = read(&self.project.join("edit.json"))["estimate"].as_object() {
            for (key, value) in values {
                if key == "calibrated" && value.is_boolean() {
                    estimate[key] = value.clone();
                } else if estimate.get(key).is_some() && key != "calibrated" {
                    if let Some(n) = bounded(value, 0.0, 10000.0) {
                        estimate[key] = json!(n);
                    }
                }
            }
        }
        estimate
    }
    pub(super) fn edit_seconds(&self, width: i64, height: i64, t2i: bool, count: i64) -> f64 {
        let e = self.edit_settings();
        let mpx = width as f64 * height as f64 / 1e6;
        let sampling = e["s_per_mpx_step"].as_f64().unwrap_or(80.0)
            * mpx.powf(e["pixel_exponent"].as_f64().unwrap_or(1.5))
            * 4.0;
        rounded(
            e["fixed_s"].as_f64().unwrap_or(30.0)
                + if t2i {
                    count as f64 * sampling * 0.5
                } else {
                    sampling + e["vae_encode_s_per_mpx"].as_f64().unwrap_or(10.0) * mpx
                },
            1,
        )
    }
    fn edit_info(&self, t2i: bool) -> Value {
        let q4 = self.klein_missing("q4").is_empty();
        let q8 = self.klein_missing("q8").is_empty();
        let calibrated = self.edit_settings()["calibrated"].clone();
        let mut estimates = serde_json::Map::new();
        for size in if t2i {
            vec![512, 768, 1024]
        } else {
            vec![512, 768]
        } {
            estimates.insert(
                size.to_string(),
                json!(self.edit_seconds(size, size, t2i, 1)),
            );
        }
        if t2i {
            json!({"ready":q4||q8,"models":{"klein-q4":q4,"klein-q8":q8},"steps":4,"sizes":[256,1024],"estimates":estimates,"estimate_label":if calibrated==true{"estimate"}else{"uncalibrated estimate"},"estimates_calibrated":calibrated})
        } else {
            json!({"ready":q4||q8,"models":{"q4":q4,"q8":q8},"sizes":[512,768],"default_size":512,"steps":4,"estimates":estimates,"estimates_calibrated":calibrated})
        }
    }
    fn presets(&self) -> Value {
        let mut defaults = json!({"draft":{"model":"q4","width":256,"height":256,"steps":8},"final":{"model":"q4","width":1024,"height":1024,"steps":8},"draft512":{"model":"klein-q8","width":512,"height":512,"steps":4}});
        let values = read(&self.project.join("presets.json"));
        for key in ["draft", "final", "draft512"] {
            let value = &values[key];
            if values.get(key).is_some() && !value.is_object() {
                break;
            }
            let (Some(w), Some(h), Some(steps), Some(model)) = (
                integer(&value["width"]),
                integer(&value["height"]),
                integer(&value["steps"]),
                value["model"].as_str(),
            ) else {
                continue;
            };
            let valid = if key == "draft512" {
                ["klein-q8", "klein-q4"].contains(&model)
                    && (256..=1024).contains(&w)
                    && (256..=1024).contains(&h)
                    && w % 16 == 0
                    && h % 16 == 0
                    && steps == 4
            } else {
                ["q4", "q8"].contains(&model)
                    && (256..=1536).contains(&w)
                    && (256..=1536).contains(&h)
                    && w % 64 == 0
                    && h % 64 == 0
                    && w * h <= 1536 * 1024
                    && (1..=50).contains(&steps)
            };
            if valid {
                defaults[key] = json!({"model":model,"width":w,"height":h,"steps":steps});
                if let Some(seconds) =
                    bounded(&value["expected_seconds"], 0.0, 14400.0).filter(|n| *n > 0.0)
                {
                    let _ = seconds;
                    defaults[key]["expected_seconds"] = value["expected_seconds"].clone();
                }
            }
        }
        defaults
    }
    fn upscaler(&self, value: &Value) -> bool {
        let Some(s) = value.as_str().filter(|s| !s.is_empty()) else {
            return false;
        };
        let path = Path::new(s);
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            self.image_models().join(path)
        };
        let Ok(path) = fs::canonicalize(path) else {
            return false;
        };
        let Ok(root) = fs::canonicalize(self.image_models()) else {
            return false;
        };
        path.starts_with(root)
            && path.is_file()
            && path.extension().is_some_and(|e| {
                ["pth", "safetensors", "gguf"]
                    .contains(&e.to_string_lossy().to_lowercase().as_str())
            })
    }
    pub(super) fn upscale_info(&self) -> Value {
        let value = read(&self.project.join("upscale.json"));
        let mut targets = vec![768, 1024];
        let edge = |v: &Value| integer(v).filter(|n| (256..=1536).contains(n) && n % 64 == 0);
        if let Some(values) = value["refine_targets"]
            .as_array()
            .filter(|v| !v.is_empty() && v.iter().all(|v| edge(v).is_some()))
        {
            targets = values.iter().filter_map(edge).collect();
            targets.sort();
            targets.dedup();
        }
        let target = edge(&value["refine_target"]).unwrap_or(1024);
        if !targets.contains(&target) {
            targets.push(target);
            targets.sort();
        }
        let strength = bounded(&value["refine_strength"], 0.05, 0.95).unwrap_or(0.45);
        let esrgan = value
            .get("esrgan_model")
            .filter(|v| v.is_null() || v.is_string())
            .cloned()
            .unwrap_or(json!("upscale/RealESRGAN_x4plus.pth"));
        let x2 = value
            .get("esrgan_x2_model")
            .filter(|v| v.is_null() || v.is_string())
            .cloned()
            .unwrap_or(Value::Null);
        let scale = if number(&value["esrgan_scale"]).is_some_and(|n| n == 2.0 || n == 4.0) {
            value["esrgan_scale"].clone()
        } else {
            json!(4)
        };
        json!({"esrgan":self.upscaler(&esrgan),"esrgan_x2":self.upscaler(&x2),"esrgan_scale":scale,"refine_targets":targets,"refine_target":target,"refine_strength":strength,"estimates_calibrated":value["estimate"]["calibrated"].as_bool().unwrap_or(false),"max_variations":4})
    }
    pub(super) fn models(&self) -> Result<Value, ()> {
        let mut out = serde_json::Map::new();
        for (key, filename) in [
            ("q4", "z_image_turbo-Q4_K.gguf"),
            ("q8", "z_image_turbo-Q8_0.gguf"),
        ] {
            let path = self.image_models().join(filename);
            let partial = path.with_file_name(format!("{filename}.part"));
            let mb = fs::metadata(partial)
                .map(|m| (m.len() as f64 / 1048576.0).round_ties_even() as u64)
                .unwrap_or(0);
            out.insert(key.into(), json!({"ready":path.is_file(),"download_mb":mb}));
        }
        let cpu = read(&self.project.join("cpu-profile.json"));
        let mut quality = false;
        if cpu["validated"] == true {
            if let Some(dimensions) = cpu.get("dimensions") {
                let dimensions = dimensions.as_array().ok_or(())?;
                quality = dimensions.iter().any(|v| {
                    v.as_array().is_some_and(|p| {
                        p.len() == 2
                            && p[0].as_f64() == Some(1024.0)
                            && p[1].as_f64() == Some(1024.0)
                    })
                });
            }
        }
        if !quality {
            if let Some(runs) = parsed(&self.project.join("benchmarks/results.json")) {
                for row in runs.as_array().ok_or(())? {
                    let row = row.as_object().ok_or(())?;
                    if row.get("status") == Some(&json!("ok")) {
                        let numeric = |key: &str| -> Result<f64, ()> {
                            match row.get(key) {
                                None => Ok(0.0),
                                Some(Value::Bool(v)) => Ok(if *v { 1.0 } else { 0.0 }),
                                Some(v) => v.as_f64().ok_or(()),
                            }
                        };
                        if numeric("size")? >= 1024.0 && numeric("steps")? >= 8.0 {
                            quality = true;
                            break;
                        }
                    }
                }
            }
        }
        out.insert("presets".into(), self.presets());
        out.insert("quality_ready".into(), json!(quality));
        let gpu = read(&self.project.join("gpu-profile.json"));
        out.insert(
            "gpu".into(),
            if truthy(&gpu["enabled"]) && truthy(&gpu["validated"]) {
                json!({"optimized":true,"models":gpu.get("models").cloned().unwrap_or(json!({}))})
            } else {
                json!({"optimized":false})
            },
        );
        out.get_mut("q8").expect("q8 model")["setup_state"] =
            match parsed(&self.project.join("q8-status.json")) {
                None => json!("not downloaded"),
                Some(value) => value
                    .as_object()
                    .ok_or(())?
                    .get("state")
                    .cloned()
                    .unwrap_or_else(|| json!("")),
            };
        let benchmarks = match parsed(&self.project.join("overnight-status.json")) {
            None => String::new(),
            Some(value) => format!(
                "Benchmarks: {}",
                value
                    .as_object()
                    .ok_or(())?
                    .get("state")
                    .map(|v| v.as_str().ok_or(()))
                    .transpose()?
                    .unwrap_or("")
            ),
        };
        out.insert("benchmarks".into(), json!(benchmarks));
        out.insert(
            "edit_presets".into(),
            json!({"light":0.3,"restyle":0.55,"reimagine":0.8}),
        );
        out.insert("default_edit_preset".into(), json!("light"));
        out.insert("upscale".into(), self.upscale_info());
        out.insert("edit".into(), self.edit_info(false));
        out.insert("klein".into(), self.edit_info(true));
        out.insert("engine".into(), self.engine_info());
        Ok(Value::Object(out))
    }
}

fn decimal_ascii(value: &str) -> String {
    static RANGES: std::sync::OnceLock<Vec<(u32, u32)>> = std::sync::OnceLock::new();
    let ranges = RANGES.get_or_init(|| {
        let rx = regex::Regex::new(r"\\u\{([0-9a-f]+)\}(?:-\\u\{([0-9a-f]+)\})?")
            .expect("decimal range expression");
        rx.captures_iter(include_str!("receipt-decimal-class.txt"))
            .map(|c| {
                let start = u32::from_str_radix(&c[1], 16).expect("decimal range");
                let end = c.get(2).map_or(start, |s| {
                    u32::from_str_radix(s.as_str(), 16).expect("decimal range")
                });
                (start, end)
            })
            .collect()
    });
    value
        .chars()
        .map(|ch| {
            let point = ch as u32;
            if let Some((start, _)) = ranges.iter().find(|(a, b)| (*a..=*b).contains(&point)) {
                char::from(b'0' + ((point - start) % 10) as u8)
            } else {
                ch
            }
        })
        .collect()
}
pub(super) fn python_int(value: &str) -> Option<i64> {
    static RX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let value = decimal_ascii(value);
    let value = value.trim();
    RX.get_or_init(|| regex::Regex::new(r"^[+-]?[0-9](?:_?[0-9])*\z").expect("integer expression"))
        .is_match(value)
        .then(|| value.replace('_', "").parse().ok())
        .flatten()
}
pub(super) fn python_float(value: &str) -> Option<f64> {
    static RX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let value = decimal_ascii(value);
    let value = value.trim();
    if !RX.get_or_init(||regex::Regex::new(r"(?i)^[+-]?(?:(?:[0-9](?:_?[0-9])*(?:\.(?:[0-9](?:_?[0-9])*)?)?|\.[0-9](?:_?[0-9])*)(?:e[+-]?[0-9](?:_?[0-9])*)?|inf(?:inity)?|nan)\z").expect("float expression")).is_match(value){return None;}
    value.replace('_', "").parse().ok()
}
