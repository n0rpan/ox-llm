This branch is a throwaway integration rehearsal. Never merge it or open it as a product PR.

The production CLI path currently supports the native Anthropic gateway, selected by user-only `oxa.engine.inProcessCloud`. OpenAI/ChatGPT production composition and the local engine selector are still unavailable. The OpenAI client and local typed transport are separately exercised components.

For a reusable synthetic CLI/TUI fixture, build from `oxa/`:

```sh
cargo build --profile ci -j 2 -p oxa-cli --bin oxa --example rehearsal_mock
./target/ci/examples/rehearsal_mock /var/tmp/ox-ux-native
```

Choose a new absolute scratch root outside any repository. Keep the fixture process alive. It creates a private synthetic HOME, project, fixed-path synthetic credential file, user-only settings, marker-writing Python shims, an ephemeral TCP controller trap and a launcher. It serves up to 128 static text replies through the shared loopback Anthropic mock. Responses do not interpret the prompt or execute tools; this fixture is for black-box UI behavior, not model quality or overall E2E acceptance.

From another terminal, launch the real binary in the isolated project:

```sh
/var/tmp/ox-ux-native/run-oxa
/var/tmp/ox-ux-native/run-oxa --no-session --no-tools -p 'Synthetic smoke'
```

`run-oxa` clears inherited environment, selects only the Python marker shims in PATH, and points the old HTTP provider URL at the owned controller trap. Neither `python-marker` nor `controller-marker` may appear under the fixture root. The trap records bare TCP connections, even without an HTTP request. Native usage rows are under `home/.oxa/agent/cloud-ledger`; all inputs and usage are synthetic. Do not substitute a real key or sign in.

Optional syscall evidence (with a locally available strace) captures only exec/connect metadata:

```sh
strace -f -e trace=execve,connect -o /var/tmp/ox-ux-native/traces/tui.trace /var/tmp/ox-ux-native/run-oxa
```

Check for Python executables, fixed controller port 8092, and the ephemeral controller port in the generated run-oxa script. Do not trace reads/writes or credential bytes. The integration lane's separate mechanical probe checks the positive tool loop, exact paid ledger/cache costs, budget refusal and these execution/connection invariants.

Press Enter in the fixture terminal to stop and join its owned mock/trap threads. Scratch files remain for inspection; remove only your own fixture root when done. No service, user configuration or installed package is changed.
