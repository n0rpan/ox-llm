# Local engine cache evidence (#2255)

This note records what the opt-in foreground capture can say about the local engine today,
what one saved observation shows, and the one missing engine hook. It changes no engine
numerics, no defaults and no cache policy, and it attributes no cause.

## What the capture reports now

`oxa --wire-evidence` keeps its wire comparison at oxa's transport handoff. Each returned row
also carries an `engine` block taken from the reply's own `timings`:

| field | source |
| --- | --- |
| `evaluated_prefill_tokens` | `timings.prompt_n` |
| `prefill_ms` | `timings.prompt_ms` |
| `checkpoint_position` | `timings.snap_pos` |
| `checkpoint_known` | whether the reply carried `snap_pos` at all |
| `cache_source` | `unavailable` |

`cache_source` is `unavailable` because no local engine endpoint reports it. A reply without
timings (a remote provider) has no `engine` block, not a zero one. A
missing usage or timing value stays absent. None of this is an allocation or a comparison.

## Missing hook

The engine's `cache.rs::PrefixCache::prepare` knows where a reused prefix came from
(`last_src`: `none`, `live`, `checkpoint`, `parked`, `parked-checkpoint`, and the worker's
`store`, `session`, `state`, `preload`) and the generation loop keeps `cache_src` (`generate.rs`,
`Run::cache_src`). Neither reaches a client:

- the OpenAI reply's `timings` (`api.rs` `timings_base`) has `cache_n` and `prompt_n` but no
  source;
- the session ledger line (`generate.rs`, `cache_src`) is a server-side file, not per-request
  events, and the reply path does not forward it.

The narrow next slice is one additive optional string in that reply's `timings` (for example
`cache_src`), written only for a request that asked for it with an owner-controlled engine
launch option, so the off path writes no new bytes and the engine sends no diagnostic header
to any provider. `oxa-engine` then parses it into `oxa_proto::Timings` as
`cache_src: Option<&'static str>` (unknown stays unknown), and the wire evidence copies it into
the `engine` block instead of `unavailable`. It never makes a `CacheMiss` cause.

Neither side of this is present on `main`: the engine does not emit the field and
`oxa-proto` has no place to carry it. That crossing (engine reply shape plus gateway parsing)
is the slice this PR does not take.

## Saved observation (not measured)

Run on a lab host with the installed dev build (`oxa 0.2.2-dev.99+70dbf83`), read-only, on a
saved session of the native local engine, `oxa cache <path>`:

- one session, 12 plain chats, no recorded boundary other than the first
  `thinking_level_change`; frozen system text and tool list equal between each saved pair;
- successive saved chats grow by roughly 40 to a few hundred input tokens;
- adjacent `cached` values mostly track the preceding saved input, some far below it (for
  example request 7 of one session dropped to 16384 against 28955 preceding input).

These are saved usage counts. They do not show which request was reused, which state served it
or why a count fell short; the command says so itself. Tokens-per-millisecond of the capture
were not measured here: the cost comparison is the hosted, script-free
`wire fixture (nine chats, capture=...)` line that the PTY test prints.

## Interpretation

`cached` far below the preceding input is not a `cache miss cause`. The engine's own
graded checkpoints and last-token rule can legitimately stop below the end of the last message,
so only the engine's `cache_src` plus its checkpoint position can separate "the live state was
not used" from "the state was used up to a checkpoint". Until then the report leaves the source
unknown on purpose.