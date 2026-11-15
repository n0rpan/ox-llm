# Roadmap

The next release is **v0.2.0**: the engine, the controller and oxa ship together from this repository,
with one tag and one release page. See [DECISIONS.md](DECISIONS.md) for the reasoning.

## v0.2.0

Scope and status live on GitHub, not in this file:

- [Milestone v0.2.0](https://github.com/oxstack/ox-llm/milestone/1): what ships in the release.
- [Milestone v0.2.1](https://github.com/oxstack/ox-llm/milestone/2): what follows it.
- Label `post-0.2`: work that is not on the v0.2.0 release path.

The release is one squashed public snapshot; the install gate pack runs once on the final binaries.
Release steps and the public repository name are the maintainer's call (see [DECISIONS.md](DECISIONS.md)).

## After v0.2.0

- Maintenance mode: no always-on agents. Issues come from use and from the health digest.
- Research is driven by the data, with pre-registered thresholds. Parked: NPU prefill/decode, int8/fp8
  KV, 256K contexts. Rejected paths are kept in [oxc/docs/FAILED-PATHS.md](oxc/docs/FAILED-PATHS.md).
