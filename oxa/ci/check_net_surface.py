#!/usr/bin/env python3
"""P2 (wave H): the network surface of the oxa binary stays where it is reviewed.

Rules (OXA-LOCAL-FIRST sec. 2, "Network surface"):
1. The socket / HTTP-client identifiers in IDENTS may appear in the source of the oxa binary
   only in the files of ALLOWED (each with its reason). "The source of the oxa binary" =
   every `<crate>/src/**/*.rs` of the workspace crates in the normal-dependency closure of
   `oxa-cli` (the crate that builds `oxa`), plus `oxa-cli` itself. Crates outside that closure
   (the test-only fake engine and the PARITY suite) and `tests/` directories are never linked
   into the binary; rule 3 proves the first part.
2. Only `oxa-mcp` and `oxa-web` may depend on an HTTP / TLS client crate (CLIENT_CRATES), in
   any dependency section that is linked (normal, build, target-specific). Inside `oxa-web`
   only its one connect site (`oxa-web/src/brave.rs`, in ALLOWED) may name the client.
3. `oxa-fake` and `oxa-parity` are not in the closure of rule 1.
Matching is on whole words over the whole file text (comments included): nothing is
stripped, so no spelling of an identifier can slip through a comment-stripping step.

Usage: python3 ci/check_net_surface.py [--root DIR] [--self-test]
  --self-test: run the check on a clean planted tree (must pass) and on trees with one planted
  violation per rule (each must fail), then on the real tree.
Exit 0 = clean, 1 = violations (listed), 2 = self-test failure.
"""
import argparse, os, re, shutil, sys, tempfile, tomllib

IDENTS = ["TcpStream", "TcpListener", "UdpSocket", "to_socket_addrs", "ureq"]
CLIENT_CRATES = ["ureq", "rustls", "webpki-roots", "native-tls", "reqwest", "hyper", "curl", "isahc", "attohttpc", "minreq"]
# path -> reason. A new entry needs a reason a reviewer can check.
ALLOWED = {
    "oxa-engine/src/http.rs": "the engine client: plain HTTP to the configured engine (loopback unless oxa.engine.allowRemote)",
    "oxa-engine/src/lib.rs": "crate docs of the engine client (text only; names the type the client is built on)",
    "oxa-mcp/src/http.rs": "MCP streamable HTTP transport: only servers the user put in mcp.json",
    "oxa-mcp/src/oauth.rs": "MCP OAuth: requests to the server's authorization server, loopback callback listener",
    "oxa-web/src/brave.rs": "web_search provider: user-configured key only, off when offline",
}
CLIENT_OK = {
    "oxa-mcp": "the TLS stack of the binary, for MCP servers the user configured",
    "oxa-web": "the same ureq/TLS stack for the web_search provider: user-configured key only, off when offline",
}
TEST_ONLY = ["oxa-fake", "oxa-parity"]
BINARY_CRATE = "oxa-cli"
LINKED_SECTIONS = ["dependencies", "build-dependencies"]


def crates(root):
    """name -> (dir, manifest dict) for every workspace member."""
    ws = tomllib.load(open(os.path.join(root, "Cargo.toml"), "rb"))
    out = {}
    for member in ws.get("workspace", {}).get("members", []):
        path = os.path.join(root, member, "Cargo.toml")
        if os.path.exists(path):
            m = tomllib.load(open(path, "rb"))
            out[m["package"]["name"]] = (member, m)
    return out


def linked_deps(manifest):
    """Every dependency name in a linked section (also `[target.*.dependencies]`)."""
    names = set()
    for sec in LINKED_SECTIONS:
        names.update(manifest.get(sec, {}).keys())
    for tgt in manifest.get("target", {}).values():
        for sec in LINKED_SECTIONS:
            names.update(tgt.get(sec, {}).keys())
    return names


def closure(all_crates, start):
    seen, todo = set(), [start]
    while todo:
        c = todo.pop()
        if c in seen or c not in all_crates:
            continue
        seen.add(c)
        todo.extend(d for d in linked_deps(all_crates[c][1]) if d in all_crates)
    return seen


def check(root):
    errors = []
    all_crates = crates(root)
    if BINARY_CRATE not in all_crates:
        return [f"{BINARY_CRATE} is not a workspace member"]
    binary = closure(all_crates, BINARY_CRATE)
    for t in TEST_ONLY:
        if t in binary:
            errors.append(f"rule 3: test-only crate {t} is linked into the binary")
    for name, (member, manifest) in sorted(all_crates.items()):
        for dep in sorted(linked_deps(manifest)):
            if dep in CLIENT_CRATES and name not in CLIENT_OK:
                errors.append(f"rule 2: {member}/Cargo.toml depends on {dep}; only {', '.join(CLIENT_OK)} may")
    word = re.compile(r"\b(" + "|".join(map(re.escape, IDENTS)) + r")\b")
    for name in sorted(binary):
        member = all_crates[name][0]
        src = os.path.join(root, member, "src")
        for dirpath, _, files in os.walk(src):
            for f in sorted(files):
                if not f.endswith(".rs"):
                    continue
                path = os.path.join(dirpath, f)
                rel = os.path.relpath(path, root).replace(os.sep, "/")
                if rel in ALLOWED:
                    continue
                text = open(path, encoding="utf-8", errors="replace").read()
                for n, line in enumerate(text.splitlines(), 1):
                    for m in word.finditer(line):
                        errors.append(f"rule 1: {rel}:{n}: `{m.group(1)}` outside the reviewed network files")
    for rel in ALLOWED:
        if not os.path.exists(os.path.join(root, rel)):
            errors.append(f"stale allow-list entry: {rel} does not exist")
    return errors


def plant(base):
    """A minimal clean tree: a binary crate, an MCP crate with a client, a test-only fake."""
    def w(rel, text):
        p = os.path.join(base, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        open(p, "w").write(text)
    w("Cargo.toml", '[workspace]\nmembers = ["oxa-cli", "oxa-mcp", "oxa-web", "oxa-engine", "oxa-fake", "oxa-parity"]\n')
    w("oxa-cli/Cargo.toml", '[package]\nname = "oxa-cli"\n[dependencies]\noxa-mcp = {}\noxa-web = {}\noxa-engine = {}\n[dev-dependencies]\noxa-fake = {}\n')
    w("oxa-web/Cargo.toml", '[package]\nname = "oxa-web"\n[dependencies]\nureq = {}\n')
    w("oxa-web/src/brave.rs", "fn get(a: &ureq::Agent) {}\n")
    w("oxa-web/src/tool.rs", "pub struct WebSearchTool;\n")
    w("oxa-cli/src/main.rs", "fn main() {}\n")
    w("oxa-mcp/Cargo.toml", '[package]\nname = "oxa-mcp"\n[dependencies]\nureq = {}\n')
    w("oxa-mcp/src/http.rs", "use ureq::Agent;\n")
    w("oxa-mcp/src/oauth.rs", "use std::net::TcpListener;\n")
    w("oxa-engine/Cargo.toml", '[package]\nname = "oxa-engine"\n')
    w("oxa-engine/src/http.rs", "use std::net::TcpStream;\n")
    w("oxa-engine/src/lib.rs", "//! over TcpStream\n")
    w("oxa-fake/Cargo.toml", '[package]\nname = "oxa-fake"\n')
    w("oxa-fake/src/server.rs", "use std::net::TcpListener;\n")
    w("oxa-parity/Cargo.toml", '[package]\nname = "oxa-parity"\n[dependencies]\noxa-fake = {}\n')
    w("oxa-parity/src/lib.rs", "\n")
    w("oxa-cli/tests/t.rs", "use std::net::TcpStream;\n")
    return w


def self_test(real_root):
    plants = {
        "socket in a product file": ("oxa-cli/src/phone.rs", "fn f() { let _ = std::net::TcpStream::connect(\"x:1\"); }\n"),
        "udp in a product file": ("oxa-engine/src/info.rs", "use std::net::UdpSocket;\n"),
        "resolver in a product file": ("oxa-mcp/src/manager.rs", "fn f(h: &str) { let _ = (h, 1).to_socket_addrs(); }\n"),
        "client crate outside oxa-mcp and oxa-web": ("oxa-engine/Cargo.toml", '[package]\nname = "oxa-engine"\n[dependencies]\nureq = {}\n'),
        "client crate as a target dependency": ("oxa-engine/Cargo.toml", '[package]\nname = "oxa-engine"\n[target.\'cfg(unix)\'.dependencies]\nrustls = {}\n'),
        "test-only crate linked": ("oxa-cli/Cargo.toml", '[package]\nname = "oxa-cli"\n[dependencies]\noxa-mcp = {}\noxa-web = {}\noxa-engine = {}\noxa-fake = {}\n'),
        "client outside the web_search connect site": ("oxa-web/src/tool.rs", "fn f() { let _ = ureq::get(\"https://example.com\").call(); }\n"),
        "socket in oxa-web outside the connect site": ("oxa-web/src/hooks.rs", "use std::net::TcpStream;\n"),
    }
    ok = True
    with tempfile.TemporaryDirectory() as tmp:
        clean = os.path.join(tmp, "clean")
        plant(clean)
        errs = check(clean)
        if errs:
            print("self-test: the clean planted tree failed:", *errs, sep="\n  ")
            ok = False
        for label, (rel, text) in plants.items():
            d = os.path.join(tmp, re.sub(r"\W+", "_", label))
            shutil.copytree(clean, d)
            p = os.path.join(d, rel)
            os.makedirs(os.path.dirname(p), exist_ok=True)
            open(p, "w").write(text)
            errs = check(d)
            if not errs:
                print(f"self-test: planted violation not caught: {label}")
                ok = False
            else:
                print(f"self-test: caught ({label}): {errs[0]}")
    return ok


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    ap.add_argument("--self-test", action="store_true")
    a = ap.parse_args()
    if a.self_test and not self_test(a.root):
        return 2
    errors = check(a.root)
    if errors:
        print("net surface: violations:", *errors, sep="\n  ")
        return 1
    print("net surface: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
