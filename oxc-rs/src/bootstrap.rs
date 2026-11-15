//! Source bundle identity checks, shared by staging and create-only installation.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub type Result<T> = std::result::Result<T, String>;
pub fn io(error: impl std::fmt::Display) -> String {
    error.to_string()
}
pub fn relative(raw: &str) -> Result<PathBuf> {
    if raw.is_empty()
        || Path::new(raw).is_absolute()
        || raw.split('/').any(|s| matches!(s, "." | ".."))
        || raw.contains('\\')
    {
        return Err("safe relative path required".into());
    }
    Ok(PathBuf::from(raw))
}
pub fn sha(path: &Path) -> Result<String> {
    hash(File::open(path).map_err(io)?)
}
pub fn hash(mut file: File) -> Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buffer).map_err(io)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
pub fn read_json(path: &Path) -> Result<Value> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(io)?;
    let info = file.metadata().map_err(io)?;
    if !info.is_file()
        || info.uid() != nix::unistd::getuid().as_raw()
        || info.len() > 4 * 1024 * 1024
    {
        return Err("unsafe_or_oversized_json".into());
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("oversized_json".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid_json".into())
}
pub fn inventory(root: &Path) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    fn walk(
        root: &Path,
        path: &Path,
        files: &mut BTreeSet<String>,
        dirs: &mut BTreeSet<String>,
    ) -> Result<()> {
        for item in fs::read_dir(path).map_err(io)? {
            let item = item.map_err(io)?;
            let path = item.path();
            let info = fs::symlink_metadata(&path).map_err(io)?;
            if info.is_symlink() {
                return Err("stage symlink refused".into());
            }
            let name = path
                .strip_prefix(root)
                .map_err(io)?
                .to_string_lossy()
                .into_owned();
            if info.is_dir() {
                if info.mode() & 0o777 != 0o700 {
                    return Err("staged directory is not private".into());
                }
                dirs.insert(name);
                walk(root, &path, files, dirs)?;
            } else if info.is_file() {
                files.insert(name);
            }
        }
        Ok(())
    }
    let mut files = BTreeSet::new();
    let mut dirs = BTreeSet::new();
    if root.exists() {
        walk(root, root, &mut files, &mut dirs)?;
    }
    Ok((files, dirs))
}
pub fn verify_stage(destination: &Path) -> Result<Value> {
    // The source CLI allows a manifest read without owner checks; installation
    // adds the stricter read_json check before invoking this verifier.
    let manifest: Value =
        serde_json::from_slice(&fs::read(destination.join("stage-manifest.json")).map_err(io)?)
            .map_err(io)?;
    if json!(sha(&destination.join("bootstrap-plan.json"))?) != manifest["plan_sha256"] {
        return Err("stage plan changed".into());
    }
    let (files, dirs) = inventory(&destination.join("payload"))?;
    let listed = manifest["files"]
        .as_object()
        .ok_or("invalid stage file inventory")?;
    let listed_dirs = manifest["directories"]
        .as_array()
        .ok_or("invalid stage directory inventory")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "invalid staged directory".into())
        })
        .collect::<Result<BTreeSet<_>>>()?;
    if dirs != listed_dirs {
        return Err("unexpected or missing staged directory".into());
    }
    if files != listed.keys().cloned().collect() {
        return Err("unexpected or missing staged file".into());
    }
    for (name, row) in listed {
        let path = destination.join("payload").join(relative(name)?);
        let info = fs::metadata(&path).map_err(io)?;
        if json!(info.len()) != row["bytes"]
            || json!(sha(&path)?) != row["sha256"]
            || json!(info.mode() & 0o777) != row["mode"]
        {
            return Err("staged payload identity changed".into());
        }
    }
    Ok(
        json!({"model_free_stage_verified":true,"files":files.len(),"installed":false,"end_to_end_parity_passed":false}),
    )
}
pub fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut out = serde_json::to_vec_pretty(value).map_err(io)?;
    out.push(b'\n');
    Ok(out)
}

const MOE: &str = "ornith-35b-a3b-local";
const MODEL: &str = "ornith-1.5-35b-a3b-apex-mtp-compact";
const LEAVES: &[&str] = &[
    "Work",
    "Pictures/sd/_uploads",
    ".local/state/local-ai/prefix-cache",
    ".local/share/ai-stack/releases/llama-cpu",
    ".local/share/ai-stack/releases/sd-cpu-repack",
    ".local/share/ai-stack/models/llm",
    ".local/share/ai-stack/models/images/vae",
];
fn document(path: &Path) -> Result<Value> {
    serde_json::from_slice(&fs::read(path).map_err(io)?).map_err(io)
}
fn project_root(raw: Option<&str>) -> Result<PathBuf> {
    let path = raw.map(PathBuf::from).unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("source parent")
            .join("lab")
    });
    if !path.join("dependencies/model-artifacts.json").is_file() {
        return Err(
            "public source checkout required; pass --project (wheel asset packaging pending)"
                .into(),
        );
    }
    fs::canonicalize(path).map_err(io)
}
fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn quote(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~/".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
fn asset_plan(project: &Path, image: &str) -> Result<Vec<Value>> {
    let images = match image {
        "flux2-klein" => [
            "flux-2-klein-4b-q8",
            "flux-2-klein-text-encoder",
            "flux-2-klein-vae",
        ],
        "z-image" => [
            "z-image-turbo-q8",
            "z-image-text-encoder",
            "z-image-vae-compatible-candidate",
        ],
        _ => return Err(format!("unknown image model: {image}")),
    };
    let selected = [MODEL, images[0], images[1], images[2]];
    let document = document(&project.join("dependencies/model-artifacts.json"))?;
    let mut out = Vec::new();
    let rows = document["models"]
        .as_array()
        .ok_or("invalid model manifest")?
        .iter()
        .chain(
            document["image_assets"]
                .as_array()
                .ok_or("invalid model manifest")?,
        );
    for row in rows {
        let id = row["id"].as_str().ok_or("invalid model identity")?;
        if !selected.contains(&id) {
            continue;
        }
        let fallback =
            json!([{"filename":row["file"],"bytes":row["bytes"],"sha256":row["sha256"]}]);
        let files = row
            .get("files")
            .filter(|v| v.as_array().is_some_and(|v| !v.is_empty()))
            .unwrap_or(&fallback)
            .as_array()
            .ok_or("invalid artifact list")?;
        if files.len() != 1 {
            return Err("selected default must be a single artifact".into());
        }
        let file = &files[0];
        let filename = file["filename"]
            .as_str()
            .ok_or("invalid artifact filename")?;
        relative(filename)?;
        let target = match id {
            MODEL => "models/llm/Ornith-1.5-35B-A3B-APEX-MTP-Compact.gguf",
            "flux-2-klein-4b-q8" => "models/images/flux2-klein/flux-2-klein-4b-Q8_0.gguf",
            "flux-2-klein-text-encoder" => "models/images/flux2-klein/Qwen3-4B-Q8_0.gguf",
            "flux-2-klein-vae" => "models/images/flux2-klein/flux2-vae.safetensors",
            "z-image-turbo-q8" => "models/images/z_image_turbo-Q8_0.gguf",
            "z-image-text-encoder" => "models/images/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
            "z-image-vae-compatible-candidate" => "models/images/vae/vae.safetensors",
            _ => return Err("selected artifact missing from manifest".into()),
        };
        let revision = row["revision"].as_str().unwrap_or_default();
        let digest = file["sha256"].as_str().unwrap_or_default();
        if !lower_hex(revision, 40)
            || !lower_hex(digest, 64)
            || !file["bytes"].as_u64().is_some_and(|n| n > 0)
        {
            return Err("immutable artifact identity required".into());
        }
        let prefix = format!(
            "https://huggingface.co/{}",
            row["repository"].as_str().ok_or("invalid repository")?
        );
        out.push(json!({"id":id,"url":format!("{prefix}/resolve/{revision}/{}",quote(filename)),"bytes":file["bytes"],"sha256":digest,"stack_relative_path":target,"license_card_url":format!("{prefix}/blob/{revision}/README.md"),"license_separate_from_wrapper":true}));
    }
    if out
        .iter()
        .filter_map(|v| v["id"].as_str())
        .collect::<BTreeSet<_>>()
        != selected.into_iter().collect()
    {
        return Err("selected artifact missing from manifest".into());
    }
    Ok(out)
}
fn plan(project: &Path, home: &Path, image: &str) -> Result<Value> {
    if !home.is_absolute()
        || home == Path::new("/")
        || home
            .components()
            .any(|p| p == std::path::Component::ParentDir)
        || home
            .as_os_str()
            .to_string_lossy()
            .contains(['\n', '\r', '\0'])
    {
        return Err("absolute non-root single-line target home required".into());
    }
    let patches = document(&project.join("patches/manifest.json"))?;
    let patches = patches["patches"]
        .as_array()
        .ok_or("invalid patch manifest")?;
    let mut engines = Vec::new();
    for name in ["llama-small-batch-pool.patch", "sd-cpu-repack.patch"] {
        let row = patches
            .iter()
            .find(|v| v["file"] == name)
            .ok_or("published patch identity missing")?;
        if json!(sha(&project.join("patches").join(name))?) != row["sha256"] {
            return Err("published patch identity mismatch".into());
        }
        engines.push(json!({"repository":row["upstream"],"commit":row["upstream_commit"],"patch":name,"patch_sha256":row["sha256"]}));
    }
    let assets = asset_plan(project, image)?;
    let size = assets
        .iter()
        .try_fold(0u64, |sum, v| {
            sum.checked_add(v["bytes"].as_u64().unwrap_or(u64::MAX))
        })
        .ok_or("invalid asset size")?;
    Ok(
        json!({"schema":1,"status":"prepared_not_installed_not_qualified","target_home":home,"assets":assets,"selected_download_bytes":size,"model_manifest_sha256":sha(&project.join("dependencies/model-artifacts.json"))?,"engines":engines,"default_model":MOE,"image_model":image,"images":"q8_cpu_lazy_private","npu_artifacts_required":false,"cpu_fallback":"sd-cpu-repack CPU path; NPU image path stays disabled until separately qualified","network_or_native_execution":false,"end_to_end_parity_passed":false,"remaining_gates":["guarded selected fetch/source build","fresh binary/model identity qualification","transactional host install","real panel installation","isolated-user full task/image/lifecycle verification","rollback/uninstall verification"]}),
    )
}
fn stage(
    destination: &Path,
    home: &Path,
    project: &Path,
    image: &str,
    oxa: Option<&str>,
) -> Result<Value> {
    let destination = if destination.is_absolute() {
        destination.to_owned()
    } else {
        std::env::current_dir().map_err(io)?.join(destination)
    };
    if fs::symlink_metadata(&destination).is_ok() {
        return Err("fresh destination required".into());
    }
    if destination.parent().is_some_and(|p| {
        p.ancestors()
            .any(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_symlink()))
    }) {
        return Err("symlinked destination parent refused".into());
    }
    let record = plan(project, home, image)?;
    let mut payload = Vec::<(String, Vec<u8>, u32)>::new();
    let mut put = |name: &str, data: Vec<u8>, mode: u32| -> Result<()> {
        relative(name)?;
        if payload.iter().any(|(n, _, _)| n == name) {
            return Err("duplicate payload path".into());
        }
        payload.push((name.into(), data, mode));
        Ok(())
    };
    let template = |path: &Path| -> Result<Value> {
        serde_json::from_str(
            &fs::read_to_string(path)
                .map_err(io)?
                .replace("@HOME@", &home.to_string_lossy()),
        )
        .map_err(io)
    };
    let mut config = template(&project.join("examples/runtime/llm-runtime.json"))?;
    let mut profile = config["profiles"]
        .get(MOE)
        .cloned()
        .filter(Value::is_object)
        .ok_or_else(|| format!("example runtime has no {MOE} profile"))?;
    profile["ready"] = json!(false);
    profile["server_sha256"] = json!("");
    config
        .as_object_mut()
        .ok_or("invalid runtime template")?
        .extend(profile.as_object().expect("profile").clone());
    let mut profiles = serde_json::Map::new();
    profiles.insert(MOE.to_owned(), profile);
    config["profiles"] = Value::Object(profiles);
    config["selected_model"] = json!(MOE);
    config["ready"] = json!(false);
    config["server_sha256"] = json!("");
    put(
        ".config/local-ai/llm/runtime.json",
        json_bytes(&config)?,
        0o600,
    )?;
    let mut image_files: Vec<_> = fs::read_dir(project.join("examples/runtime/images"))
        .map_err(io)?
        .filter_map(|v| v.ok())
        .map(|v| v.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    image_files.sort();
    for source in image_files {
        let name = source.file_name().unwrap_or_default().to_string_lossy();
        let mut value = template(&source)?;
        if name == "cpu-profile.json" {
            value["enabled"] = json!(false);
            value["validated"] = json!(false);
            value["sha256"] = json!("");
            value["models"] = json!(["q8"]);
        }
        if name == "gpu-profile.json" {
            value["enabled"] = json!(false);
            value["validated"] = json!(false);
        }
        put(
            &format!(".config/local-ai/images/{name}"),
            json_bytes(&value)?,
            0o600,
        )?;
    }
    put(
        ".local/bin/local-ai",
        fs::read(std::env::current_exe().map_err(io)?).map_err(io)?,
        0o700,
    )?;
    // The service working directory remains stable. No Python runtime files or
    // interpreter launcher are included in the native bundle.
    put(
        ".local/share/ai-stack/runtime/native-executor.json",
        json_bytes(
            &json!({"schema":1,"executor":"native","controller_entry":"serve","studio_entry":"studio"}),
        )?,
        0o600,
    )?;
    if let Some(path) = oxa {
        let data = fs::read(path).map_err(io)?;
        if !data.starts_with(b"\x7fELF") {
            return Err("--oxa-binary must be a built ELF executable".into());
        }
        put(".local/bin/oxa", data, 0o700)?;
    }
    let mut units: Vec<_> = fs::read_dir(project.join("examples/runtime/units"))
        .map_err(io)?
        .filter_map(|v| v.ok())
        .map(|v| v.path())
        .filter(|p| p.extension().is_some_and(|e| e == "service"))
        .collect();
    units.sort();
    for source in units {
        let name = source.file_name().unwrap_or_default().to_string_lossy();
        let text = fs::read_to_string(&source).map_err(io)?;
        let text = text
            .replace(
                "ExecStart=/usr/bin/python3 %h/.local/share/ai-stack/runtime/controller.py",
                "ExecStart=%h/.local/bin/local-ai serve",
            )
            .replace(
                "ExecStart=/usr/bin/python3 %h/.local/share/ai-stack/runtime/studio",
                "ExecStart=%h/.local/bin/local-ai studio",
            );
        put(
            &format!(".config/systemd/user/{name}"),
            text.into_bytes(),
            0o600,
        )?;
    }
    fn source_files(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(path).map_err(io)? {
            let entry = entry.map_err(io)?;
            if entry.path().is_file() {
                out.push(entry.path());
            } else if entry.file_type().map_err(io)?.is_dir() {
                source_files(&entry.path(), out)?;
            }
        }
        Ok(())
    }
    let plugin = project.join("src/snapdragon_ai_lab/omarchy_plugin");
    let mut plugin_files = Vec::new();
    source_files(&plugin, &mut plugin_files)?;
    plugin_files.sort();
    for file in plugin_files {
        put(
            &format!(
                ".config/omarchy/plugins/snapdragon.local-ai/{}",
                file.strip_prefix(&plugin).map_err(io)?.to_string_lossy()
            ),
            fs::read(file).map_err(io)?,
            0o600,
        )?;
    }
    put(
        ".config/local-ai/npu-status.json",
        json_bytes(&json!({"ready":false,"state":"disabled","bootstrap_qualified":false}))?,
        0o600,
    )?;
    put(".local/state/local-ai/inference.lock", Vec::new(), 0o600)?;
    for name in ["LICENSE", "NOTICE"] {
        put(
            &format!(".local/share/ai-stack/notices/wrapper/{name}"),
            fs::read(project.join(name)).map_err(io)?,
            0o600,
        )?;
    }
    if let Some(parent) = destination.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(io)?;
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&destination)
        .map_err(io)?;
    let payload_root = destination.join("payload");
    let mut inventory = serde_json::Map::new();
    for (name, data, mode) in payload {
        let path = payload_root.join(relative(&name)?);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path.parent().ok_or("payload parent missing")?)
            .map_err(io)?;
        fs::write(&path, &data).map_err(io)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).map_err(io)?;
        inventory.insert(
            name,
            json!({"sha256":format!("{:x}",Sha256::digest(&data)),"bytes":data.len(),"mode":mode}),
        );
    }
    for leaf in LEAVES {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(payload_root.join(relative(leaf)?))
            .map_err(io)?;
    }
    let (_, directories) = self::inventory(&payload_root)?;
    let plan_path = destination.join("bootstrap-plan.json");
    fs::write(&plan_path, json_bytes(&record)?).map_err(io)?;
    fs::set_permissions(&plan_path, fs::Permissions::from_mode(0o600)).map_err(io)?;
    let manifest = json!({"schema":1,"files":inventory,"directories":directories,"plan_sha256":sha(&plan_path)?,"installed":false,"end_to_end_parity_passed":false,"executor":"native"});
    let manifest_path = destination.join("stage-manifest.json");
    fs::write(&manifest_path, json_bytes(&manifest)?).map_err(io)?;
    fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600)).map_err(io)?;
    verify_stage(&destination)
}
pub fn cli(words: &[String]) -> i32 {
    if words
        .iter()
        .take_while(|w| w.as_str() != "--")
        .any(|w| matches!(w.as_str(), "--help" | "-h"))
    {
        let command = words
            .iter()
            .map(String::as_str)
            .find(|s| ["plan", "stage", "verify-stage"].contains(s));
        print!(
            "{}",
            match command {
                Some("plan") => include_str!("bootstrap-plan-help.txt"),
                Some("stage") => include_str!("bootstrap-stage-help.txt"),
                Some("verify-stage") => include_str!("bootstrap-verify-stage-help.txt"),
                _ => include_str!("bootstrap-help.txt"),
            }
        );
        return 0;
    }
    let result = (|| {
        let mut project = None;
        let mut image = "flux2-klein";
        let mut home = None;
        let mut oxa = None;
        let mut operands = Vec::new();
        let mut i = 0;
        while i < words.len() {
            let (word, inline) = words[i]
                .split_once('=')
                .map_or((words[i].as_str(), None), |(k, v)| (k, Some(v)));
            match word {
                "--project"|"--image-model"|"--target-home"|"--oxa-binary"=>{
                    let value=if let Some(value)=inline{value}else{i+=1;words.get(i).ok_or("option requires a value")?.as_str()};
                    match word{"--project"=>project=Some(value),"--image-model"=>image=value,"--target-home"=>home=Some(value),_=>oxa=Some(value)}
                },
                "--include-dense"=>return Err("--include-dense was removed: the dense Qwen3.8 27B model was retired in v0.1.2".into()),
                _ if word.starts_with('-')=>return Err("unknown bootstrap option".into()),
                _=>operands.push(words[i].as_str()),
            }
            i += 1;
        }
        let action = operands.first().copied().ok_or("action required")?;
        match action {
            "verify-stage" => {
                verify_stage(Path::new(operands.get(1).ok_or("destination required")?))
            }
            "plan" => plan(
                &project_root(project)?,
                Path::new(home.ok_or("target home required")?),
                image,
            ),
            "stage" => stage(
                Path::new(operands.get(1).ok_or("destination required")?),
                Path::new(home.ok_or("target home required")?),
                &project_root(project)?,
                image,
                oxa,
            ),
            _ => Err("unknown bootstrap action".into()),
        }
    })();
    match result {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).expect("bootstrap result")
            );
            0
        }
        Err(error) => {
            let code = if error.starts_with("--include-dense") {
                2
            } else {
                1
            };
            eprintln!("bootstrap: {error}");
            code
        }
    }
}
