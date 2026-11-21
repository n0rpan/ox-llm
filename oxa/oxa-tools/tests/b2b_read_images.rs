//! B2b: `read` returns an image as an image part when the model takes images, within the owner's
//! `oxa.read.maxImageBytes`. Any other model gets the placeholder line, and a private or swapped
//! path is refused before a byte is read. Isolation: every file lives under `oxa_fake::TestEnv`.
use base64::Engine as _;
use oxa_fake::TestEnv;
use oxa_proto::{
    CancelToken, ExecResult, ExecSpec, Mode, OxaError, Sandbox, Tool, ToolCtx, ToolOutput, ToolTune,
};
use oxa_tools::ReadTool;
use serde_json::json;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

/// A sandbox that is never asked to run anything: these tests only use the read tool.
struct NoSandbox;

impl Sandbox for NoSandbox {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult {
            output: Vec::new(),
            exit: None,
            timed_out: false,
            unsandboxed: false,
        }
    }
}

/// A PNG header of `w` x `h` followed by `extra` zero bytes (the header is all `read` looks at).
fn png(w: u32, h: u32, extra: usize) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0, 0, 0, 13]);
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&w.to_be_bytes());
    png.extend_from_slice(&h.to_be_bytes());
    png.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
    png.resize(png.len() + extra, 0);
    png
}

/// Run `read` on `path` in the test's work directory with `tune`, and `pins` as the permission
/// check's pinned locations.
fn run(env: &TestEnv, path: &str, tune: ToolTune, pins: &[(PathBuf, PathBuf)]) -> ToolOutput {
    let cwd = env.work();
    let spill = env.tmp();
    let cancel = CancelToken::new();
    let cx = ToolCtx {
        checked: pins,
        cwd: &cwd,
        spill_dir: &spill,
        sandbox: &NoSandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune,
        hidden: None,
    };
    ReadTool::new().run(&json!({ "path": path }), &cx, &mut |_: &str| {})
}

fn image_tune(max_image_bytes: u32) -> ToolTune {
    ToolTune {
        image_input: true,
        max_image_bytes,
        ..Default::default()
    }
}

#[test]
fn read_png_returns_an_image_part() {
    let env = TestEnv::new();
    let bytes = png(3, 2, 0);
    fs::write(env.work().join("pic.png"), &bytes).expect("write png");
    let out = run(&env, "pic.png", image_tune(0), &[]);
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, "[image pic.png 3x2: shown to the model]");
    assert_eq!(out.images.len(), 1);
    assert_eq!(out.images[0].mime, "image/png");
    let sent = base64::engine::general_purpose::STANDARD
        .decode(&out.images[0].base64)
        .expect("base64");
    assert_eq!(sent, bytes, "the image part carries the file bytes");
    // The finish decision's marker (`oxa.done.imageCrossCheck`); display-only.
    assert_eq!(out.ui["imageRead"], true);
}

#[test]
fn read_webp_is_attached_as_webp() {
    let env = TestEnv::new();
    // RIFF header, then an extended (VP8X) chunk: width 3 and height 2, as width-1 and height-1.
    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&[0, 0, 0, 0]);
    webp.extend_from_slice(b"WEBPVP8X");
    webp.extend_from_slice(&[10, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 1, 0, 0]);
    fs::write(env.work().join("pic.webp"), &webp).expect("write webp");
    let out = run(&env, "pic.webp", image_tune(0), &[]);
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, "[image pic.webp 3x2: shown to the model]");
    assert_eq!(out.images.len(), 1);
    assert_eq!(out.images[0].mime, "image/webp");
}

#[test]
fn read_image_without_model_support_returns_the_placeholder() {
    let env = TestEnv::new();
    fs::write(env.work().join("pic.png"), png(3, 2, 0)).expect("write png");
    let out = run(&env, "pic.png", ToolTune::default(), &[]);
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, oxa_proto::image_placeholder("pic.png", 3, 2));
    assert!(out.images.is_empty());
    // A placeholder read still counts as consulting the image for the finish decision.
    assert_eq!(out.ui["imageRead"], true);
}

#[test]
fn oversized_image_warns_with_the_limit() {
    let env = TestEnv::new();
    let bytes = png(3, 2, 100);
    let size = bytes.len();
    fs::write(env.work().join("pic.png"), &bytes).expect("write png");
    let out = run(&env, "pic.png", image_tune(64), &[]);
    assert!(!out.is_error, "{}", out.text);
    assert!(out.images.is_empty(), "an image over the limit is not sent");
    assert_eq!(
        out.text,
        format!(
            "[image pic.png 3x2: not sent, {size} bytes is over the limit of 64 (oxa.read.maxImageBytes)]"
        )
    );
    // 0 is no limit: the same file is sent.
    let out = run(&env, "pic.png", image_tune(0), &[]);
    assert_eq!(out.images.len(), 1);
}

#[test]
fn private_path_image_is_still_denied() {
    let env = TestEnv::new();
    let work = env.work();
    fs::write(work.join("real.png"), png(3, 2, 0)).expect("write png");
    let approved = fs::canonicalize(work.join("real.png")).expect("real path");
    // `link.png` is a symlink to a file outside the work directory; the check pinned `real.png`.
    let secret = env.root().join("outside.png");
    fs::write(&secret, png(9, 9, 0)).expect("write outside png");
    symlink(&secret, work.join("link.png")).expect("symlink");
    let pins = vec![(work.join("link.png"), approved)];
    let out = run(&env, "link.png", image_tune(0), &pins);
    assert!(out.is_error, "{}", out.text);
    assert!(out.images.is_empty());
    assert!(
        out.text.contains("changed after the permission check"),
        "{}",
        out.text
    );
    assert!(!out.text.contains("9x9"), "{}", out.text);
}
