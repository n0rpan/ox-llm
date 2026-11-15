//! Work in progress: request validation preceding the real Studio engine plans.
use super::Studio;
use serde_json::Value;

pub(super) enum PlanError {
    Reply(u16, String),
    Close,
}
impl PlanError {
    fn bad(message: impl Into<String>) -> Self {
        Self::Reply(400, message.into())
    }
    fn missing(message: impl Into<String>) -> Self {
        Self::Reply(200, message.into())
    }
}
pub(super) struct Generation {
    pub prompt: String,
    pub model: String,
    pub width: i64,
    pub height: i64,
    pub steps: i64,
    pub count: i64,
    pub seed: Option<i64>,
    pub cpu: bool,
    pub klein: bool,
}
fn trim(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f'))
}
fn truthy(value: &Value) -> bool {
    crate::release_files::truthy(value)
}
pub(super) fn integer(value: &Value) -> Option<i64> {
    match value {
        Value::Bool(v) => Some(i64::from(*v)),
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|n| n.is_finite() && *n >= i64::MIN as f64 && *n < i64::MAX as f64)
                .map(|n| n.trunc() as i64)
        }),
        Value::String(s) => super::metadata::python_int(s),
        _ => None,
    }
}
fn exact_integer(value: &Value) -> Option<i64> {
    value.as_i64()
}
fn seed(value: &Value) -> Result<Option<i64>, PlanError> {
    if value.is_null() {
        return Ok(None);
    }
    let number = if let Some(s) = value.as_str() {
        super::metadata::python_float(s)
    } else {
        value.as_f64()
    };
    let parsed = if value.is_boolean() {
        None
    } else {
        integer(value)
    };
    let valid = parsed.zip(number).filter(|(n, f)| {
        *n >= 0
            && *n <= 1i64 << 62
            && f.is_finite()
            && *f >= 0.0
            && *f <= ((1i64 << 62) as f64)
            && (*f as i64) == *n
            && *f == f.trunc()
    });
    valid
        .map(|(n, _)| Some(n))
        .ok_or_else(|| PlanError::bad("Seed must be a whole number of 0 or more"))
}
impl Studio {
    pub(super) fn generation_input(&self, body: &Value) -> Result<Generation, PlanError> {
        let prompt = body.get("prompt").unwrap_or(&Value::Null);
        if !truthy(prompt) {
            return Err(PlanError::missing("empty prompt"));
        }
        let prompt = trim(prompt.as_str().ok_or(PlanError::Close)?).to_owned();
        if prompt.is_empty() {
            return Err(PlanError::missing("empty prompt"));
        }
        let model = body
            .get("model")
            .cloned()
            .unwrap_or_else(|| Value::String("q4".into()));
        if model.is_object() || model.is_array() {
            return Err(PlanError::Close);
        }
        let model = model.as_str().unwrap_or_default().to_owned();
        let klein = ["klein-q4", "klein-q8"].contains(&model.as_str());
        if !klein && !["q4", "q8"].contains(&model.as_str()) {
            return Err(PlanError::bad("Choose Q4 or Q8, or FLUX.2 klein"));
        }
        let width = body.get("width").cloned().unwrap_or(Value::from(512));
        let height = body.get("height").cloned().unwrap_or(Value::from(512));
        let steps = body.get("steps").cloned().unwrap_or(Value::from(8));
        let (width, height, steps) = if klein {
            let dims = exact_integer(&width)
                .zip(exact_integer(&height))
                .filter(|(w, h)| {
                    [*w, *h]
                        .iter()
                        .all(|n| (256..=1024).contains(n) && *n % 16 == 0)
                });
            let (w, h) = dims.ok_or_else(|| {
                PlanError::bad("FLUX.2 klein sizes are multiples of 16 from 256 to 1024")
            })?;
            (w, h, 4)
        } else {
            let dims = integer(&width)
                .zip(integer(&height))
                .zip(integer(&steps))
                .filter(|((w, h), s)| {
                    (256..=1536).contains(w)
                        && (256..=1536).contains(h)
                        && w % 64 == 0
                        && h % 64 == 0
                        && w * h <= 1536 * 1024
                        && (1..=50).contains(s)
                });
            let ((w,h),s)=dims.ok_or_else(||PlanError::bad("Use dimensions 256–1536 in multiples of 64, at most 1536×1024 pixels, and 1–50 steps"))?;
            (w, h, s)
        };
        let count = body.get("variations").cloned().unwrap_or(Value::from(1));
        let count = exact_integer(&count)
            .filter(|n| (1..=4).contains(n))
            .ok_or_else(|| PlanError::bad("Variations must be 1–4"))?;
        let seed = seed(&body["seed"])?;
        let cpu = body.get("cpu").is_none_or(truthy);
        if klein {
            if truthy(&body["init"]) {
                return Err(PlanError::bad("FLUX.2 klein text-to-image does not take a photo; use the Edit panel for photos"));
            }
            if !cpu {
                return Err(PlanError::bad(
                    "FLUX.2 klein runs on CPU only; keep CPU selected",
                ));
            }
            let missing = self.klein_missing(if model == "klein-q4" { "q4" } else { "q8" });
            if !missing.is_empty() {
                return Err(PlanError::missing(format!(
                    "Missing klein model files: {} (see docs/STUDIO-EDIT.md)",
                    missing.join(", ")
                )));
            }
        } else {
            for path in [
                self.image_models().join(if model == "q4" {
                    "z_image_turbo-Q4_K.gguf"
                } else {
                    "z_image_turbo-Q8_0.gguf"
                }),
                self.image_models()
                    .join("Qwen3-4B-Instruct-2507-Q4_K_M.gguf"),
                self.image_models().join("vae/vae.safetensors"),
            ] {
                if !path.exists() {
                    return Err(PlanError::missing(format!(
                        "missing model {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    )));
                }
            }
        }
        Ok(Generation {
            prompt,
            model,
            width,
            height,
            steps,
            count,
            seed,
            cpu,
            klein,
        })
    }
    pub(super) fn edit_input(&self, body: &Value) -> Result<(), PlanError> {
        let instruction = body["instruction"].as_str().map(trim).unwrap_or_default();
        if instruction.is_empty() {
            return Err(PlanError::missing("Type what to change first"));
        }
        if instruction.chars().count() > 1000 {
            return Err(PlanError::bad("Instruction is longer than 1000 characters"));
        }
        if body.get("crop").is_some_and(|v| !v.is_boolean()) {
            return Err(PlanError::bad("crop must be true or false"));
        }
        let size = body.get("size").cloned().unwrap_or(Value::from(512));
        if !exact_integer(&size).is_some_and(|v| [512, 768].contains(&v)) {
            return Err(PlanError::bad("Edit size must be 512 or 768"));
        }
        let model = body
            .get("model")
            .cloned()
            .unwrap_or_else(|| Value::String("q8".into()));
        if model.is_object() || model.is_array() {
            return Err(PlanError::Close);
        }
        let model = model.as_str().unwrap_or_default();
        if !["q4", "q8"].contains(&model) {
            return Err(PlanError::bad("Choose Q8 or Q4"));
        }
        let steps = body.get("steps").cloned().unwrap_or(Value::from(4));
        if !exact_integer(&steps).is_some_and(|n| (1..=8).contains(&n)) {
            return Err(PlanError::bad(
                "Edit steps must be 1-8 (the distilled model is made for 4)",
            ));
        }
        seed(&body["seed"])?;
        let missing = self.klein_missing(model);
        if !missing.is_empty() {
            return Err(PlanError::missing(format!(
                "Missing edit model files: {} (see docs/STUDIO-EDIT.md)",
                missing.join(", ")
            )));
        }
        Ok(())
    }
}
