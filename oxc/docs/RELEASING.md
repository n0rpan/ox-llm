# Development and releases

Development happens in the public [ox-llm repository](https://github.com/oxstack/ox-llm).
Changes go through small pull requests against protected `main`, with the required hosted
checks green and the appropriate review. See [CONTRIBUTING](https://github.com/oxstack/ox-llm/blob/main/CONTRIBUTING.md). There is no private-to-public snapshot promotion step.

A change that reports performance or quality results needs a linked, scrubbed receipt and
its measurement conditions. Benchmark results are historical evidence; preserve them when
a newer result supersedes them. Keep secret, SDK-material, product-isolation and receipt
checks intact. Hardware qualification is separate from model-free CI and is reported by
the maintainer when needed.

The maintainer decides when to release, creates the version tag, and approves the protected
`release` environment. The [release workflow](https://github.com/oxstack/ox-llm/blob/main/.github/workflows/release.yml) builds
from that tagged source on GitHub-hosted runners, runs the model-free checks, packages the
artifacts and publishes checksums and build attestations using the job token. Contributors
and coding agents do not create tags, releases or publish artifacts.

Users verify a downloaded artifact with `sha256sum -c SHA256SUMS --ignore-missing` and
`gh attestation verify FILE --repo oxstack/ox-llm --signer-workflow oxstack/ox-llm/.github/workflows/release.yml`.
Installation qualification and opt-in runtime features remain subject to their documented
gates; a green build does not establish hardware speed, memory use or long-run stability.
