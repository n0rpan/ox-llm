//! Studio CPU job ownership, anonymous prompts and private in-memory image records.
use super::{data, error, plans::Generation, Studio};
use axum::response::Response;
use nix::{
    fcntl::{Flock, FlockArg},
    sys::memfd::{memfd_create, MFdFlags},
};
use serde_json::{json, Map, Value};
use std::{
    fs::{File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    os::unix::{fs::OpenOptionsExt, process::ExitStatusExt},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
};
use tokio::io::AsyncReadExt;

#[derive(Default)]
pub(super) struct Store {
    jobs: Map<String, Value>,
    images: Map<String, Value>,
}
fn uuid() -> Result<String, getrandom::Error> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b)?;
    b[6] = (b[6] & 15) | 64;
    b[8] = (b[8] & 63) | 128;
    Ok(b.iter().map(|b| format!("{b:02x}")).collect())
}
fn read(path: PathBuf) -> Value {
    std::fs::read(path)
        .ok()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or(Value::Null)
}
struct Plan {
    binary: PathBuf,
    args: Vec<String>,
    prompt: String,
    records: Vec<Value>,
    outputs: Vec<PathBuf>,
    model: String,
    device: String,
    engine: Option<(String, String)>,
    repack: bool,
}
impl Studio {
    pub(super) fn activity(&self) -> Value {
        let store = self.jobs.lock().expect("image state");
        let active: Vec<_> = store
            .jobs
            .values()
            .filter(|v| !matches!(v["state"].as_str(), Some("done" | "error")))
            .collect();
        let first = active.first();
        json!({"state":if active.is_empty(){"idle"}else{"generating"},"active_jobs":active.len(),"model":first.map(|v|&v["model"]).filter(|v|matches!(v.as_str(),Some("q4"|"q8"))),"device":first.map(|v|&v["device"]).filter(|v|matches!(v.as_str(),Some("cpu"|"gpu")))})
    }
    pub(super) fn job(&self, query: &str) -> Response {
        let id = reqwest::Url::parse(&format!("http://localhost/?{query}"))
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(k, v)| k == "id" && !v.is_empty())
                    .map(|(_, v)| v.into_owned())
            });
        let value = id.and_then(|id| {
            self.jobs
                .lock()
                .expect("image state")
                .jobs
                .get(&id)
                .cloned()
        });
        match value {
            Some(value) => data(200, value),
            None => data(404, json!({"state":"error","error":"no such job"})),
        }
    }
    fn plan_generation(&self, input: Generation, body: &Value) -> Result<Plan, String> {
        if !input.cpu {
            return Err("GPU generation is paused; choose CPU.".into());
        }
        if body.get("init").is_some_and(crate::release_files::truthy) {
            return Err("Upload the photo to edit first".into());
        }
        let model_key = if input.klein { "klein" } else { &input.model };
        let profile = if input.klein {
            None
        } else {
            self.cpu_profile(&input.model, input.width, input.height, input.steps, false)?
        };
        let tuning = read(self.project.join("runtime-settings.json"));
        let threads = tuning[model_key]
            .get("threads")
            .and_then(super::plans::integer)
            .unwrap_or(6);
        let attention = tuning[model_key]["flash_attention"]
            .as_str()
            .unwrap_or("off");
        let token = uuid().map_err(|_| "Image identifier unavailable")?;
        let stem = format!(
            "{}-{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            &token[..6]
        );
        let output = self.output.join(if input.count == 1 {
            format!("{stem}.png")
        } else {
            format!("{stem}-%d.png")
        });
        let outputs: Vec<_> = (0..input.count)
            .map(|i| {
                if input.count == 1 {
                    output.clone()
                } else {
                    self.output.join(format!("{stem}-{i}.png"))
                }
            })
            .collect();
        let (diffusion, llm, vae) = if input.klein {
            (
                self.klein_dir().join(if input.model == "klein-q4" {
                    "flux-2-klein-4b-Q4_0.gguf"
                } else {
                    "flux-2-klein-4b-Q8_0.gguf"
                }),
                self.klein_dir().join("Qwen3-4B-Q8_0.gguf"),
                self.klein_dir().join("flux2-vae.safetensors"),
            )
        } else {
            (
                self.image_models().join(if input.model == "q4" {
                    "z_image_turbo-Q4_K.gguf"
                } else {
                    "z_image_turbo-Q8_0.gguf"
                }),
                self.image_models()
                    .join("Qwen3-4B-Instruct-2507-Q4_K_M.gguf"),
                self.image_models().join("vae/vae.safetensors"),
            )
        };
        let mut args = vec![
            "--diffusion-model".into(),
            diffusion.to_string_lossy().into_owned(),
            "--llm".into(),
            llm.to_string_lossy().into_owned(),
            "--vae".into(),
            vae.to_string_lossy().into_owned(),
            "--cfg-scale".into(),
            "1.0".into(),
        ];
        if input.klein {
            args.extend(["--sampling-method".into(), "euler".into()]);
        }
        args.extend([
            "--steps".into(),
            input.steps.to_string(),
            "-W".into(),
            input.width.to_string(),
            "-H".into(),
            input.height.to_string(),
            "-o".into(),
            output.to_string_lossy().into_owned(),
            "--threads".into(),
            threads.to_string(),
            "--flash-attn".into(),
            format!("off,diffusion={attention}"),
            "--vae-tiling".into(),
            "--vae-tile-size".into(),
            "512".into(),
            "--disable-image-metadata".into(),
        ]);
        if input.klein {
            args.extend(["--backend".into(), "cpu".into()]);
        }
        if profile.is_some() {
            args.extend(["--auto-fit".into(), "off".into()]);
        }
        if input.seed.is_some() || input.count > 1 {
            args.extend(["--seed".into(), input.seed.unwrap_or(42).to_string()]);
        }
        if input.count > 1 {
            args.extend(["-b".into(), input.count.to_string()]);
        }
        if !input.klein {
            args.extend(["--backend".into(), "cpu".into()]);
        }
        let engine = if input.klein {
            let estimate = self
                .estimate(&format!(
                    "model={}&width={}&height={}",
                    input.model, input.width, input.height
                ))
                .map_err(|_| "Image routing unavailable")?;
            if estimate["engine"] == "oxi" {
                return Err("The selected image engine is not available.".into());
            }
            Some((
                "sd.cpp".into(),
                estimate["engine_reason"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
            ))
        } else {
            None
        };
        let records=outputs.iter().enumerate().map(|(i,path)|{
            let mut record=json!({"path":path,"prompt":input.prompt,"seed":input.seed.unwrap_or(42)+i as i64,"model":input.model,"width":input.width,"height":input.height,"steps":input.steps,"kind":"image"});
            if input.klein{record["engine"]=json!("klein-t2i");record["backend"]=json!("sd.cpp");}record
        }).collect();
        let binary = profile
            .as_ref()
            .and_then(|v| v["binary"].as_str())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("SD_CLI").map(PathBuf::from))
            .unwrap_or_else(|| self.stack.join("src/stable-diffusion.cpp/build/bin/sd-cli"));
        Ok(Plan {
            binary,
            args,
            prompt: input.prompt,
            records,
            outputs,
            model: input.model,
            device: "cpu".into(),
            engine,
            repack: profile.is_some(),
        })
    }
    pub(super) async fn start_generation(
        self: &Arc<Self>,
        input: Generation,
        body: &Value,
    ) -> Response {
        let plan = match self.plan_generation(input, body) {
            Ok(v) => v,
            Err(e) => return error(400, &e),
        };
        let _admission = self.admission.lock().await;
        if self
            .jobs
            .lock()
            .expect("image state")
            .jobs
            .values()
            .any(|v| !matches!(v["state"].as_str(), Some("done" | "error")))
        {
            return error(200, "A generation is already running");
        }
        let file = match OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o666)
            .open(&self.lease)
        {
            Ok(v) => v,
            Err(_) => {
                return error(
                    200,
                    "Another image or chat job is running; please try again shortly",
                )
            }
        };
        let lease = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(v) => v,
            Err((file, _)) => {
                let client = match reqwest::Client::builder()
                    .no_proxy()
                    .timeout(std::time::Duration::from_secs(15))
                    .build()
                {
                    Ok(v) => v,
                    Err(_) => {
                        return error(
                            200,
                            "Another image or chat job is running; please try again shortly",
                        )
                    }
                };
                let unloaded = match client
                    .post("http://127.0.0.1:8092/_local/unload")
                    .json(&json!({}))
                    .send()
                    .await
                {
                    Ok(reply) if reply.status().as_u16() == 200 => reply
                        .json::<Value>()
                        .await
                        .is_ok_and(|v| v["state"] == "unloaded"),
                    _ => false,
                };
                if !unloaded {
                    return error(
                        200,
                        "Another image or chat job is running; please try again shortly",
                    );
                }
                match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                    Ok(v) => v,
                    Err(_) => {
                        return error(
                            200,
                            "Another image or chat job is running; please try again shortly",
                        )
                    }
                }
            }
        };
        let id = match uuid() {
            Ok(v) => v,
            Err(_) => return error(500, "Image identifier unavailable"),
        };
        let mut job = json!({"state":"queued","model":plan.model,"device":plan.device});
        if let Some((engine, reason)) = &plan.engine {
            job["engine"] = json!(engine);
            job["engine_reason"] = json!(reason);
        }
        self.jobs
            .lock()
            .expect("image state")
            .jobs
            .insert(id.clone(), job);
        let studio = self.clone();
        let job_id = id.clone();
        let task = tokio::spawn(async move {
            studio.run_cpu(job_id, plan, lease).await;
        });
        self.job_tasks.lock().expect("image tasks").push(task);
        data(200, json!({"id":id}))
    }
    fn set_job(&self, id: &str, key: &str, value: Value) {
        if let Some(job) = self.jobs.lock().expect("image state").jobs.get_mut(id) {
            job[key] = value;
        }
    }
    fn progress(&self, id: &str, line: &str) {
        let lower = line.to_lowercase();
        let mut store = self.jobs.lock().expect("image state");
        let Some(job) = store.jobs.get_mut(id) else {
            return;
        };
        if lower.contains("sampling completed") {
            job["phase"] = json!("decoding");
            job["state"] = json!("preparing decoder");
        } else if lower.contains("decoding") || lower.contains("decode_first_stage") {
            job["phase"] = json!("decoding");
            job["state"] = json!("decoding");
        } else if lower.contains("generating image:") {
            job["phase"] = json!("sampling");
            job["state"] = json!("starting sampling");
        } else if lower.contains("get_learned_condition completed") {
            job["state"] = json!("prompt encoded");
        } else if lower.contains("loading tensors completed") {
            job["state"] = json!(match job["phase"].as_str() {
                Some("decoding") => "decoding",
                Some("sampling") => "starting sampling",
                _ => "preparing generation",
            });
        } else if lower.contains("loading tensors") {
            job["state"] = json!(if job["phase"] == "decoding" {
                "decoding"
            } else {
                "loading model"
            });
        } else {
            static RX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            if let Some(m) = RX
                .get_or_init(|| {
                    regex::Regex::new(r"(\d+)/(\d+) - [\d.]+s/it").expect("progress expression")
                })
                .captures(line)
            {
                job["state"] = json!(format!(
                    "{} {}/{}",
                    if job["phase"] == "decoding" {
                        "decoding"
                    } else {
                        "generating"
                    },
                    &m[1],
                    &m[2]
                ));
            }
        }
    }
    fn actions(&self, record: &Value) -> Value {
        let settings = self.upscale_info();
        let mut refine = Map::new();
        let width = record["width"].as_f64().unwrap_or(512.0);
        let height = record["height"].as_f64().unwrap_or(512.0);
        let model = record["model"]
            .as_str()
            .unwrap_or("q8")
            .trim_start_matches("klein-");
        let steps = if record["engine"] == "klein-t2i" {
            8
        } else {
            record["steps"].as_i64().unwrap_or(8)
        };
        if record["engine"] != "klein" {
            for target in settings["refine_targets"].as_array().into_iter().flatten() {
                let target = target.as_f64().unwrap_or(1024.0);
                let scale = target / width.max(height);
                let w = (width * scale / 64.0).round_ties_even().max(4.0) as i64 * 64;
                let h = (height * scale / 64.0).round_ties_even().max(4.0) as i64 * 64;
                if w > 1536 || h > 1536 || w * h > 1536 * 1024 {
                    continue;
                }
                let query = format!(
                    "model={model}&width={w}&height={h}&steps={steps}&edit=1&strength={}",
                    settings["refine_strength"]
                );
                if let Ok(value) = self.estimate(&query) {
                    refine.insert(format!("{target:.0}"), value["seconds"].clone());
                }
            }
        }
        let upscale = if settings["esrgan"] == true {
            let scale = settings["esrgan_scale"].as_f64().unwrap_or(4.0);
            if width * scale <= 4096.0 && height * scale <= 4096.0 {
                json!((3.0 + 20.0 * width * scale * height * scale / 1e6) * 10.0)
                    .as_f64()
                    .map(|v| json!(v.round_ties_even() / 10.0))
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        } else {
            Value::Null
        };
        json!({"upscale":upscale,"refine":refine,"post_upscale":settings["esrgan_x2"]})
    }
    async fn run_cpu(self: Arc<Self>, id: String, plan: Plan, _lease: Flock<File>) {
        self.set_job(&id, "step", json!(""));
        self.set_job(&id, "state", json!("starting"));
        let result = self.run_command(&id, &plan).await;
        let code = match result {
            Ok(code) => code,
            Err(message) => {
                self.set_job(&id, "error", json!(message));
                self.set_job(&id, "state", json!("error"));
                return;
            }
        };
        if code != 0 || plan.outputs.iter().any(|p| !p.exists()) {
            self.set_job(
                &id,
                "error",
                json!(format!(
                    "sd-cli exited {code}{}",
                    if code == 0 {
                        " without writing its image"
                    } else {
                        ""
                    }
                )),
            );
            self.set_job(&id, "state", json!("error"));
            return;
        }
        let mut images = Vec::new();
        for record in plan.records {
            let image_id = match uuid() {
                Ok(v) => v,
                Err(_) => {
                    self.set_job(&id, "state", json!("error"));
                    return;
                }
            };
            let actions = self.actions(&record);
            let mut entry = json!({"id":image_id,"path":record["path"],"seed":record["seed"],"width":record["width"],"height":record["height"],"kind":record["kind"],"actions":actions});
            if let Some(backend) = record.get("backend") {
                entry["backend"] = backend.clone();
            }
            {
                let mut store = self.jobs.lock().expect("image state");
                store.images.insert(image_id, record);
                while store.images.len() > 200 {
                    let first = store.images.keys().next().cloned().expect("image record");
                    store.images.shift_remove(&first);
                }
            }
            images.push(entry);
        }
        let mut store = self.jobs.lock().expect("image state");
        if let Some(job) = store.jobs.get_mut(&id) {
            job["images"] = json!(images);
            if let Some(first) = images.first() {
                job["path"] = first["path"].clone();
            }
            job.as_object_mut().expect("job").shift_remove("step");
            job["state"] = json!("done");
        }
    }
    async fn run_command(&self, id: &str, plan: &Plan) -> Result<i32, String> {
        let descriptor =
            memfd_create(c"prompt-studio", MFdFlags::MFD_CLOEXEC).map_err(|e| e.to_string())?;
        let mut prompt = File::from(descriptor);
        prompt
            .write_all(plan.prompt.as_bytes())
            .map_err(|e| e.to_string())?;
        prompt.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        let mut command =
            tokio::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
        command
            .arg("__studio_exec")
            .arg(&plan.binary)
            .args(&plan.args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(prompt))
            .kill_on_drop(true);
        if plan.repack {
            command.env("SD_CPU_REPACK_EXPERIMENT", "1");
        }
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let mut output = child.stdout.take().ok_or("Image output pipe unavailable")?;
        let mut stop = self.stopping.subscribe();
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            tokio::select! {
                value=output.read(&mut byte)=>match value {
                    Ok(0)=>break,
                    Err(_)=>{
                        let _=child.kill().await;
                        return Err("Image output pipe failed".into());
                    },
                    Ok(_)=>{
                        if byte[0]==b'\r'||byte[0]==b'\n' {
                            self.progress(id,&String::from_utf8_lossy(&line));
                            line.clear();
                        } else {
                            line.push(byte[0]);
                            if line.len()>8192 {line.drain(..line.len()-8192);}
                            if line.ends_with(b"s/it") {self.progress(id,&String::from_utf8_lossy(&line));}
                        }
                    },
                },
                _=stop.changed()=>{
                    let _=child.kill().await;
                    let _=child.wait().await;
                    return Err("Image generation stopped".into());
                },
            }
        }
        if !line.is_empty() {
            self.progress(id, &String::from_utf8_lossy(&line));
        }
        let status = child.wait().await.map_err(|e| e.to_string())?;
        Ok(status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1)))
    }
}
