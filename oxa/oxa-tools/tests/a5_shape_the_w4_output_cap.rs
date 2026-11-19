//! A5 acceptance tests: shape: the W4 output cap.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_tools::{shape, strip_ansi};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const FAMILY: &str = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
const MARK_PREFIX: &str = "[output truncated: ";

/// Deterministic pseudo-random generator (xorshift64*), no external crate.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Pieces that exercise newlines, ANSI, multi-byte text, ZWJ emoji and invalid UTF-8.
const PIECES: &[&[u8]] = &[
    b"abc def ",
    b"\n",
    b"\r\n",
    b"\t",
    b"\x1b[31mred\x1b[0m",
    b"\x1b]0;title\x07",
    "\u{e9}t\u{e9} ".as_bytes(),
    "\u{4e2d}\u{6587} ".as_bytes(),
    FAMILY.as_bytes(),
    "\u{1f44d}\u{1f3fd} ".as_bytes(),
    b"\xff\xfe",
    b"0123456789 0123456789 0123456789 ",
];

fn random_raw(rng: &mut Rng, pieces: usize) -> Vec<u8> {
    let mut raw = Vec::new();
    for _ in 0..pieces {
        raw.extend_from_slice(PIECES[rng.below(PIECES.len())]);
    }
    raw
}

fn spill_dir(env: &TestEnv) -> PathBuf {
    env.tmp().join("spill")
}

/// Split a shaped output that went over the cap into (head, mark, tail).
fn split_marked(out: &str) -> (&str, &str, &str) {
    let mark_start = out.find(MARK_PREFIX).expect("mark line present");
    let head = &out[..mark_start - 1];
    let rest = &out[mark_start..];
    let mark_end = rest.find('\n').expect("mark ends with newline");
    (head, &rest[..mark_end], &rest[mark_end + 1..])
}

/// two runs, identical output
#[test]
fn deterministic_same_input_same_bytes() {
    let env = TestEnv::new();
    let mut rng = Rng::new(7);
    for cap in [256usize, 1000, 4096] {
        for keep_tail in [false, true] {
            let raw = random_raw(&mut rng, 400);
            let a = shape(&raw, cap, keep_tail, &spill_dir(&env));
            let b = shape(&raw, cap, keep_tail, &spill_dir(&env));
            assert_eq!(a, b, "cap {cap} keep_tail {keep_tail}");
        }
    }
}

/// for caps 256..64K and random inputs
#[test]
fn never_exceeds_cap() {
    let env = TestEnv::new();
    let caps = [256usize, 257, 300, 512, 1000, 4096, 16384, 65536];
    for seed in 0..24u64 {
        let mut rng = Rng::new(seed + 1);
        let pieces = 1 + rng.below(3000);
        let raw = random_raw(&mut rng, pieces);
        for &cap in &caps {
            for keep_tail in [false, true] {
                let out = shape(&raw, cap, keep_tail, &spill_dir(&env));
                assert!(out.len() <= cap, "seed {seed} cap {cap}: {} bytes", out.len());
            }
        }
    }
    // Raw sizes that straddle the cap exactly.
    for cap in [256usize, 1024] {
        for n in [cap - 1, cap, cap + 1, cap + 2] {
            let raw = vec![b'x'; n];
            let out = shape(&raw, cap, false, &spill_dir(&env));
            assert!(out.len() <= cap);
        }
    }
}

/// cuts never land inside a UTF-8 sequence or a ZWJ emoji
#[test]
fn valid_utf8_no_split_emoji() {
    let env = TestEnv::new();
    let line = format!(
        "{} caf\u{e9} \u{4e2d}\u{6587} {}\u{1f3fd}x\u{fe0f}\n",
        FAMILY, FAMILY
    );
    let raw = line.repeat(200).into_bytes();
    for cap in (256..=2048).step_by(37) {
        for keep_tail in [false, true] {
            let out = shape(&raw, cap, keep_tail, &spill_dir(&env));
            // String is valid UTF-8 by construction; also check the cut edges.
            let (head, _mark, tail) = split_marked(&out);
            assert!(!head.ends_with('\u{200d}'), "cap {cap}: head ends in ZWJ");
            assert!(!tail.starts_with('\u{200d}'), "cap {cap}: tail starts in ZWJ");
            assert!(!tail.starts_with('\u{fe0f}'), "cap {cap}: tail starts in VS16");
            assert!(
                !tail.starts_with('\u{1f3fd}'),
                "cap {cap}: tail starts in skin tone"
            );
            // Every emoji family in the head and the tail is whole.
            for part in [head, tail] {
                assert_eq!(
                    part.matches('\u{200d}').count() % 2,
                    0,
                    "cap {cap}: split ZWJ sequence"
                );
            }
            // head and tail come from the source text, in order.
            let source = String::from_utf8(raw.clone()).unwrap();
            assert!(source.contains(head));
            assert!(source.contains(tail));
        }
    }
}

/// CSI/OSC sequences removed, text kept
#[test]
fn ansi_stripped() {
    let input = "\u{1b}[31mred\u{1b}[0m plain \u{1b}]0;title\u{7}done\u{1b}]8;;http://x\u{1b}\\link\u{1b}]8;;\u{1b}\\\n\t\u{7}bell\u{1b}[2K\u{1b}[1;1H\rend";
    assert_eq!(strip_ansi(input), "red plain donelink\n\tbellend");
    // Through shape: under the cap the result is the stripped text, with CRLF folded.
    let env = TestEnv::new();
    let out = shape(input.as_bytes(), 4096, false, &spill_dir(&env));
    assert_eq!(out, "red plain donelink\n\tbellend");
}

/// spill file content == raw, name = sha256(raw)
#[test]
fn spill_holds_every_byte() {
    let env = TestEnv::new();
    let mut raw = random_raw(&mut Rng::new(99), 2000);
    raw.extend_from_slice(b"\xff tail marker\r\n");
    let dir = spill_dir(&env);
    let out = shape(&raw, 512, false, &dir);
    let digest = Sha256::digest(&raw);
    let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!("{name}.txt"));
    assert_eq!(fs::read(&path).expect("spill file exists"), raw);
    assert!(
        out.contains(&path.display().to_string()),
        "mark names the spill path"
    );
    assert!(out.contains(&format!("{} bytes", raw.len())));
}

/// no mark, no spill
#[test]
fn under_cap_unchanged() {
    let env = TestEnv::new();
    let raw = "line one\nline two\n".repeat(10).into_bytes();
    let dir = spill_dir(&env);
    let out = shape(&raw, 16 * 1024, false, &dir);
    assert_eq!(out, String::from_utf8(raw.clone()).unwrap());
    assert!(!out.contains(MARK_PREFIX));
    assert!(!dir.exists(), "no spill directory for under-cap output");
}

/// bash shape
#[test]
fn keep_tail_gives_tail_three_quarters() {
    let env = TestEnv::new();
    let raw: String = (0..2000).map(|i| format!("line {i:04}\n")).collect();
    let cap = 1000usize;

    let out = shape(raw.as_bytes(), cap, true, &spill_dir(&env));
    let (head, mark, tail) = split_marked(&out);
    let budget = cap - mark.len() - 2;
    let three_quarters = budget * 3 / 4;
    assert!(
        tail.len() <= three_quarters,
        "tail {} > {}",
        tail.len(),
        three_quarters
    );
    assert!(tail.len() + 12 >= three_quarters, "tail {} too short", tail.len());
    assert!(head.len() <= budget - three_quarters);
    assert!(tail.ends_with("line 1999\n") || tail.ends_with("line 1999"));

    // Head-first shaping gives the head three quarters instead.
    let out = shape(raw.as_bytes(), cap, false, &spill_dir(&env));
    let (head, mark, tail) = split_marked(&out);
    let budget = cap - mark.len() - 2;
    assert!(head.len() <= budget * 3 / 4);
    assert!(head.len() + 12 >= budget * 3 / 4, "head {} too short", head.len());
    assert!(tail.len() <= budget - budget * 3 / 4);
    assert!(head.starts_with("line 0000"));
}

/// mark says not saved
#[test]
fn unwritable_spill_dir_still_returns() {
    let env = TestEnv::new();
    // A regular file where the spill directory should be: the directory cannot be created.
    let blocker = env.write("blocker", "not a directory");
    let dir: &Path = &blocker.join("spill");
    let raw = vec![b'a'; 5000];
    let out = shape(&raw, 300, false, dir);
    assert!(out.len() <= 300);
    assert!(out.contains("full output not saved"), "got: {out}");
    assert!(!out.contains("full output in"));
    assert_eq!(fs::read_to_string(&blocker).unwrap(), "not a directory");
}

#[test]
fn spill_file_is_mode_0600_in_0700_dir() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    let dir = spill_dir(&env);
    let raw = vec![b'z'; 3000];
    let _ = shape(&raw, 256, true, &dir);
    let mode_dir = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode_dir, 0o700);
    let file = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    let mode_file = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode_file, 0o600);
}

#[test]
fn repeated_shape_keeps_existing_spill_file() {
    let env = TestEnv::new();
    let dir = spill_dir(&env);
    let raw = vec![b'q'; 4000];
    let a = shape(&raw, 300, false, &dir);
    let b = shape(&raw, 300, false, &dir);
    assert_eq!(a, b);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
}

#[test]
fn very_long_spill_path_falls_back_to_short_mark() {
    let env = TestEnv::new();
    let mut dir = env.tmp();
    for _ in 0..3 {
        dir = dir.join("d".repeat(120));
    }
    let raw = vec![b'w'; 5000];
    let out = shape(&raw, 256, false, &dir);
    assert!(out.len() <= 256);
    assert!(out.contains("[output truncated: 5000 bytes]"), "got: {out}");
}

#[test]
fn invalid_utf8_is_replaced_not_rejected() {
    let env = TestEnv::new();
    let raw = b"ok \xff\xfe bytes".to_vec();
    let out = shape(&raw, 4096, false, &spill_dir(&env));
    assert_eq!(out, "ok \u{fffd}\u{fffd} bytes");
}

#[test]
fn cap_below_minimum_is_raised() {
    let env = TestEnv::new();
    let raw = vec![b'm'; 1000];
    let a = shape(&raw, 10, false, &spill_dir(&env));
    let b = shape(&raw, 256, false, &spill_dir(&env));
    assert_eq!(a, b);
    assert!(a.len() <= 256);
}
