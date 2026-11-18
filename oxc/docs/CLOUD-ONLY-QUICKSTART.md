# Legacy v0.2.0 cloud-only quickstart

This guide applies to the Python controller shipped in v0.2.0. For the native
v0.2.1 release target, see the [native install and upgrade guide](https://github.com/oxstack/ox-llm/blob/main/docs/NATIVE-INSTALL.md).

oxc (the controller and its cloud gateway) plus oxa, with **no local engine and no model
download**. Every model is a cloud model that oxc proxies over its loopback OpenAI v1
endpoint. Nothing here needs Snapdragon hardware. Cloud models send your prompts to the
provider you choose; see [CLOUD-MODELS.md](CLOUD-MODELS.md) for the privacy rules and the
budget.

## Requirements

- Linux on x86_64 or arm64
- `python3` (with `venv`), `curl`
- A systemd user session for the default service; `--no-systemd` (step 1) needs none
- A free loopback port (default `8092`; change it with `--port`)
- A Rust toolchain (`cargo`), to build oxa
- The Anthropic SDK for the python that runs the controller, if you use `anthropic/*` models
  (step 2; the controller does not install it)
- An API key for at least one provider (OpenRouter, Anthropic or OpenAI)

## 1. Install the controller

From `oxc/` in a checkout of the [published v0.2.0 source](https://github.com/oxstack/ox-llm/tree/83fe1019d08059efc1dd6ac6932099482d55fd10).
First create a venv outside that checkout and
install the pinned Anthropic SDK (`anthropic==0.125.0`, the `cloud` extra in `pyproject.toml`) into it,
then run the installer with that venv's Python, because the installer bakes the interpreter that runs
it into the launcher and the unit:

```console
$ python3 -m venv ~/oxc/venv
$ ~/oxc/venv/bin/python -m pip install 'anthropic==0.125.0'
$ PYTHONPATH=src ~/oxc/venv/bin/python -m oxc.cloud_install --cloud-only \
    --prefix ~/oxc --config-dir ~/oxc/config --port 8092 --daily-budget 2
```

This install writes `cloud.enabled: true` into `runtime.json`; cloud is the only mode it sets up,
and no engine or model is installed. If another controller already listens on the port you chose
(check with `ss -ltn`), pick another `--port` and use the same port in steps 4 and 5.

Without the SDK in that interpreter, Anthropic models answer `503` `cloud_sdk_unavailable`;
OpenRouter and OpenAI models do not need it, so you can skip the `pip` line if you use only those.
If you ran the installer with another interpreter, re-run it with the venv's Python and `--force`
to regenerate the launcher and unit.

Add `--no-systemd` to write no service and run `~/oxc/bin/oxc-controller` in the foreground
instead. Without it, the installer writes a user unit (`oxc.service`, change it with
`--unit-name`) and runs `systemctl --user daemon-reload` and `enable --now`.

The installer writes `<config-dir>/runtime.json`, the launcher `<prefix>/bin/oxc-controller`
and a copy of the controller code under `<prefix>/share/oxc/runtime`. It uses the catalog in
`docs/cloud-models.json` of the source tree unless you pass `--models-file` (format: [CLOUD-MODELS.md](CLOUD-MODELS.md#catalog)).
An existing `runtime.json` is kept; `--force` replaces it. `--oxa-binary PATH` also copies a
built oxa to `<prefix>/bin/oxa`.

The default catalog's prices are only an estimate for the budget. Check them against your
provider, or pass your own `--models-file`.

## 2. Install the Anthropic SDK (only for `anthropic/*` models)

The installer checks whether the python that runs the controller can import `anthropic`, and
reports it as `anthropic_sdk` in its JSON output. If the catalog has Anthropic models and the
SDK is missing, it prints one line with the exact command (it installs nothing itself), which is
this, with the same python:

```console
$ ~/oxc/venv/bin/python -m pip install anthropic==0.125.0
```

(With the venv of step 1 this is already done; the hint names whichever python ran the installer.
This is the `cloud` extra of `oxc/pyproject.toml`. Use a virtual environment if your system
python is externally managed.) Without it, a chat to an `anthropic/*` model gets HTTP `503`,
code `cloud_sdk_unavailable`, "The Anthropic SDK is not installed on this host." OpenRouter and
OpenAI models need no extra package. Restart the controller after installing.

## 3. Add your keys

The installer never creates or reads the key file. Create `<config-dir>/cloud-secrets.json`
with the providers you use, in the format of [CLOUD-MODELS.md](CLOUD-MODELS.md#key-file), for example
`{"openrouter": {"api_key": "<your key>"}}`. Then restrict it (a new file is usually `0644`, which is refused):

```console
$ chmod 600 ~/oxc/config/cloud-secrets.json
```

The file must be a regular file you own with mode `0600`. Never paste a real key into an issue,
a log or this repository.

`--daily-budget` (USD, default 1.0) is stored as `cloud.daily_budget_usd` in `runtime.json`.
A request that would exceed it gets `429` `budget_exceeded`.

## 4. Check it

```console
$ curl -s http://127.0.0.1:8092/v1/models
```

Every row is a cloud model (`"remote": true`). The status at `/_local/status` reports
`"engine_mode": "none"`.

A request to a provider whose key is missing or whose file is refused fails cleanly with HTTP
`503`, code `cloud_credentials_unavailable`, and a message like "Cloud credentials are not
configured: the configured path is a symbolic link." The suffix states the reason (a symbolic
link or non-regular file, the wrong owner, group/other permissions enabled, or a file too large)
without ever showing the key. Fix the file and retry; no restart is needed, the file is read per
request. oxa shows the controller's message ("the cloud gateway refused the request
(cloud_credentials_unavailable): ...").

## 5. Build and run oxa

Run this from the `oxa/` directory of the checkout (from `oxc/`, that is `cd ../oxa`):

```console
$ cargo build --release --locked
$ export OXA_ENGINE_URL=http://127.0.0.1:8092
$ cd ~/my-project && /path/to/checkout/oxa/target/release/oxa --model haiku
```

The binary is `oxa/target/release/oxa` (it is not copied anywhere; `--oxa-binary` in step 1 copies it
to `<prefix>/bin/oxa` if you pass it).

`OXA_ENGINE_URL` is the controller's address and defaults to `http://127.0.0.1:8092`, so it
is only needed for another port. oxa picks the first listed model when you give no `--model`.
`--model` takes a name pattern such as `haiku`, or a full id such as `anthropic/claude-haiku-5-5`;
both select the same model. The footer shows `cloud · <provider>` and oxa sends no local-engine
preload. `oxa --list-models` lists the cloud models from the controller.

## Uninstall

`oxc uninstall` does not know this install. Remove it by hand:

```console
$ systemctl --user disable --now oxc.service    # skip with --no-systemd
$ rm ~/.config/systemd/user/oxc.service && systemctl --user daemon-reload
$ rm -r ~/oxc                                   # prefix, venv, config and secrets
```

Use your own `--unit-name`, `--unit-dir`, `--prefix` and `--config-dir` if you changed them.
