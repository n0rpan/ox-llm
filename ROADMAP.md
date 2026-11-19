# Roadmap

The next release is **v0.2.1**: the Rust-only release. It ships the native `oxa` and `oxe-server`
binaries with local models and the rewritten docs. There is no Python runtime in the product.
After it, **v0.2.2** follows. See [DECISIONS.md](DECISIONS.md) for the reasoning.

## v0.2.1 and v0.2.2

Scope and status live on GitHub, not in this file:

- [Milestones](https://github.com/oxstack/ox-llm/milestones): what ships in v0.2.1 and v0.2.2.
- Label `post-0.2`: work that is not on the v0.2.1 release path.

Release steps and the public repository name are the maintainer's call (see [DECISIONS.md](DECISIONS.md)).
The install boundary is in [docs/NATIVE-INSTALL.md](docs/NATIVE-INSTALL.md).

## After v0.2.2

- Maintenance mode: no always-on agents. Issues come from use and from the health digest.
- Research is driven by the data, with pre-registered thresholds. Parked: NPU prefill/decode, int8/fp8
  KV, 256K contexts.
