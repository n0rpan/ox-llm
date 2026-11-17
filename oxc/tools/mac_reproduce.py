#!/usr/bin/env python3
"""Reproduce the lab's matched local-LLM comparison rows on an Apple Silicon Mac.

Written and unit-tested on Linux; NOT yet run on a Mac.

WHAT IT DOES
  Runs stock software on the same model files, quantisations and fixture prompts
  that the lab used, and writes one receipt JSON per (model, stack):
    - llama-metal: upstream llama.cpp at the pinned commit
      5f490d7bfe78844e6390c869454c149fd7e003a8, default Metal build, all layers on GPU.
    - llama-cpu:   the same binaries with -ngl 0 (CPU only), threads = performance cores.
    - mlx:         MLX via mlx-lm. MLX cannot load GGUF, so you convert the model to MLX
                   yourself and pass its directory with --mlx-model. These receipts say
                   "same_quant_file": false.
    - oxe-cpu:     the repository's own CPU-only engine, using an `oxe` binary you installed
                   yourself (--oxe-bin PATH). Decode (`oxe bench`) and prefill (`oxe
                   prefill-bench`) are timed separately. Both read the same verified GGUF file
                   as the llama.cpp arms, so same_quant_file is true, but they take fixed token
                   ids, not the fixture text: the receipt says same_prompt_ids false and
                   unmatched_format true, and no prompt-matched speedup is claimed.
    - oxe-adapter: the adapter arm from N4. The interface is listed by `plan`, but running it
                   is refused until that arm exists, so no receipt is ever produced for it.
  This script never builds, clones, downloads or installs anything. You build llama.cpp
  yourself from the pinned commit (the `run` command prints the exact steps if it is missing).

HOW LONG
  Not measured. It depends on the Mac: each llama-bench invocation and each completion
  run loads a 17-84 GB model, and the Flash model is about 84 GB of files in total.

WHAT IT COLLECTS (complete list)
  - Chip name, model identifier (for example Mac15,6), performance and efficiency core
    counts, RAM size in GB, macOS version.
  - Power source (ac / battery / unknown) and the CPU_Speed_Limit value from pmset,
    sampled at the start of the run and at the end of each cell.
  - SHA-256 of the model files, of the llama-bench and llama-completion binaries, and of
    the MLX weight and config files (file names and sizes too).
  - Speeds and token counts reported by llama-bench, llama-completion and mlx-lm, with
    the exact argv (paths replaced by placeholders).
  - For oxe-cpu: SHA-256 of the oxe binary, SHA-256 of the fixed synthetic prompt ids the
    cell used (the ids themselves are a constant in this script; no fixture text is sent
    to the engine), the engine's reported build commit, and the hash of ids it generated.
  - SHA-256 of each generated text. The generated text itself is NOT stored.
  - Commit, tree state, the UTC time, and this script's own SHA-256.

WHAT IT DOES NOT COLLECT
  No serial number, hostname, user name, home directory, prices or network identifiers.
  It never runs system_profiler, ioreg, hostname or scutil. Every receipt is scanned for
  these before it is written; a hit stops the write and nothing is saved.

COMMANDS (run from the directory that holds this script)
  1. python3 mac_reproduce.py plan
     Prints the cells and commands. Runs nothing.
  2. python3 mac_reproduce.py verify-models --models-dir DIR
     SHA-256 checks the model files in DIR against the manifest.
  3. python3 mac_reproduce.py run --models-dir DIR --llama-dir DIR --out receipts/ \
         [--models ornith,qwen,flash] [--stacks llama-metal,llama-cpu,mlx] \
         [--mlx-model ornith=MLX_DIR ...]
     DIR is the folder with the GGUF files; --llama-dir is your llama.cpp checkout.
  4. python3 mac_reproduce.py run --models-dir DIR --oxe-bin PATH --out receipts/ \
         --stacks oxe-cpu
     PATH is the `oxe` binary you installed from this repository; nothing is downloaded.
  Please send back only the receipts/*.json files.

  No CPU-vs-GPU/Metal/MLX ratio is a gate. A decode or prefill ratio between arms is only
  reportable with the phase medians both receipts carry and the conditions they state.
"""

from __future__ import annotations

import argparse
import collections
import datetime
import getpass
import hashlib
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

SCANNER_ID = "mac_reproduce/1"
RECEIPT_SCHEMA = "snapdragon-local-ai-lab/receipt/v1"
PINNED_COMMIT = "5f490d7bfe78844e6390c869454c149fd7e003a8"
UPSTREAM_URL = "https://github.com/ggml-org/llama.cpp.git"
LLAMA_BINARIES = ("llama-bench", "llama-completion")
STACKS = ("llama-metal", "llama-cpu", "mlx")
LLAMA_CPP_STACKS = ("llama-metal", "llama-cpu")
OXE_STACKS = ("oxe-cpu", "oxe-adapter")
ALL_STACKS = STACKS + OXE_STACKS
# Fixed synthetic prompt ids for the oxe-cpu cells. `oxe bench` and `oxe prefill-bench` accept token ids,
# not text, so both are given these ids explicitly; engine.input records their SHA-256. The ids themselves
# are arbitrary (never tokenised fixture text), which is why the cell carries unmatched_format true.
OXE_PROMPT_TOKENS = 512
OXE_PROMPT_IDS = tuple((i * 2654435761 % 150000) + 1000 for i in range(OXE_PROMPT_TOKENS))
DEFAULT_REPS = 3
MIN_REPS = 3
CTX = 4096
GEN_TOKENS = 128
BENCH_PREFILL_TOKENS = 512
BENCH_DECODE_TOKENS = 128
HASH_CHUNK = 8 * 1024 * 1024
CMD_TIMEOUT_S = 30
BENCH_TIMEOUT_S = 4 * 3600
COMPLETION_TIMEOUT_S = 2 * 3600
MLX_TIMEOUT_S = 4 * 3600
OXE_PREFILL_TIMEOUT_S = 4 * 3600
FIXTURE_RELPATH = os.path.join("benchmarks", "fixtures", "code-screen-v1.json")

# Source of each manifest entry: dependencies/model-artifacts.json (ids ornith-1.5-35b-a3b-apex-mtp-compact,
# qwen36-moe-mtp-ud-q4-k-m, qwen38-flash-next-ud-iq3-xxs). Repo names, revisions, file names, byte counts and
# SHA-256 values are cross-checked by tests/test_mac_reproduce.py against that file when it is present.
MANIFEST = {
    "ornith": {
        "name": "Ornith-1.5-35B-A3B (APEX-MTP Compact GGUF)",
        "repo": "mudler/Ornith-1.5-35B-A3B-APEX-MTP-GGUF",
        "revision": "4c326713c52d13fdb40983a2a09a46cca8e88739",
        "quant": "APEX-MTP Compact",
        "files": [
            {"name": "Ornith-1.5-35B-A3B-APEX-MTP-Compact.gguf", "bytes": 17437861152,
             "sha256": "a7373f724cf3693e8a6a5f8209ae9a06ff8071ae2af60659bdbdf8ff6f6c10df"},
        ],
    },
    "qwen": {
        "name": "Qwen3.6-35B-A3B MTP UD-Q4_K_M",
        "repo": "unsloth/Qwen3.6-35B-A3B-MTP-GGUF",
        "revision": "5bc3e238d916f48a861bac2f8a1990a0e9b7e98d",
        "quant": "UD-Q4_K_M",
        "files": [
            {"name": "Qwen3.6-35B-A3B-UD-Q4_K_M.gguf", "bytes": 22663387424,
             "sha256": "0b21525e972670ed59e1812e170b27c26355381f0656ecc4e25617ece7dac58b"},
        ],
    },
    "flash": {
        "name": "Qwen3.8-Flash-Next mixed UD-IQ3_XXS (four shards)",
        "repo": "unsloth/Qwen3.8-Flash-Next-GGUF",
        "revision": "084c631ac221016019d6e0a863afe92d90edd3b0",
        "quant": "UD-IQ3_XXS mixed",
        "files": [
            {"name": "Qwen3.8-Flash-Next-UD-IQ3_XXS-00001-of-00004.gguf", "bytes": 10946618,
             "sha256": "9d5644b371ffac921427c8021cc7242dab8e3ba37d7b48d1e79fe3770fbf9915"},
            {"name": "Qwen3.8-Flash-Next-UD-IQ3_XXS-00002-of-00004.gguf", "bytes": 49946027488,
             "sha256": "aacaff3bdb9032c2c3be428167930809735f65d3cfb204eba07d85e421e7ddf4"},
            {"name": "Qwen3.8-Flash-Next-UD-IQ3_XXS-00003-of-00004.gguf", "bytes": 4869325888,
             "sha256": "ef8f14e790ab471b363f146d5e8664eefbaf03e5776f317a1fb96a2bc7707b07"},
            {"name": "Qwen3.8-Flash-Next-UD-IQ3_XXS-00004-of-00004.gguf", "bytes": 28800138432,
             "sha256": "5de8b5dbf89409acc39468decafd4a4a95b13c29d88a14b0356107897829ec36"},
        ],
    },
}

# Driver run as `sys.executable -c MLX_DRIVER MODEL_DIR PROMPT_FILE N_TOKENS REPS`.
# It prints exactly one JSON object on its last stdout line. Untested against a real mlx-lm.
MLX_DRIVER = r'''
import hashlib
import json
import sys
from importlib import metadata


def _version(name):
    try:
        return metadata.version(name)
    except Exception:
        return None


def _emit(obj):
    print(json.dumps(obj, sort_keys=True))


def main():
    model_dir, prompt_path = sys.argv[1], sys.argv[2]
    n_tokens, reps = int(sys.argv[3]), int(sys.argv[4])
    try:
        import mlx_lm
    except Exception as exc:
        _emit({"status": "skipped", "reason": "mlx_lm not importable (" + type(exc).__name__ + ")"})
        return
    out = {"status": "ok", "mlx": _version("mlx"), "mlx_lm": _version("mlx-lm"), "runs": []}
    try:
        with open(prompt_path, encoding="utf-8") as fh:
            prompt = fh.read()
        model, tokenizer = mlx_lm.load(model_dir)
        kwargs = {"max_tokens": n_tokens}
        try:
            from mlx_lm.sample_utils import make_sampler
            kwargs["sampler"] = make_sampler(temp=0.0)
        except Exception:
            out["sampler_note"] = "make_sampler unavailable; library default sampler used"
        for _ in range(reps):
            pieces = []
            last = None
            for resp in mlx_lm.stream_generate(model, tokenizer, prompt, **kwargs):
                pieces.append(getattr(resp, "text", "") or "")
                last = resp
            text = "".join(pieces)
            out["runs"].append({
                "prompt_tokens": getattr(last, "prompt_tokens", None),
                "generation_tokens": getattr(last, "generation_tokens", None),
                "prefill_tok_s": getattr(last, "prompt_tps", None),
                "decode_tok_s": getattr(last, "generation_tps", None),
                "peak_memory_reported": getattr(last, "peak_memory", None),
                "output_sha256": hashlib.sha256(text.encode("utf-8")).hexdigest(),
            })
    except Exception as exc:
        out["status"] = "failed"
        out["reason"] = type(exc).__name__
    _emit(out)


main()
'''

PROMPT_PERF_RE = re.compile(
    r"prompt eval time =\s*([\d.]+) ms /\s*(\d+) tokens \(\s*[\d.]+ ms per token,\s*([\d.]+) tokens per second\)")
EVAL_PERF_RE = re.compile(
    r"(?<!prompt )eval time =\s*([\d.]+) ms /\s*(\d+) runs\s*\(\s*[\d.]+ ms per token,\s*([\d.]+) tokens per second\)")

# Public-safety rules: (rule name, regex). Only rule names are ever reported, never matched text.
STATIC_RULES = [
    ("home-path-prefix", re.compile(r"/(?:Users|home|root|private/var|var/folders)/")),
    ("mac-address", re.compile(r"\b(?:[0-9A-Fa-f]{2}[:-]){5}[0-9A-Fa-f]{2}\b")),
    ("serial-number", re.compile(r"serial(?:_number)?[\"']?\s*[:=]\s*[\"']?\S{6,}", re.IGNORECASE)),
    ("email-address", re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}")),
    ("api-key-sk", re.compile(r"sk-[A-Za-z0-9_-]{20,}")),
    ("github-token", re.compile(r"gh[pousr]_[A-Za-z0-9]{30,}")),
    ("private-key-block", re.compile(r"-----BEGIN .*PRIVATE KEY-----")),
]


class UsageError(Exception):
    pass


class PublicSafetyRefusal(Exception):
    def __init__(self, rules):
        super().__init__("public-safety scan refused the receipt")
        self.rules = list(rules)


CmdResult = collections.namedtuple("CmdResult", "returncode stdout stderr wall_s")


# ---------------------------------------------------------------- small helpers

def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def script_sha256():
    with open(os.path.abspath(__file__), "rb") as fh:
        return hashlib.sha256(fh.read()).hexdigest()


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(HASH_CHUNK), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _text(value):
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", "replace")
    return str(value)


def run_cmd(argv, timeout):
    """Run a command with a timeout. Never raises for command failure; returns a CmdResult."""
    started = time.monotonic()
    try:
        proc = subprocess.run(argv, capture_output=True, timeout=timeout, check=False)
        rc, out, err = proc.returncode, _text(proc.stdout), _text(proc.stderr)
    except subprocess.TimeoutExpired as exc:
        rc, out, err = -1, _text(exc.stdout), _text(exc.stderr) + "\n[timed out]"
    except OSError as exc:
        rc, out, err = 127, "", "could not start command: " + type(exc).__name__
    return CmdResult(rc, out, err, round(time.monotonic() - started, 3))


def _stderr_tail(text, limit=800):
    text = (text or "").strip()
    return text[-limit:] if text else "(no stderr)"


def _report_failure(cell, what, res):
    cell["notes"].append("%s failed (returncode %s)" % (what, res.returncode))
    sys.stderr.write("---- %s failed; stderr tail (terminal only, not stored) ----\n%s\n"
                     % (what, _stderr_tail(res.stderr)))


# ---------------------------------------------------------------- hardware (macOS)

def _sysctl(name):
    res = run_cmd(["sysctl", "-n", name], CMD_TIMEOUT_S)
    value = res.stdout.strip()
    return value if res.returncode == 0 and value else None


def _int_or_zero(value):
    return int(value) if value and value.isdigit() else 0


def sample_power_thermal():
    """Return (power_source, cpu_speed_limit) using pmset only."""
    power = "unknown"
    batt = run_cmd(["pmset", "-g", "batt"], CMD_TIMEOUT_S)
    if batt.returncode == 0:
        if "'AC Power'" in batt.stdout:
            power = "ac"
        elif "'Battery Power'" in batt.stdout:
            power = "battery"
    limit = None
    therm = run_cmd(["pmset", "-g", "therm"], CMD_TIMEOUT_S)
    if therm.returncode == 0:
        match = re.search(r"CPU_Speed_Limit\s*=\s*(\d+)", therm.stdout)
        if match:
            limit = int(match.group(1))
    return power, limit


def collect_hardware(system=None):
    """Hardware block. Only sysctl, sw_vers and pmset are ever invoked."""
    system = system or platform.system()
    machine = platform.machine()
    hw = {
        "chip": None, "model_identifier": None, "performance_cores": 0, "efficiency_cores": 0,
        "ram_gb": 0, "os": "macOS" if system == "Darwin" else system,
        "os_version": None, "is_apple_silicon": False, "power_source": "unknown",
        "cpu_speed_limit": None,
    }
    if system != "Darwin":
        hw["chip"] = machine or None
        hw["os_version"] = platform.release() or None
        return hw
    hw["is_apple_silicon"] = machine == "arm64"
    hw["chip"] = _sysctl("machdep.cpu.brand_string")
    mem = _sysctl("hw.memsize")
    hw["ram_gb"] = round(int(mem) / 2 ** 30) if mem and mem.isdigit() else 0
    hw["performance_cores"] = _int_or_zero(_sysctl("hw.perflevel0.physicalcpu"))
    hw["efficiency_cores"] = _int_or_zero(_sysctl("hw.perflevel1.physicalcpu"))
    hw["model_identifier"] = _sysctl("hw.model")
    ver = run_cmd(["sw_vers", "-productVersion"], CMD_TIMEOUT_S)
    hw["os_version"] = ver.stdout.strip() if ver.returncode == 0 and ver.stdout.strip() else None
    hw["power_source"], hw["cpu_speed_limit"] = sample_power_thermal()
    return hw


# ---------------------------------------------------------------- llama.cpp checkout

def check_llama_checkout(llama_dir):
    """Return (ok, problems, info). Read-only: git queries and file checks only."""
    problems = []
    info = {"binaries": [], "bin_dir": os.path.join(llama_dir, "build", "bin"), "tree_clean": False}
    head = run_cmd(["git", "-C", llama_dir, "rev-parse", "HEAD"], CMD_TIMEOUT_S)
    if head.returncode != 0:
        problems.append("the llama.cpp directory is not a git checkout")
    else:
        if head.stdout.strip() != PINNED_COMMIT:
            problems.append("HEAD is not the pinned commit %s" % PINNED_COMMIT)
        status = run_cmd(["git", "-C", llama_dir, "status", "--porcelain"], CMD_TIMEOUT_S)
        if status.returncode != 0 or status.stdout.strip():
            problems.append("the working tree is not clean (git status --porcelain is not empty)")
        else:
            info["tree_clean"] = True
    for name in LLAMA_BINARIES:
        path = os.path.join(llama_dir, "build", "bin", name)
        if os.path.isfile(path) and os.access(path, os.X_OK):
            info["binaries"].append({"name": name, "path": path, "sha256": sha256_file(path)})
        else:
            problems.append("missing executable build/bin/%s" % name)
    return (not problems), problems, info


def print_llama_instructions():
    lines = [
        "Build llama.cpp yourself at the pinned commit (this script will not do it):",
        "  git clone %s <llama-dir>" % UPSTREAM_URL,
        "  git -C <llama-dir> checkout %s" % PINNED_COMMIT,
        "  cmake -S <llama-dir> -B <llama-dir>/build -DCMAKE_BUILD_TYPE=Release "
        "-DLLAMA_BUILD_TOOLS=ON -DLLAMA_BUILD_EXAMPLES=OFF",
        "  cmake --build <llama-dir>/build --config Release -j --target llama-bench llama-completion",
        "Then pass that <llama-dir> as --llama-dir. The Metal build is the default on Apple Silicon.",
    ]
    print("\n".join(lines))


# ---------------------------------------------------------------- argv builders and sanitising

def gpu_layers_for(stack):
    return 99 if stack == "llama-metal" else 0


def bench_argv(bench_bin, model_path, stack, threads, reps):
    argv = [bench_bin, "-m", model_path, "-t", str(threads),
            "-p", str(BENCH_PREFILL_TOKENS), "-n", str(BENCH_DECODE_TOKENS),
            "-r", str(reps), "-o", "json", "-ngl", str(gpu_layers_for(stack))]
    if stack == "llama-cpu":
        argv += ["-dev", "none", "-nopo", "1", "--no-host", "1", "--repack", "1",
                 "-lm", "mmap", "-fa", "off", "-ctk", "f16", "-ctv", "f16", "-b", "256", "-ub", "256"]
    return argv


def completion_argv(comp_bin, model_path, prompt_path, stack, threads):
    argv = [comp_bin, "-m", model_path, "-f", prompt_path, "-n", str(GEN_TOKENS),
            "--temp", "0", "-s", "1", "-c", str(CTX), "-t", str(threads),
            "-ngl", str(gpu_layers_for(stack)), "--ignore-eos", "-no-cnv", "--no-display-prompt"]
    if stack == "llama-cpu":
        argv += ["-fa", "off", "-ctk", "f16", "-ctv", "f16", "-b", "256", "-ub", "256",
                 "--no-op-offload", "--no-host"]
    return argv


def mlx_argv(python_bin, model_dir, prompt_path, reps):
    return [python_bin, "-c", MLX_DRIVER, model_dir, prompt_path, str(GEN_TOKENS), str(reps + 1)]


# ---------------------------------------------------------------- oxe (repo CPU engine) arm

def oxe_prompt_ids():
    """The fixed synthetic prompt ids both oxe subcommands are given (see OXE_PROMPT_IDS)."""
    return list(OXE_PROMPT_IDS)


def _oxe_ids_arg(ids):
    return ",".join(str(i) for i in ids)


def oxe_decode_argv(oxe_bin, model_path, threads, ids):
    """`oxe bench`: decode only (its prompt pass is not timed)."""
    return [oxe_bin, "bench", "--model", model_path, "--threads", str(threads), "--ctx", str(CTX),
            "--tokens", str(BENCH_DECODE_TOKENS), "--prompt-len", str(len(ids)),
            "--prompt-ids", _oxe_ids_arg(ids)]


def oxe_prefill_argv(oxe_bin, model_path, threads, ids):
    """`oxe prefill-bench`: batched prefill timing only, one rep, the binary's default prefill arm."""
    return [oxe_bin, "prefill-bench", "--model", model_path, "--threads", str(threads), "--ctx", str(CTX),
            "--tokens", _oxe_ids_arg(ids), "--prompt-len", str(len(ids)), "--reps", "1"]


_FORBIDDEN_ARG_PREFIXES = ("/home/", "/Users/", "/root/", "/private/", "/var/")


def sanitize_argv(argv, path_map, exact_map=None):
    """Replace each path argument by '<placeholder-dir>/<basename>'; refuse anything absolute left over."""
    exact_map = exact_map or {}
    out = []
    for arg in argv:
        if arg in exact_map:
            out.append(exact_map[arg])
        elif arg in path_map:
            out.append("%s/%s" % (path_map[arg], os.path.basename(arg)))
        else:
            out.append(arg)
    for arg in out:
        if arg.startswith("/") or any(prefix in arg for prefix in _FORBIDDEN_ARG_PREFIXES):
            raise ValueError("unsanitised absolute path left in argv")
    return out


# ---------------------------------------------------------------- output parsers

def parse_bench_json(text):
    try:
        data = json.loads(text)
    except ValueError as exc:
        raise ValueError("llama-bench output is not JSON") from exc
    if not isinstance(data, list) or not data:
        raise ValueError("llama-bench JSON is not a non-empty list")
    rows = []
    for item in data:
        if not isinstance(item, dict):
            raise ValueError("llama-bench JSON row is not an object")
        for key in ("n_prompt", "n_gen", "samples_ts"):
            if key not in item:
                raise ValueError("llama-bench JSON row lacks " + key)
        rows.append({key: item.get(key) for key in
                     ("n_prompt", "n_gen", "avg_ts", "stddev_ts", "samples_ts", "n_threads", "build_commit")})
    return rows


def select_bench_rows(rows):
    prefill = next((r for r in rows if r["n_prompt"] == BENCH_PREFILL_TOKENS and r["n_gen"] == 0), None)
    decode = next((r for r in rows if r["n_prompt"] == 0 and r["n_gen"] == BENCH_DECODE_TOKENS), None)
    if prefill is None or decode is None:
        raise ValueError("llama-bench JSON lacks the pp%d or tg%d rows" % (BENCH_PREFILL_TOKENS, BENCH_DECODE_TOKENS))
    return prefill, decode


def parse_completion_perf(text):
    """Parse llama-completion's perf lines (printed to stderr by common_perf_print)."""
    prompt = PROMPT_PERF_RE.search(text)
    evaluation = EVAL_PERF_RE.search(text)
    if not prompt or not evaluation:
        raise ValueError("perf lines not found")
    return {
        "prompt_tokens": int(prompt.group(2)),
        "prefill_tok_s": float(prompt.group(3)),
        "generated_tokens": int(evaluation.group(2)),
        "decode_tok_s": float(evaluation.group(3)),
    }


def parse_mlx_output(text):
    for line in reversed((text or "").splitlines()):
        line = line.strip()
        if line.startswith("{"):
            return json.loads(line)
    raise ValueError("no JSON line from the MLX driver")


def _oxe_json(text):
    """The oxe JSON object printed by `bench` / `prefill-bench` (pretty-printed on stdout)."""
    start = (text or "").find("{")
    if start < 0:
        raise ValueError("no JSON object in oxe output")
    try:
        obj, _end = json.JSONDecoder().raw_decode(text[start:])
    except ValueError as exc:
        raise ValueError("oxe output is not JSON") from exc
    if not isinstance(obj, dict):
        raise ValueError("oxe output is not a JSON object")
    return obj


def _oxe_number(value, name):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or value <= 0:
        raise ValueError("oxe %s is not a positive number" % name)
    return float(value)


def _oxe_count(value, name):
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError("oxe %s is not a non-negative integer" % name)
    return value


def parse_oxe_decode(text):
    """`oxe bench` JSON -> the decode half of a run. Raises ValueError when anything is missing."""
    data = _oxe_json(text)
    return {
        "decode_tok_s": _oxe_number(data.get("tok_s"), "tok_s"),
        "prompt_tokens": _oxe_count(data.get("prompt_tokens"), "prompt_tokens"),
        "generated_tokens": _oxe_count(data.get("tokens"), "tokens"),
        "generated_ids_hash": data.get("generated_hash") if isinstance(data.get("generated_hash"), str) else None,
        "engine_commit": data.get("engine_commit") if isinstance(data.get("engine_commit"), str) else None,
    }


def parse_oxe_prefill(text, prompt_len):
    """`oxe prefill-bench` JSON -> the prefill half of a run. Raises ValueError when it cannot be used."""
    data = _oxe_json(text)
    if data.get("prefix_len", 0) != 0:
        raise ValueError("oxe prefill-bench ran on a prefix")
    arms = data.get("arms")
    if not isinstance(arms, list) or len(arms) != 1 or not isinstance(arms[0], dict):
        raise ValueError("oxe prefill-bench output is not one arm")
    arm = arms[0]
    if _oxe_count(arm.get("prompt_tokens"), "prompt_tokens") != prompt_len:
        raise ValueError("oxe prefill-bench timed %s tokens, expected %d"
                         % (arm.get("prompt_tokens"), prompt_len))
    return {
        "prefill_tok_s": _oxe_number(arm.get("tok_s_median"), "tok_s_median"),
        "prompt_tokens": prompt_len,
        "prefill_attention": arm.get("prefill_attention") if isinstance(arm.get("prefill_attention"), dict) else None,
        "pf_tiles": arm.get("pf_tiles"),
        "pf_group": arm.get("pf_group"),
    }


def commit_matches(reported):
    rep = (reported or "").strip()
    return bool(rep) and (PINNED_COMMIT.startswith(rep) or rep.startswith(PINNED_COMMIT))


# ---------------------------------------------------------------- fixtures and models

def default_fixtures_path():
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.normpath(os.path.join(here, "..", FIXTURE_RELPATH))


def load_fixtures(path):
    with open(path, encoding="utf-8") as fh:
        data = json.load(fh)
    cases = data.get("cases") if isinstance(data, dict) else None
    if not isinstance(cases, list) or not cases:
        raise ValueError("fixture file has no 'cases' list")
    prompts = []
    seen = set()
    for case in cases:
        cid, text = case.get("id"), case.get("prompt")
        if not isinstance(cid, str) or not isinstance(text, str) or not text:
            raise ValueError("fixture case lacks a string id or prompt")
        if cid in seen:
            raise ValueError("duplicate fixture id")
        seen.add(cid)
        prompts.append((cid, text))
    return prompts


def verify_models(models_dir, model_ids):
    results = {}
    for mid in model_ids:
        files = []
        ok = True
        for spec in MANIFEST[mid]["files"]:
            path = os.path.join(models_dir, spec["name"])
            entry = {"name": spec["name"], "bytes": None, "sha256": None, "status": "missing",
                     "expected_sha256": spec["sha256"]}
            if os.path.isfile(path):
                entry["bytes"] = os.path.getsize(path)
                entry["sha256"] = sha256_file(path)
                entry["status"] = "match" if entry["sha256"] == spec["sha256"] else "mismatch"
            if entry["status"] != "match":
                ok = False
            files.append(entry)
        results[mid] = {"ok": ok, "files": files}
    return results


def describe_mlx_model(mlx_dir):
    """Return (files, quant_label) for an MLX model directory. Raises ValueError if unusable."""
    names = sorted(n for n in os.listdir(mlx_dir) if n == "config.json" or n.endswith(".safetensors"))
    if "config.json" not in names or not any(n.endswith(".safetensors") for n in names):
        raise ValueError("MLX directory needs config.json and *.safetensors files")
    files = []
    for name in names:
        path = os.path.join(mlx_dir, name)
        files.append({"name": name, "bytes": os.path.getsize(path), "sha256": sha256_file(path),
                      "sha256_matches_manifest": None})
    with open(os.path.join(mlx_dir, "config.json"), encoding="utf-8") as fh:
        config = json.load(fh)
    quant = config.get("quantization") if isinstance(config, dict) else None
    if isinstance(quant, dict) and "bits" in quant:
        label = "MLX quantized %s-bit (group_size %s)" % (quant.get("bits"), quant.get("group_size"))
    else:
        label = "MLX (no quantization block in config.json)"
    return files, label


# ---------------------------------------------------------------- receipts

def make_run(label, test, prompt_id, prompt_sha, rep, dropped, ctx, threads, gpu_layers, argv, returncode, **metrics):
    run = {
        "label": label, "test": test, "prompt_id": prompt_id, "prompt_sha256": prompt_sha,
        "rep": rep, "dropped_warmup": dropped, "ctx": ctx, "threads": threads,
        "gpu_layers": gpu_layers, "argv": argv, "prompt_tokens": None, "generated_tokens": None,
        "prefill_tok_s": None, "decode_tok_s": None, "wall_s": None, "output_sha256": None,
        "generated_ids_hash": None,
        "returncode": returncode,
    }
    for key, value in metrics.items():
        if key not in run:
            raise KeyError(key)
        run[key] = value
    return run


def summarize(runs):
    groups = collections.OrderedDict()
    for run in runs:
        if run["dropped_warmup"] or run["returncode"] != 0:
            continue
        for metric in ("prefill_tok_s", "decode_tok_s"):
            value = run.get(metric)
            if isinstance(value, (int, float)):
                groups.setdefault((run["test"], run["prompt_id"], metric), []).append(float(value))
    out = []
    for (test, prompt_id, metric), values in groups.items():
        out.append({"test": test, "prompt_id": prompt_id, "metric": metric, "n": len(values),
                    "median": round(statistics.median(values), 3),
                    "min": round(min(values), 3), "max": round(max(values), 3)})
    return out


def evidence_for(runs, status):
    counts = collections.OrderedDict()
    for run in runs:
        if run["dropped_warmup"] or run["returncode"] != 0:
            continue
        if run["prefill_tok_s"] is None and run["decode_tok_s"] is None:
            continue
        key = (run["test"], run["prompt_id"])
        counts[key] = counts.get(key, 0) + 1
    n = min(counts.values()) if counts else 0
    all_rc_zero = bool(runs) and all(run["returncode"] == 0 for run in runs)
    cls = "M" if (n >= 3 and all_rc_zero and status == "ok") else "S"
    return {"class": cls, "n": n}


def scan_public(text):
    """Return a list of rule NAMES that matched in text. Never returns matched values."""
    if not isinstance(text, str):
        text = str(text)
    hits = []

    def hit(name):
        if name not in hits:
            hits.append(name)

    homes = set()
    try:
        homes.add(os.path.expanduser("~"))
    except Exception:
        pass
    homes.add(os.environ.get("HOME", ""))
    for home in homes:
        if len(home) >= 2 and home not in ("/", "~") and home in text:
            hit("current-home-dir")
    try:
        user = getpass.getuser()
    except Exception:
        user = ""
    if user and len(user) >= 3 and re.search(r"\b%s\b" % re.escape(user), text, re.IGNORECASE):
        hit("login-name")
    node = platform.node() or ""
    labels = {node, node.split(".")[0]} if node else set()
    for label in labels:
        if len(label) >= 3 and re.search(r"\b%s\b" % re.escape(label), text, re.IGNORECASE):
            hit("hostname")
    for name, rx in STATIC_RULES:
        if rx.search(text):
            hit(name)
    return hits


def write_receipt(receipt, out_dir):
    """Serialise, scan, and write. Raises PublicSafetyRefusal and writes nothing on any hit."""
    receipt["created_utc"] = utc_now()
    text = json.dumps(receipt, indent=2) + "\n"
    rules = scan_public(text)
    if rules:
        raise PublicSafetyRefusal(rules)
    name = "%s-%s.json" % (receipt["model"]["id"], receipt["engine"]["stack"])
    path = os.path.join(out_dir, name)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(text)
    return path


def new_cell(model_id, stack, hardware):
    spec = MANIFEST[model_id]
    is_mlx = stack == "mlx"
    is_oxe = stack in OXE_STACKS
    return {
        "schema": RECEIPT_SCHEMA,
        "kind": "mac-reproduce",
        "ticket": None,
        "created_utc": None,
        "script_sha256": script_sha256(),
        "hardware": dict(hardware),
        "engine": {
            "name": "mlx-lm" if is_mlx else ("oxe" if is_oxe else "llama.cpp"),
            "stack": stack,
            "commit": None if (is_mlx or is_oxe) else PINNED_COMMIT,
            "tree_clean": None,
            "binaries": [],
            "version": None,
        },
        "model": {
            "id": model_id,
            "name": spec["name"],
            "quant": spec["quant"],
            "same_quant_file": not is_mlx,
            "files": [],
        },
        "runs": [],
        "summary": [],
        "evidence": {"class": "S", "n": 0},
        "status": "pending",
        "notes": [],
        "public_safe": {"scanner": SCANNER_ID, "hits": 0},
    }


def finalize_cell(cell, status):
    cell["status"] = status
    cell["summary"] = summarize(cell["runs"])
    cell["evidence"] = evidence_for(cell["runs"], status)


def _write_prompt(tmpdir, text):
    path = os.path.join(tmpdir, "prompt.txt")
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)
    return path, hashlib.sha256(text.encode("utf-8")).hexdigest()


# ---------------------------------------------------------------- cell runners

def run_bench(cell, bench_bin, model_path, stack, threads, reps, path_map):
    argv = bench_argv(bench_bin, model_path, stack, threads, reps)
    stored = sanitize_argv(argv, path_map)
    gpu = gpu_layers_for(stack)
    warm = run_cmd(argv, BENCH_TIMEOUT_S)
    cell["runs"].append(make_run("bench:warmup", "bench", None, None, 0, True, None, threads, gpu,
                                 stored, warm.returncode, wall_s=warm.wall_s))
    if warm.returncode != 0:
        _report_failure(cell, "llama-bench warm-up", warm)
        return False
    res = run_cmd(argv, BENCH_TIMEOUT_S)
    if res.returncode != 0:
        cell["runs"].append(make_run("bench:measured-failed", "bench", None, None, 0, False, None, threads,
                                     gpu, stored, res.returncode, wall_s=res.wall_s))
        _report_failure(cell, "llama-bench measured invocation", res)
        return False
    try:
        prefill_row, decode_row = select_bench_rows(parse_bench_json(res.stdout))
    except ValueError as exc:
        cell["notes"].append("llama-bench output could not be used: %s" % exc)
        return False
    reported = prefill_row.get("build_commit") or ""
    cell["engine"]["version"] = reported or None
    if not commit_matches(reported):
        cell["notes"].append("llama-bench did not report the pinned build commit; cell marked failed")
        return False
    cell["notes"].append("llama-bench build_commit reported: %s" % reported)
    prefill_samples = prefill_row.get("samples_ts") or []
    decode_samples = decode_row.get("samples_ts") or []
    if len(prefill_samples) != reps or len(decode_samples) != reps:
        cell["notes"].append("llama-bench returned %d/%d samples, expected %d"
                             % (len(prefill_samples), len(decode_samples), reps))
        return False
    for idx, value in enumerate(prefill_samples):
        cell["runs"].append(make_run("bench:pp%d:r%d" % (BENCH_PREFILL_TOKENS, idx + 1), "bench",
                                     "pp%d" % BENCH_PREFILL_TOKENS, None, idx + 1, False, None, threads, gpu,
                                     stored, res.returncode, prompt_tokens=prefill_row["n_prompt"],
                                     prefill_tok_s=float(value), wall_s=res.wall_s))
    for idx, value in enumerate(decode_samples):
        cell["runs"].append(make_run("bench:tg%d:r%d" % (BENCH_DECODE_TOKENS, idx + 1), "bench",
                                     "tg%d" % BENCH_DECODE_TOKENS, None, idx + 1, False, None, threads, gpu,
                                     stored, res.returncode, generated_tokens=decode_row["n_gen"],
                                     decode_tok_s=float(value), wall_s=res.wall_s))
    return True


def run_prompts(cell, comp_bin, model_path, stack, threads, reps, prompts, tmpdir, path_map):
    gpu = gpu_layers_for(stack)
    for prompt_id, text in prompts:
        prompt_path, psha = _write_prompt(tmpdir, text)
        argv = completion_argv(comp_bin, model_path, prompt_path, stack, threads)
        stored = sanitize_argv(argv, path_map)
        for k in range(reps + 1):
            warm = (k == 0)
            label = "prompt:%s:w0" % prompt_id if warm else "prompt:%s:r%d" % (prompt_id, k)
            res = run_cmd(argv, COMPLETION_TIMEOUT_S)
            if res.returncode != 0:
                cell["runs"].append(make_run(label, "prompt", prompt_id, psha, 0 if warm else k, warm, CTX,
                                             threads, gpu, stored, res.returncode, wall_s=res.wall_s))
                _report_failure(cell, "llama-completion (%s)" % prompt_id, res)
                return False
            try:
                perf = parse_completion_perf(res.stderr)
            except ValueError:
                try:
                    perf = parse_completion_perf(res.stdout)
                except ValueError:
                    cell["notes"].append("perf lines not found for prompt %s; cell marked failed" % prompt_id)
                    return False
            cell["runs"].append(make_run(
                label, "prompt", prompt_id, psha, 0 if warm else k, warm, CTX, threads, gpu, stored, 0,
                prompt_tokens=perf["prompt_tokens"], generated_tokens=perf["generated_tokens"],
                prefill_tok_s=perf["prefill_tok_s"], decode_tok_s=perf["decode_tok_s"],
                wall_s=res.wall_s, output_sha256=hashlib.sha256(res.stdout.encode("utf-8")).hexdigest()))
    return True


def run_mlx(cell, mlx_dir, prompts, reps, tmpdir, path_map):
    try:
        files, quant = describe_mlx_model(mlx_dir)
    except (OSError, ValueError) as exc:
        cell["notes"].append("MLX model directory unusable: %s" % type(exc).__name__)
        return "skipped"
    cell["model"]["files"] = files
    cell["model"]["quant"] = quant
    driver_sha = hashlib.sha256(MLX_DRIVER.encode("utf-8")).hexdigest()
    path_map = dict(path_map)
    path_map[mlx_dir] = "<mlx-model-dir>"
    path_map[os.path.join(tmpdir, "prompt.txt")] = "<tmp>"
    exact = {MLX_DRIVER: "<mlx-driver sha256=%s>" % driver_sha}
    cell["engine"]["binaries"] = [{"name": "mlx_driver", "sha256": driver_sha}]
    for prompt_id, text in prompts:
        prompt_path, psha = _write_prompt(tmpdir, text)
        argv = mlx_argv(sys.executable, mlx_dir, prompt_path, reps)
        stored = sanitize_argv(argv, path_map, exact)
        res = run_cmd(argv, MLX_TIMEOUT_S)
        if res.returncode != 0:
            cell["runs"].append(make_run("prompt:%s:failed" % prompt_id, "prompt", prompt_id, psha, 0, False,
                                         None, None, None, stored, res.returncode, wall_s=res.wall_s))
            _report_failure(cell, "MLX driver (%s)" % prompt_id, res)
            return "failed"
        try:
            result = parse_mlx_output(res.stdout)
        except ValueError:
            cell["notes"].append("MLX driver printed no JSON result; cell marked failed")
            return "failed"
        if result.get("status") == "skipped":
            cell["notes"].append("MLX skipped: %s" % result.get("reason"))
            return "skipped"
        if result.get("status") != "ok":
            cell["notes"].append("MLX driver failed: %s" % result.get("reason"))
            return "failed"
        cell["engine"]["version"] = "mlx %s; mlx-lm %s" % (result.get("mlx"), result.get("mlx_lm"))
        runs = result.get("runs") or []
        if len(runs) != reps + 1:
            cell["notes"].append("MLX driver returned %d generations, expected %d" % (len(runs), reps + 1))
            return "failed"
        for k, rr in enumerate(runs):
            warm = (k == 0)
            label = "prompt:%s:w0" % prompt_id if warm else "prompt:%s:r%d" % (prompt_id, k)
            cell["runs"].append(make_run(
                label, "prompt", prompt_id, psha, 0 if warm else k, warm, None, None, None, stored, 0,
                prompt_tokens=rr.get("prompt_tokens"), generated_tokens=rr.get("generation_tokens"),
                prefill_tok_s=rr.get("prefill_tok_s"), decode_tok_s=rr.get("decode_tok_s"),
                output_sha256=rr.get("output_sha256")))
    return "ok"


OXE_NEEDS = (
    "oxe-cpu needs --oxe-bin PATH: an oxe binary you installed from this repository (it is never "
    "downloaded, built or installed here)")


def check_oxe_bin(path):
    """(ok, problem) for the owner-supplied oxe binary. Read-only: stat/access only, never executes it."""
    if not path:
        return False, OXE_NEEDS
    if not os.path.exists(path):
        return False, "--oxe-bin: no such file (%s)" % OXE_NEEDS
    if not os.path.isfile(path):
        return False, "--oxe-bin is not a regular file (%s)" % OXE_NEEDS
    if not os.access(path, os.X_OK):
        return False, "--oxe-bin is not executable (%s)" % OXE_NEEDS
    return True, None


def _oxe_input_block(ids):
    payload = b"".join(int(i).to_bytes(4, "little") for i in ids)
    return {
        "kind": "synthetic-token-ids",
        "token_count": len(ids),
        "sha256": hashlib.sha256(payload).hexdigest(),
        "fixture_prompts_used": False,
    }


def run_oxe_cell(cell, oxe_bin, model_path, stack, threads, reps, tmpdir, path_map):
    """oxe-cpu: decode (`oxe bench`) and prefill (`oxe prefill-bench`) timed and recorded separately."""
    ids = oxe_prompt_ids()
    prompt_arg = _oxe_ids_arg(ids)
    id_placeholder = "<synthetic-ids:%d>" % len(ids)
    exact = {prompt_arg: id_placeholder}
    path_map = dict(path_map)
    path_map[oxe_bin] = "<oxe-bin>"
    path_map[model_path] = "<models-dir>"
    cell["engine"]["binaries"] = [{"name": "oxe", "sha256": sha256_file(oxe_bin)}]
    cell["engine"]["input"] = _oxe_input_block(ids)
    cell["format"] = {
        "same_quant_file": True,
        "same_prompt_ids": False,
        "unmatched_format": True,
        "note": "the oxe arms read the same verified GGUF bytes as llama-metal/llama-cpu, but they are "
                "given fixed token ids instead of the fixture text, so prompt-matched ratios with the "
                "llama.cpp or mlx cells are not claimed.",
    }
    cell["notes"].extend([
        "oxe is this repository's CPU engine; the binary is built and installed by the volunteer and "
        "identified by SHA-256. No NPU, GPU or device flag is passed.",
        "decode: oxe bench with %d fixed synthetic prompt ids, then %d timed tokens; its untimed prompt "
        "pass is one discarded invocation. prefill: oxe prefill-bench on the same ids, the binary's "
        "default prefill arm, one untimed warm-up then one timed rep per invocation; the arm's kernel, "
        "pf_tiles and pf_group are recorded in engine.prefill_attention."
        % (len(ids), BENCH_DECODE_TOKENS),
        "No fixture prompt text reaches the engine, so prompt_sha256/output_sha256 are null for this arm; "
        "the input is described by engine.input (id count and SHA-256) and each argv carries a placeholder "
        "for the id list.",
    ])
    if len(MANIFEST[cell["model"]["id"]]["files"]) > 1:
        cell["notes"].append("The manifest lists several GGUF shards for this model; only the first is passed "
                             "to oxe, so all shards stay verified as model identity.")

    def decode_once():
        argv = oxe_decode_argv(oxe_bin, model_path, threads, ids)
        stored = sanitize_argv(argv, path_map, exact)
        res = run_cmd(argv, BENCH_TIMEOUT_S)
        parsed = None
        if res.returncode == 0:
            try:
                parsed = parse_oxe_decode(res.stdout)
            except ValueError:
                parsed = None
        return stored, res, parsed

    ok = True
    for k in range(reps + 1):
        warm = (k == 0)
        label = "oxe-decode:%s" % ("w0" if warm else "r%d" % k)
        stored, res, parsed = decode_once()
        if res.returncode != 0:
            cell["runs"].append(make_run(label, "oxe-decode", "oxe-ids", None, 0 if warm else k, warm, CTX,
                                         threads, None, stored, res.returncode, wall_s=res.wall_s))
            _report_failure(cell, "oxe bench", res)
            ok = False
            break
        if parsed is None:
            cell["runs"].append(make_run(label, "oxe-decode", "oxe-ids", None, 0 if warm else k, warm, CTX,
                                         threads, None, stored, res.returncode, wall_s=res.wall_s))
            cell["notes"].append("oxe bench output could not be used (no positive tok_s / token counts)")
            ok = False
            break
        if parsed["engine_commit"]:
            cell["engine"]["version"] = "oxe " + parsed["engine_commit"]
        cell["runs"].append(make_run(
            label, "oxe-decode", "oxe-ids", None, 0 if warm else k, warm, CTX, threads, None, stored,
            res.returncode, prompt_tokens=parsed["prompt_tokens"],
            generated_tokens=parsed["generated_tokens"], decode_tok_s=parsed["decode_tok_s"],
            generated_ids_hash=parsed["generated_ids_hash"], wall_s=res.wall_s))
    if not ok:
        return "failed"

    for k in range(reps + 1):
        warm = (k == 0)
        label = "oxe-prefill:%s" % ("w0" if warm else "r%d" % k)
        argv = oxe_prefill_argv(oxe_bin, model_path, threads, ids)
        stored = sanitize_argv(argv, path_map, exact)
        res = run_cmd(argv, OXE_PREFILL_TIMEOUT_S)
        parsed = None
        note = None
        if res.returncode == 0:
            try:
                parsed = parse_oxe_prefill(res.stdout, len(ids))
            except ValueError as exc:
                note = "oxe prefill-bench output could not be used: %s" % exc
        if res.returncode != 0:
            cell["runs"].append(make_run(label, "oxe-prefill", "oxe-ids", None, 0 if warm else k, warm, CTX,
                                         threads, None, stored, res.returncode, wall_s=res.wall_s))
            _report_failure(cell, "oxe prefill-bench", res)
            return "failed"
        if parsed is not None:
            cell["engine"]["prefill_attention"] = parsed["prefill_attention"]
            cell["engine"]["pf_tiles"] = parsed["pf_tiles"]
            cell["engine"]["pf_group"] = parsed["pf_group"]
        if parsed is None:
            if note:
                cell["notes"].append(note)
            cell["runs"].append(make_run(label, "oxe-prefill", "oxe-ids", None, 0 if warm else k, warm, CTX,
                                         threads, None, stored, res.returncode, wall_s=res.wall_s))
            return "failed"
        cell["runs"].append(make_run(
            label, "oxe-prefill", "oxe-ids", None, 0 if warm else k, warm, CTX, threads, None, stored,
            res.returncode, prompt_tokens=parsed["prompt_tokens"], prefill_tok_s=parsed["prefill_tok_s"],
            wall_s=res.wall_s))
    return "ok"


STATIC_NOTES_LLAMA = [
    "Stock upstream llama.cpp at the pinned commit; binaries built by the volunteer, identified by SHA-256.",
    "bench: llama-bench's own synthetic tokens, not the fixture prompts; one discarded invocation first, "
    "then -r REPS. llama-bench's internal warm-up is left enabled.",
    "prompt: llama-completion, temperature 0, seed 1, -n 128, ctx 4096, --ignore-eos. decode_tok_s comes from the "
    "'eval time' perf line; generated_tokens is that line's run count (the first token from prompt eval is not "
    "counted).",
    "output_sha256 is the SHA-256 of llama-completion stdout as UTF-8 text. The generated text is not stored.",
    "Token IDs are produced by each engine's tokenizer from the same GGUF; this script does not compare token IDs "
    "across stacks.",
]


def run_llama_cell(cell, model_id, stack, llama_info, models_dir, prompts, reps, threads, tmpdir):
    bin_dir = llama_info["bin_dir"]
    bench_bin = os.path.join(bin_dir, "llama-bench")
    comp_bin = os.path.join(bin_dir, "llama-completion")
    model_path = os.path.join(models_dir, MANIFEST[model_id]["files"][0]["name"])
    path_map = {
        bench_bin: "<llama-dir>/build/bin",
        comp_bin: "<llama-dir>/build/bin",
        model_path: "<models-dir>",
        os.path.join(tmpdir, "prompt.txt"): "<tmp>",
    }
    cell["notes"].extend(STATIC_NOTES_LLAMA)
    if len(MANIFEST[model_id]["files"]) > 1:
        cell["notes"].append("Split GGUF: the first shard is passed to -m; llama.cpp loads the sibling shards "
                             "from the same directory.")
    if stack == "llama-cpu":
        cell["notes"].append("llama-cpu uses the same default Apple build as llama-metal: Metal is compiled in and "
                             "Accelerate BLAS is on by default. This is not the Linux CPU build (BLAS and Metal off). "
                             "CPU mask and strict pinning (-C, --cpu-strict) are not applied on macOS.")
    ok = run_bench(cell, bench_bin, model_path, stack, threads, reps, path_map)
    if ok:
        ok = run_prompts(cell, comp_bin, model_path, stack, threads, reps, prompts, tmpdir, path_map)
    return "ok" if ok else "failed"


def run_one_cell(cell, model_id, stack, ctx):
    verification = ctx["verification"][model_id]
    cell["model"]["files"] = [
        {"name": f["name"], "bytes": f["bytes"], "sha256": f["sha256"],
         "sha256_matches_manifest": f["status"] == "match"}
        for f in verification["files"]]
    if not verification["ok"]:
        bad = [f["name"] + " (" + f["status"] + ")" for f in verification["files"] if f["status"] != "match"]
        cell["notes"].append("model file(s) not usable: " + ", ".join(bad) + "; cells skipped")
        return "skipped"
    if stack == "mlx":
        mlx_dir = ctx["mlx_map"].get(model_id)
        if not mlx_dir:
            cell["notes"].append("MLX skipped: no --mlx-model %s=DIR given" % model_id)
            cell["model"]["files"] = []
            return "skipped"
        cell["notes"].append("MLX cannot load GGUF: this is a separately converted MLX model, so "
                             "same_quant_file is false. Token IDs are not compared with the GGUF runs.")
        return run_mlx(cell, mlx_dir, ctx["prompts"], ctx["reps"], ctx["tmpdir"],
                       {ctx["python_placeholder"]: "<python-dir>"})
    if stack == "oxe-adapter":
        cell["notes"].append("oxe-adapter is not available: the adapter arm is N4's deliverable; nothing was run.")
        return "skipped"
    if stack == "oxe-cpu":
        model_path = os.path.join(ctx["models_dir"], MANIFEST[model_id]["files"][0]["name"])
        return run_oxe_cell(cell, ctx["oxe_bin"], model_path, stack, ctx["threads"], ctx["reps"],
                            ctx["tmpdir"], {ctx["oxe_bin"]: "<oxe-bin>", model_path: "<models-dir>"})
    cell["engine"]["tree_clean"] = bool(ctx["llama_info"].get("tree_clean"))
    cell["engine"]["binaries"] = [{"name": b["name"], "sha256": b["sha256"]} for b in ctx["llama_info"]["binaries"]]
    return run_llama_cell(cell, model_id, stack, ctx["llama_info"], ctx["models_dir"], ctx["prompts"],
                          ctx["reps"], ctx["threads"], ctx["tmpdir"])


# ---------------------------------------------------------------- argument handling

def _parse_list(text, allowed, flag):
    items = [x.strip() for x in text.split(",") if x.strip()]
    if not items:
        raise UsageError("%s needs at least one value" % flag)
    bad = [x for x in items if x not in allowed]
    if bad:
        raise UsageError("%s: unknown value(s): %s (allowed: %s)" % (flag, ", ".join(bad), ", ".join(allowed)))
    return items


def _parse_mlx_models(values):
    out = {}
    for item in values or []:
        if "=" not in item:
            raise UsageError("--mlx-model expects ID=DIR")
        mid, path = item.split("=", 1)
        if mid not in MANIFEST:
            raise UsageError("--mlx-model: unknown model id %s" % mid)
        if not os.path.isdir(path):
            raise UsageError("--mlx-model %s: not a directory" % mid)
        out[mid] = path
    return out


def _load_prompts(path):
    path = path or default_fixtures_path()
    if not os.path.isfile(path):
        raise UsageError("fixtures file not found; pass --fixtures PATH")
    try:
        return load_fixtures(path)
    except (OSError, ValueError) as exc:
        raise UsageError("fixtures file unusable: %s" % exc)


def cmd_plan(args):
    model_ids = _parse_list(args.models, list(MANIFEST), "--models")
    stacks = _parse_list(args.stacks, ALL_STACKS, "--stacks")
    try:
        prompt_ids = [pid for pid, _ in _load_prompts(args.fixtures)]
    except UsageError:
        prompt_ids = ["<fixture-id>"]
    tests = ["bench (one discarded invocation, then -r %d)" % args.reps,
             "prompt x %s (each: 1 discarded + %d measured)" % (", ".join(prompt_ids), args.reps)]
    cells = 0
    for mid in model_ids:
        for stack in stacks:
            cells += 1
            print("== CELL %s / %s  (%s)" % (mid, stack, MANIFEST[mid]["name"]))
            if stack == "mlx":
                print("  needs: --mlx-model %s=<mlx-model-dir>   (cell skipped without it)" % mid)
                print("  python: <python-dir>/python3 -c <mlx-driver> <mlx-model-dir> <tmp>/prompt.txt %d %d"
                      % (GEN_TOKENS, args.reps + 1))
                continue
            if stack == "oxe-cpu":
                model_path = "<models-dir>/" + MANIFEST[mid]["files"][0]["name"]
                threads = "<performance-cores>"
                print("  needs: --oxe-bin PATH, an oxe binary you installed from this repository (the "
                      "script never downloads, builds or installs it; no device is used)")
                print("  format: same verified GGUF bytes as llama.cpp, but fixed token ids instead of "
                      "fixture text: same_prompt_ids false, unmatched_format true")
                print("  decode (%d discarded + %d measured): " % (1, args.reps)
                      + " ".join(oxe_decode_argv("<oxe-bin>", model_path, threads, oxe_prompt_ids())))
                print("  prefill (%d discarded + %d measured): " % (1, args.reps)
                      + " ".join(oxe_prefill_argv("<oxe-bin>", model_path, threads, oxe_prompt_ids())))
                print("  input: %d fixed synthetic token ids (no fixture text reaches the engine)" % OXE_PROMPT_TOKENS)
                continue
            if stack == "oxe-adapter":
                print("  needs: the adapter arm (N4). Not available here: `run` refuses this stack and "
                      "writes no receipt.")
                continue
            print("  needs: llama.cpp checkout at %s, clean tree, build/bin/llama-bench and llama-completion"
                  % PINNED_COMMIT)
            threads = "<performance-cores>"
            model_path = "<models-dir>/" + MANIFEST[mid]["files"][0]["name"]
            bench = bench_argv("<llama-dir>/build/bin/llama-bench", model_path, stack, threads, args.reps)
            print("  bench warm-up and measured: " + " ".join(bench))
            comp = completion_argv("<llama-dir>/build/bin/llama-completion", model_path, "<tmp>/prompt.txt",
                                   stack, threads)
            print("  prompt (per fixture id): " + " ".join(comp))
    print("")
    print("Cells: %d. Tests per llama cell: %s." % (cells, "; ".join(tests)))
    print("Tests per oxe-cpu cell: oxe bench (1 discarded + %d measured), oxe prefill-bench (1 discarded + "
          "%d measured)." % (args.reps, args.reps))
    print("Nothing was run. Commands above use placeholders: <models-dir>, <llama-dir>, <tmp>, <mlx-model-dir>, "
          "<oxe-bin>.")
    print("No arm-to-arm ratio is a gate here; report phase medians with the conditions stated in the receipts.")
    return 0


def cmd_verify(args):
    model_ids = _parse_list(args.models, list(MANIFEST), "--models") if args.models else list(MANIFEST)
    results = verify_models(args.models_dir, model_ids)
    bad = False
    for mid in model_ids:
        res = results[mid]
        print("%s: %s" % (mid, "all files match" if res["ok"] else "NOT USABLE (its cells will be skipped)"))
        for entry in res["files"]:
            suffix = "  sha256=" + entry["sha256"] if entry["sha256"] else ""
            print("  %-8s %s%s" % (entry["status"], entry["name"], suffix))
        bad = bad or not res["ok"]
    return 1 if bad else 0


def cmd_selfcheck(args):
    bad = False
    for path in args.files:
        name = os.path.basename(path)
        try:
            with open(path, encoding="utf-8", errors="replace") as fh:
                text = fh.read()
        except OSError as exc:
            print("%s: cannot read (%s)" % (name, type(exc).__name__))
            bad = True
            continue
        rules = scan_public(text)
        if rules:
            print("%s: HIT %s" % (name, ", ".join(rules)))
            bad = True
        else:
            print("%s: clean" % name)
    return 1 if bad else 0


def cmd_run(args):
    model_ids = _parse_list(args.models, list(MANIFEST), "--models")
    stacks = _parse_list(args.stacks, ALL_STACKS, "--stacks")
    if args.reps < MIN_REPS:
        raise UsageError("--reps must be at least %d" % MIN_REPS)
    if "oxe-adapter" in stacks:
        print("error: --stacks oxe-adapter is not available: the adapter arm is N4's deliverable. "
              "Nothing was run and no receipt was written.", file=sys.stderr)
        return 2
    mlx_map = _parse_mlx_models(args.mlx_model)
    if "oxe-cpu" in stacks:
        ok, problem = check_oxe_bin(args.oxe_bin)
        if not ok:
            print("error: %s. Nothing was run and no receipt was written." % problem, file=sys.stderr)
            return 2
    prompts = _load_prompts(args.fixtures)
    hardware = collect_hardware()
    if hardware["performance_cores"] < 1:
        print("Cannot read the performance core count (sysctl hw.perflevel0.physicalcpu). "
              "Run this on the Mac itself; nothing was run.", file=sys.stderr)
        return 2
    llama_info = {"binaries": [], "tree_clean": False}
    if any(s in LLAMA_CPP_STACKS for s in stacks):
        if not args.llama_dir:
            raise UsageError("--llama-dir is required for llama stacks")
        ok, problems, llama_info = check_llama_checkout(args.llama_dir)
        if not ok:
            print("llama.cpp checkout is not ready:", file=sys.stderr)
            for problem in problems:
                print("  - " + problem, file=sys.stderr)
            print_llama_instructions()
            return 2
    os.makedirs(args.out, exist_ok=True)
    verification = verify_models(args.models_dir, model_ids)
    for mid in model_ids:
        state = "usable" if verification[mid]["ok"] else "NOT usable (cells skipped)"
        print("model %s: %s" % (mid, state))
    threads = hardware["performance_cores"]
    tmpdir = tempfile.mkdtemp(prefix="mac-reproduce-")
    ctx = {
        "verification": verification, "mlx_map": mlx_map, "prompts": prompts, "reps": args.reps,
        "threads": threads, "tmpdir": tmpdir, "models_dir": args.models_dir, "llama_info": llama_info,
        "python_placeholder": sys.executable, "oxe_bin": args.oxe_bin,
    }
    exit_code = 0
    current = None
    try:
        for mid in model_ids:
            for stack in stacks:
                current = new_cell(mid, stack, hardware)
                status = run_one_cell(current, mid, stack, ctx)
                end_power, end_limit = sample_power_thermal()
                current["notes"].append("end of cell: power_source=%s, cpu_speed_limit=%s"
                                        % (end_power, end_limit))
                finalize_cell(current, status)
                try:
                    path = write_receipt(current, args.out)
                except PublicSafetyRefusal as refusal:
                    print("REFUSED to write %s-%s: public-safety rules hit: %s"
                          % (mid, stack, ", ".join(refusal.rules)), file=sys.stderr)
                    return 3
                print("%s / %s: %s, %d runs -> %s" % (mid, stack, status, len(current["runs"]), path))
                if status == "failed":
                    exit_code = 1
                current = None
    except KeyboardInterrupt:
        if current is not None:
            current["notes"].append("interrupted by user before this cell finished")
            finalize_cell(current, "interrupted")
            try:
                path = write_receipt(current, args.out)
                print("interrupted; wrote partial receipt -> %s" % path)
            except PublicSafetyRefusal as refusal:
                print("interrupted; partial receipt refused by public-safety rules: %s"
                      % ", ".join(refusal.rules), file=sys.stderr)
                return 3
        else:
            print("interrupted")
        return 130
    finally:
        shutil.rmtree(tmpdir, ignore_errors=True)
    return exit_code


def build_parser():
    parser = argparse.ArgumentParser(description="Reproduce the lab's matched local-LLM rows on a Mac.")
    sub = parser.add_subparsers(dest="cmd", required=True)

    def add_common(p):
        p.add_argument("--models", default=",".join(MANIFEST), help="comma list: ornith,qwen,flash")
        p.add_argument("--stacks", default=",".join(STACKS), help="comma list: " + ",".join(ALL_STACKS))
        p.add_argument("--reps", type=int, default=DEFAULT_REPS, help="measured reps per cell (min 3)")
        p.add_argument("--fixtures", default=None, help="path to code-screen-v1.json")

    p_plan = sub.add_parser("plan", help="print the cells and commands; runs nothing")
    add_common(p_plan)
    p_plan.set_defaults(func=cmd_plan)

    p_verify = sub.add_parser("verify-models", help="SHA-256 check the model files")
    p_verify.add_argument("--models-dir", required=True)
    p_verify.add_argument("--models", default=None)
    p_verify.set_defaults(func=cmd_verify)

    p_run = sub.add_parser("run", help="run the cells and write receipts")
    add_common(p_run)
    p_run.add_argument("--models-dir", required=True)
    p_run.add_argument("--llama-dir", default=None)
    p_run.add_argument("--out", default="receipts")
    p_run.add_argument("--mlx-model", action="append", default=[], metavar="ID=DIR")
    p_run.add_argument("--oxe-bin", default=None, metavar="PATH",
                       help="installed oxe binary for the oxe-cpu arm (never downloaded or built here)")
    p_run.set_defaults(func=cmd_run)

    p_self = sub.add_parser("selfcheck", help="public-safety scan of receipt files")
    p_self.add_argument("files", nargs="+")
    p_self.set_defaults(func=cmd_selfcheck)
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except UsageError as exc:
        print("error: %s" % exc, file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
