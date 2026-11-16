# OpenAI Responses client

Work in progress under the second-provider contract on issue #331. The client
belongs in `src/openai/client.rs`; rust-cloud owns runtime, gateway, accounting
and the durable protocol seam. This change does not enable a provider route.

The transport must use the existing hardened HTTP policy (no proxy or redirects,
bounded bodies, cancellation-aware streaming), support API-key Responses and the
adapted subscription request, and refresh through the private token-file helper
with a firm HTTP deadline while its refresh lock is held. Credential text must
never enter errors or Debug. Usage stays private until the caller settles it.

Acceptance uses loopback fixtures only, including three tools with done-only and
partial arguments, empty completed output, error results, refresh, and explicit
persisted replay items continued by a new process. Missing replay state retains
the translator's typed refusal. No real keys, sign-in or provider calls.
