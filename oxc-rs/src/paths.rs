use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| {
            nix::unistd::User::from_uid(nix::unistd::Uid::current())
                .ok()
                .flatten()
                .map(|user| user.dir)
        })
        .expect("Could not determine home directory.")
}

pub fn xdg(name: &str, fallback: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

pub fn config() -> PathBuf {
    std::env::var_os("LOCAL_LLM_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("local-ai/llm/runtime.json"))
}

pub fn read_config() -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(config()).ok()?).ok()?;
    value.is_object().then_some(value)
}

pub fn python_json(value: &serde_json::Value) -> String {
    use serde_json::Value;
    match value {
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!(
                    "{}: {}",
                    python_json(&Value::String(key.clone())),
                    python_json(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::String(_) => {
            let raw = value.to_string();
            let mut result = String::new();
            for ch in raw.chars() {
                if ch.is_ascii() {
                    result.push(ch);
                } else {
                    let mut units = [0_u16; 2];
                    for unit in ch.encode_utf16(&mut units) {
                        result.push_str(&format!("\\u{unit:04x}"));
                    }
                }
            }
            result
        }
        _ => value.to_string(),
    }
}

/// Python's default `:g` presentation (six significant digits).
pub fn general_number(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    let scientific = format!("{value:.5e}");
    let (mantissa, exponent) = scientific.split_once('e').expect("finite numeric exponent");
    let exponent: i32 = exponent.parse().expect("numeric exponent");
    if (-4..6).contains(&exponent) {
        let decimals = (5 - exponent).max(0) as usize;
        let formatted = format!("{value:.decimals$}");
        if decimals == 0 {
            formatted
        } else {
            formatted
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned()
        }
    } else {
        format!(
            "{}e{exponent:+03}",
            mantissa.trim_end_matches('0').trim_end_matches('.')
        )
    }
}
