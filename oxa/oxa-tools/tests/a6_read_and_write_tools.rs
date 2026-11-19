//! A6 acceptance tests: read and write tools.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_proto::{
    Access, CancelToken, ExecResult, ExecSpec, Mode, OxaError, Sandbox, Tool, ToolCtx, ToolOutput, Value,
};
use oxa_tools::descriptions::errors;
use oxa_tools::{ReadTool, WriteTool};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// A sandbox that is never asked to run anything: these tests only use file tools.
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

/// Run a tool with `args` in the test's work directory.
fn run_tool(tool: &dyn Tool, env: &TestEnv, args: Value) -> ToolOutput {
    let cwd = env.work();
    let spill = env.tmp();
    let cancel = CancelToken::new();
    let sandbox = NoSandbox;
    let cx = ToolCtx {
        checked: &[],
        cwd: &cwd,
        spill_dir: &spill,
        sandbox: &sandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
        hidden: None,
    };
    tool.run(&args, &cx, &mut |_: &str| {})
}

/// Lines `line 1` .. `line n`, each with a newline.
fn numbered_lines(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

/// pi's continuation notices
#[test]
fn read_paging_offset_limit() {
    let env = TestEnv::new();
    env.write("work/f.txt", &numbered_lines(10));

    // The whole file: the trailing newline ends the text, no notice.
    let out = run_tool(&ReadTool::new(), &env, json!({"path": "f.txt"}));
    assert!(!out.is_error);
    assert_eq!(out.text, numbered_lines(10));

    // A limit stops early: pi counts the empty line after the final newline in the total (11).
    let out = run_tool(&ReadTool::new(), &env, json!({"path": "f.txt", "limit": 3}));
    assert!(!out.is_error);
    assert_eq!(
        out.text,
        "line 1\nline 2\nline 3\n\n[8 more lines in file. Use offset=4 to continue.]"
    );

    // Offset is 1-based; the continuation offset works.
    let out = run_tool(
        &ReadTool::new(),
        &env,
        json!({"path": "f.txt", "offset": 4, "limit": 2}),
    );
    assert_eq!(
        out.text,
        "line 4\nline 5\n\n[6 more lines in file. Use offset=6 to continue.]"
    );
    let out = run_tool(&ReadTool::new(), &env, json!({"path": "f.txt", "offset": 6}));
    assert_eq!(out.text, "line 6\nline 7\nline 8\nline 9\nline 10\n");
}

/// Lines cut by pi's 2000-line limit, and by the byte cap (16 KB by default).
#[test]
fn read_continuation_by_lines_and_bytes() {
    let env = TestEnv::new();
    // 2500 short lines: 2000 lines are shown, the total is 2501 (pi counts the final empty line).
    let many: String = (0..2500).map(|i| format!("{i:04}\n")).collect();
    env.write("work/many.txt", &many);
    let out = run_tool(&ReadTool::new(), &env, json!({"path": "many.txt"}));
    assert!(!out.is_error);
    assert!(
        out.text
            .ends_with("\n\n[Showing lines 1-2000 of 2501. Use offset=2001 to continue.]"),
        "{}",
        out.text
    );
    let next = run_tool(
        &ReadTool::new(),
        &env,
        json!({"path": "many.txt", "offset": 2001}),
    );
    assert!(next.text.starts_with("2000\n2001\n"), "{}", next.text);

    // 300 lines of 100 bytes: the byte cap stops at 162 lines (100 + 161 * 101 = 16361 bytes).
    let wide: String = (0..300).map(|i| format!("{i:03}{}\n", "x".repeat(97))).collect();
    env.write("work/wide.txt", &wide);
    let out = run_tool(&ReadTool::new(), &env, json!({"path": "wide.txt"}));
    let notice = errors::read_more_by_bytes(1, 162, 301, 16 * 1024, 163);
    assert!(out.text.ends_with(&format!("\n\n{notice}")), "{}", out.text);
    assert_eq!(
        out.text.lines().count(),
        162 + 2,
        "162 lines, a blank line, the notice"
    );
}

/// A single line over the byte cap is not shown: pi's notice with the bash fallback.
#[test]
fn read_first_line_over_cap_gives_notice() {
    let env = TestEnv::new();
    env.write("work/big.txt", &format!("{}\nshort\n", "x".repeat(20_000)));
    let out = run_tool(&ReadTool::new(), &env, json!({"path": "big.txt"}));
    assert!(!out.is_error);
    assert_eq!(
        out.text,
        errors::read_line_too_long(1, 20_000, 16 * 1024, "big.txt")
    );
}

/// a 200 MB sparse file reads its first page without loading it
#[test]
fn read_streams_large_file() {
    let env = TestEnv::new();
    // 1000 text lines (more than the 8 KiB binary probe, so the probe sees text only), then a
    // 200 MB hole with no newline in it.
    let path = env.write("work/sparse.txt", &numbered_lines(1000));
    let f = fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open sparse file");
    f.set_len(200 * 1024 * 1024).expect("extend sparse file");
    drop(f);

    let out = run_tool(&ReadTool::new(), &env, json!({"path": "sparse.txt", "limit": 5}));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        out.text,
        "line 1\nline 2\nline 3\nline 4\nline 5\n\n[996 more lines in file. Use offset=6 to continue.]"
    );

    // The last line is the 200 MB hole: it is counted, never shown, and the cap cuts before it.
    let out = run_tool(
        &ReadTool::new(),
        &env,
        json!({"path": "sparse.txt", "offset": 1000}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        out.text,
        "line 1000\n\n[Showing lines 1000-1000 of 1001 (16.0KB limit). Use offset=1001 to continue.]"
    );
    assert!(!out.text.contains('\0'));
}

/// fixed error text
#[test]
fn read_binary_refused() {
    let env = TestEnv::new();
    let mut bytes = b"\x7fELF".to_vec();
    bytes.extend_from_slice(&[0u8; 64]);
    fs::write(env.work().join("bin.dat"), &bytes).expect("write binary");
    let out = run_tool(&ReadTool::new(), &env, json!({"path": "bin.dat"}));
    assert!(out.is_error);
    assert_eq!(out.text, errors::read_binary_refused("bin.dat"));
    assert_eq!(out.text, "Cannot read binary file: bin.dat");
}

/// exactly image_placeholder(name, w, h)
#[test]
fn read_image_returns_placeholder_line() {
    let env = TestEnv::new();
    // PNG: signature, IHDR length, "IHDR", width 3, height 2, then depth/color/compression/filter/interlace and a CRC.
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0, 0, 0, 13]);
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&3u32.to_be_bytes());
    png.extend_from_slice(&2u32.to_be_bytes());
    png.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
    fs::write(env.work().join("pic.png"), &png).expect("write png");
    let out = run_tool(&ReadTool::new(), &env, json!({"path": "pic.png"}));
    assert!(!out.is_error);
    assert_eq!(out.text, oxa_proto::image_placeholder("pic.png", 3, 2));
    assert_eq!(out.text, "[image pic.png 3x2: not visible to this model]");

    // GIF: signature, width and height little-endian.
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&[10, 0, 5, 0, 0, 0, 0]);
    fs::write(env.work().join("anim.gif"), &gif).expect("write gif");
    let out = run_tool(&ReadTool::new(), &env, json!({"path": "anim.gif"}));
    assert_eq!(out.text, oxa_proto::image_placeholder("anim.gif", 10, 5));
}

/// pi's text
#[test]
fn read_offset_beyond_end_error() {
    let env = TestEnv::new();
    env.write("work/three.txt", "a\nb\nc\n");
    let out = run_tool(&ReadTool::new(), &env, json!({"path": "three.txt", "offset": 10}));
    assert!(out.is_error);
    assert_eq!(out.text, "Offset 10 is beyond end of file (4 lines total)");
    assert_eq!(out.text, errors::read_offset_beyond_end(10, 4));
}

/// temp + rename; existing mode preserved
#[test]
fn write_is_atomic_and_keeps_mode() {
    let env = TestEnv::new();
    let path = env.write("work/keep.txt", "old\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("chmod");

    let out = run_tool(
        &WriteTool::new(),
        &env,
        json!({"path": "keep.txt", "content": "new content\n"}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, "Successfully wrote to keep.txt");
    assert_eq!(out.ui, json!({"bytes": 12, "created": false}));
    assert_eq!(fs::read_to_string(&path).expect("read back"), "new content\n");
    let mode = fs::metadata(&path).expect("meta").permissions().mode() & 0o7777;
    assert_eq!(mode, 0o640, "existing mode is kept");

    // No temporary file is left behind.
    let leftovers: Vec<_> = fs::read_dir(env.work())
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("oxa-tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// nested directories
#[test]
fn write_creates_parents() {
    let env = TestEnv::new();
    let out = run_tool(
        &WriteTool::new(),
        &env,
        json!({"path": "a/b/c.txt", "content": "deep\n"}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.ui, json!({"bytes": 5, "created": true}));
    assert_eq!(
        fs::read_to_string(env.work().join("a/b/c.txt")).expect("read back"),
        "deep\n"
    );
}

/// `..` resolved lexically
#[test]
fn access_reports_normalised_paths() {
    let env = TestEnv::new();
    let cwd = env.work();
    let spill = env.tmp();
    let cancel = CancelToken::new();
    let sandbox = NoSandbox;
    let cx = ToolCtx {
        checked: &[],
        cwd: &cwd,
        spill_dir: &spill,
        sandbox: &sandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
        hidden: None,
    };

    let read = ReadTool::new()
        .access(&json!({"path": "sub/../x/./f.txt"}), &cx)
        .expect("access");
    assert_eq!(read, vec![Access::Read(env.work().join("x/f.txt"))]);

    // `..` past the work directory goes up lexically: root/work/a/../../b is root/b.
    let read = ReadTool::new()
        .access(&json!({"path": "a/../../b"}), &cx)
        .expect("access");
    assert_eq!(read, vec![Access::Read(env.root().join("b"))]);

    let write = WriteTool::new()
        .access(&json!({"path": "/x/y/../z.txt", "content": ""}), &cx)
        .expect("access");
    assert_eq!(write, vec![Access::Write(Path::new("/x/z.txt").to_path_buf())]);

    assert!(
        ReadTool::new().access(&json!({"offset": 1}), &cx).is_err(),
        "path is required"
    );
}

/// A missing file reports pi's text, with the path as the model wrote it.
#[test]
fn read_missing_file_reports_path_as_written() {
    let env = TestEnv::new();
    let out = run_tool(&ReadTool::new(), &env, json!({"path": "./nope.txt"}));
    assert!(out.is_error);
    assert_eq!(out.text, "Path not found: ./nope.txt");
}

/// The file is never written partially: a write to a read-only directory fails cleanly.
#[test]
fn write_error_leaves_no_temp_file() {
    let env = TestEnv::new();
    fs::create_dir_all(env.work().join("ro")).expect("mkdir");
    fs::set_permissions(env.work().join("ro"), fs::Permissions::from_mode(0o500)).expect("chmod");
    let out = run_tool(
        &WriteTool::new(),
        &env,
        json!({"path": "ro/new.txt", "content": "x"}),
    );
    // Restore so the test directory can be removed.
    fs::set_permissions(env.work().join("ro"), fs::Permissions::from_mode(0o700)).expect("chmod back");
    // Running as root the write may succeed; then there is nothing to check beyond the content.
    if out.is_error {
        assert!(
            out.text.starts_with("Could not write file: ro/new.txt. "),
            "{}",
            out.text
        );
        let names: Vec<_> = fs::read_dir(env.work().join("ro")).expect("read dir").collect();
        assert!(names.is_empty(), "no temp file left behind");
    } else {
        assert_eq!(
            fs::read_to_string(env.work().join("ro/new.txt")).expect("read back"),
            "x"
        );
    }
}
