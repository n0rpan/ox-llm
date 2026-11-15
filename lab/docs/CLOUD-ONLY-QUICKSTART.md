# Cloud-only quickstart

oxc (the controller and its cloud gateway) plus oxa, with **no local engine and no model
download**. Every model is a cloud model that oxc proxies over its loopback OpenAI v1
endpoint. Nothing here needs Snapdragon hardware. Cloud models send your prompts to the
provider you choose; see [CLOUD-MODELS.md](CLOUD-MODELS.md) for the privacy rules and the
budget.

## Requirements

- Linux on x86_64 or arm64
- `python3`
- A Rust toolchain, to build oxa
- The Anthropic SDK for the python that runs the controller, if you use `anthropic/*` models
  (step 2; the controller does not install it)
- An API key for at least one provider (OpenRouter, Anthropic or OpenAI)

## 1. Install the controller

From `lab/` in a checkout of this repository. First create a venv outside the checkout and
install the pinned Anthropic SDK (`anthropic==0.125.0`, the `cloud` extra in `pyproject.toml`) into it,
then run the installer with that venv's Python, because the installer bakes the interpreter that runs
it into the launcher and the unit:

```console
$ python3 -m venv ~/oxc/venv
$ ~/oxc/venv/bin/python -m pip install 'anthropic==0.125.0'
$ PYTHONPATH=src ~/oxc/venv/bin/python -m snapdragon_ai_lab.cloud_install --cloud-only \
    --prefix ~/oxc --config-dir ~/oxc/config --port 8092 --daily-budget 2
```

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
$ python3 -m pip install anthropic==0.125.0
```

(This is the `cloud` extra of `lab/pyproject.toml`. Use a virtual environment if your system
python is externally managed.) Without it, a chat to an `anthropic/*` model gets HTTP `503`,
code `cloud_sdk_unavailable`, "The Anthropic SDK is not installed on this host." OpenRouter and
OpenAI models need no extra package. Restart the controller after installing.

## 3. Add your keys

The installer never creates or reads the key file. Create `<config-dir>/cloud-secrets.json`
with the providers you use, in the format of [CLOUD-MODELS.md](CLOUD-MODELS.md#key-file), then
restrict it:

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

A request to a provider whose key is missing or whose file has the wrong mode fails cleanly
with HTTP `503`, code `cloud_credentials_unavailable` and the message "Cloud credentials are
not configured." Fix the file and retry; no restart is needed, the file is read per request. oxa shows the
controller's message ("the cloud gateway refused the request (cloud_credentials_unavailable): ...").

## 5. Build and run oxa

```console
$ cd ../oxa && cargo build --release --locked
$ export OXA_ENGINE_URL=http://127.0.0.1:8092
$ cd ~/my-project && /path/to/oxa/target/release/oxa --model haiku
```

`OXA_ENGINE_URL` is the controller's address and defaults to `http://127.0.0.1:8092`, so it
is only needed for another port. oxa picks the first listed model when you give no `--model`. `--model` takes a name pattern
such as `haiku` (a full id with a provider prefix, like `anthropic/...`, is currently refused);
the footer shows `cloud · <provider>` and oxa sends no local-engine preload. `oxa --list-models`
lists the models.

## Uninstall

`oxc uninstall` does not know this install. Remove it by hand:

```console
$ systemctl --user disable --now oxc.service    # skip with --no-systemd
$ rm ~/.config/systemd/user/oxc.service && systemctl --user daemon-reload
$ rm -r ~/oxc                                   # prefix, venv, config and secrets
```

Use your own `--unit-name`, `--unit-dir`, `--prefix` and `--config-dir` if you changed them.
