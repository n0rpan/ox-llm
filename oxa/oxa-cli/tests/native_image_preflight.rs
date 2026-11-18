//! Native image preflight using synthetic files in an isolated user directory.
#[path = "../src/image_request.rs"]
mod image_request;

use oxa_config::{dirs_from_env, Env};
use oxa_fake::TestEnv;
use oxa_proto::{codes, OxaError};
use serde_json::json;
use std::path::Path;

fn run(
    env: &TestEnv,
    args: &[&str],
) -> Result<(image_request::ImageRequest, image_request::ImageWeights), OxaError> {
    let agent_env = env.envs("http://127.0.0.1:9");
    let pairs: Vec<_> = agent_env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let dirs = dirs_from_env(&Env::from_pairs(&pairs), None).unwrap();
    let args: Vec<_> = args.iter().map(|arg| (*arg).to_owned()).collect();
    image_request::preflight(&args, &dirs, &env.work())
}

fn refusal(env: &TestEnv, args: &[&str]) -> OxaError {
    match run(env, args) {
        Err(error) => error,
        Ok(_) => panic!("image preflight unexpectedly succeeded"),
    }
}

#[test]
fn typed_arguments_preserve_request_and_owner_selected_weights() {
    let env = TestEnv::new();
    let diffusion = env.write("weights/synthetic-dit.gguf", "synthetic");
    let encoder = env.write("weights/synthetic-te.gguf", "synthetic");
    let vae = env.write("weights/synthetic-vae.safetensors", "synthetic");
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({"oxa":{"image":{"enabled":true,"diffusion":diffusion,"textEncoder":encoder,"vae":vae}}})
            .to_string(),
    );
    env.write(
        "work/.oxa/settings.json",
        &json!({"oxa":{"image":{"diffusion":"untrusted"}}}).to_string(),
    );
    let (request, weights) = run(
        &env,
        &[
            "Synthetic image prompt",
            "--out",
            "result.png",
            "--size",
            "640x512",
            "--seed",
            "0",
            "--steps",
            "7",
        ],
    )
    .unwrap();
    assert_eq!(request.prompt, "Synthetic image prompt");
    assert_eq!(request.output, Path::new("result.png"));
    assert_eq!(request.size, (640, 512));
    assert_eq!(request.seed, Some(0));
    assert_eq!(request.steps, 7);
    assert_eq!(weights.diffusion, diffusion);
    assert_eq!(weights.text_encoder, encoder);
    assert_eq!(weights.vae, vae);
    assert!(!env.work().join("result.png").exists());

    let (defaults, _) = run(&env, &["Synthetic image prompt"]).unwrap();
    assert_eq!(defaults.output, Path::new("image.png"));
    assert_eq!(defaults.size, (512, 512));
    assert_eq!(defaults.seed, None);
    assert_eq!(defaults.steps, 4);

    let (limits, _) = run(&env, &["Synthetic", "--size", "256x1024", "--steps", "8"]).unwrap();
    assert_eq!(limits.size, (256, 1024));
    assert_eq!(limits.steps, 8);
}

#[test]
fn malformed_arguments_fail_before_settings_or_model_checks() {
    let env = TestEnv::new();
    for args in [
        &[][..],
        &[" "],
        &["Synthetic", "--size", "511x512"],
        &["Synthetic", "--size", "0x512"],
        &["Synthetic", "--size", "240x512"],
        &["Synthetic", "--size", "512x1040"],
        &["Synthetic", "--size", "4294967280x4294967280"],
        &["Synthetic", "--size", "512x"],
        &["Synthetic", "--steps", "0"],
        &["Synthetic", "--steps", "9"],
        &["Synthetic", "--steps", "4294967295"],
        &["Synthetic", "--seed", "-1"],
        &["Synthetic", "--seed", "18446744073709551616"],
        &["Synthetic", "--out", "--steps"],
        &["Synthetic", "--out", "image.jpg"],
        &["Synthetic", "--steps", "4", "--steps", "5"],
        &["Synthetic", "unquoted"],
        &["Synthetic", "--binary", "unknown"],
    ] {
        assert_eq!(refusal(&env, args).code, codes::USAGE, "{args:?}");
    }
}

#[test]
fn project_opt_in_is_ignored_and_missing_weights_point_to_downloads() {
    let env = TestEnv::new();
    env.write("work/.oxa/settings.json", r#"{"oxa":{"image":{"enabled":true}}}"#);
    let args = ["Synthetic image prompt"];
    let error = refusal(&env, &args);
    assert_eq!(error.code, codes::INVALID);
    assert!(error.to_string().contains("oxa.image.enabled"));

    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"image":{"enabled":true,"diffusion":"relative.gguf"}}}"#,
    );
    let error = refusal(&env, &args);
    assert_eq!(error.code, codes::NOT_FOUND);
    assert!(error.to_string().contains("missing"));
    assert!(error
        .to_string()
        .contains("oxc/docs/DEPENDENCIES.md#image-assets"));
    assert!(!error.to_string().contains("relative.gguf"));
}
