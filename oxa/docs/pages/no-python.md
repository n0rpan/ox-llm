# Run oxa without Python

oxa can run Anthropic cloud sessions through its in-process Rust gateway, without
starting Python or contacting the Python controller. This is opt-in, not yet a
replacement for every controller command or provider.
## Enable the native cloud path
Merge this into `~/.oxa/agent/settings.json` (user settings only):
```json
{
  "oxa": {
    "engine": { "inProcessCloud": true },
    "cloud": {
      "daily_budget_usd": 10,
      "models": {
        "example-cloud": {
          "provider": "anthropic",
          "upstream_model": "replace-with-your-model",
          "context_window": 128000,
          "max_output": 8192,
          "pricing": {
            "input_per_million": 2,
            "output_per_million": 3,
            "cache_read_per_million": 0.2,
            "cache_write_per_million": 2.5
          }
        }
      }
    }
  }
}
```

Replace the illustrative model, limits and prices with the provider's current
values. `oxa --list-models` lists configured routes without reading credentials or
contacting a provider. Select one with `oxa --model example-cloud`.
The default transport remains unchanged when `inProcessCloud` is false.
Offline mode refuses the native cloud path; project settings cannot enable it.

Keys stay in `~/.config/oxc/llm/cloud-secrets.json`, outside the checkout, owned by
this user with mode `0600`. The Anthropic entry is an object with an `api_key`
string under `anthropic`; never put the key in settings. It is read per request.
Private usage rows live in `<agent directory>/cloud-ledger/`, normally
`~/.oxa/agent/cloud-ledger/` (`OXA_AGENT_DIR` changes the agent directory).
Missing prices or a daily cap refuse requests before credentials or provider I/O.

## What still needs Python
Browser subscription sign-in (`oxc login chatgpt`) still uses the Python command;
it does not enable a native ChatGPT route here. Local engine management also
still needs the Python controller until the native local-engine slice 5 lands.

## Check by hand, without real credentials

Use a throwaway HOME and cleared environment, fake credentials and a loopback
Anthropic SSE mock; set the model's `base_url` to that mock. Put only marker-writing
`python` and `python3` shims on the child's PATH and run the absolute oxa binary
with `--model example-cloud --no-session --no-tools -p 'Synthetic proof'` under
`strace -f -e trace=execve,connect`. Calibrate both shims and a controller trap
listener first. Require the mock answer and one settled usage row, no marker or
Python exec, and no connect to the controller trap or port 8092. Allow only the
mock connection; a successful exit alone does not prove the native path was used.
