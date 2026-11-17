# Opt-in battery decode team (N9)

Status: design and fixture contract only; tracking v0.2.2, label `after-0.2.1`.
No implementation default, no v0.2.1 change, no device access and no measurement. The
opt-in described here is implemented as a pure decision plus launch wiring so the fixture
contract below is executable; it changes nothing unless a user declares a smaller team and the
machine is on battery. The owner power row that could ever pick a default is proposed for
v0.2.3 and is a separate, pre-registered gate.

## Where it sits

The selector is a step in the existing local-engine launch/configuration boundary, not a new
selector layer: `oxc-local`'s `launch_profile` resolver (already the only place that turns the
user-only `oxa.local` settings into the child's literal words, used by the resident local
factory in `configured_engine`). There is no new CLI flag (`oxa-cli/src/args.rs` is untouched),
no new protocol field (`oxa-proto/src/engine.rs` is untouched), and no generic settings layer:
one new user-only per-model value, `oxa.local.models.<alias>.battery_decode_team`, is the whole
opt-in. N2 (placement of local-engine controls) and N6 (team selection) keep their own work;
this policy only rewrites the `--oxe-cpus` word that the installed arguments already set, so
there is still exactly one decode-team selector in the product.

The engine needs no change either: `engine/crates/server/src/main.rs` already accepts
`--oxe-cpus LIST` and `engine/crates/sched/src/lib.rs` already clamps a list to the process
mask and to existing CPUs. `--oxe-cpus` is the only word the policy may replace.

## Decision contract

`oxc_local::battery::decide` is pure and every input is injected:

| input | meaning |
|---|---|
| `power` | `Ac`, `Charging`, `Discharging` or `Unknown` from a read-only source |
| `installed` | the decode team the installed arguments selected (already qualified) |
| `declared` | the opt-in smaller team; `None` = setting absent, policy off |
| `explicit` | a user-supplied argument list chooses a CPU team different from the installed one |
| `allowed` | CPUs this process may run on; `None` = the mask could not be read |

Rules, in order:

1. No `battery_decode_team` → keep the installed team. Nothing reads a power source.
2. A user-supplied `arguments` list carries a CPU team that differs from the installed team →
   keep the user's choice. (The resident factory's `oxa.local` subtree is user-only and feeds
   both the installed layout and the per-model values, so an identical list is not an override
   and the opt-in applies.)
3. `Ac`, `Charging` or `Unknown` → keep the installed team.
4. The declared team is not a non-empty, strict subset of the effective team, or the
   effective argument list carries no single separate `--oxe-cpus LIST` (absent, repeated,
   `=`-spelled or unparsable) → refuse the configuration at discovery. Fail closed, never guess.
5. Mask unknown, or no declared CPU is usable → keep the installed team.
6. Otherwise → launch with the declared CPUs that the mask allows, in the declared order.

`Unknown` is deliberately conservative in the direction of "no change": a missing tree, an
unreadable entry, a malformed value, a driver status this policy does not know, or a battery
whose `scope` is not `System` (a mouse's battery is not the machine's) are all `Unknown`, and
the installed team is kept. A desktop with no battery is `Unknown` too.

The declared team is user- or owner-supplied, so no thread count is invented here: there is no
built-in "six threads", no hard-coded ratio and no default team. The policy is opt-in and the
default is byte-identical to today.

## Platform reader

`oxc_local::battery::read_power` reads one tree, `/sys/class/power_supply`, with `std::fs`:
`type`, `scope`, `status` and `online` values only. `allowed_mask` reads `Cpus_allowed_list`
from `/proc/self/status`. Both are read-only, take no lock, write nothing, and are called at
most once per child launch (cold load or model switch) by the launch closure. There is no
background polling framework, no sysfs/governor write, no energy estimate, no cloud or account
effect, and no change to a running team: the child's `Config` is immutable for its lifetime, so
a restart after a crash reuses the same words instead of re-deciding mid-token.

## Invariants

* The selected team is always a non-empty strict subset of the installed (already qualified)
  team; the policy cannot widen or invent a configuration.
* Only the `--oxe-cpus` value differs between an AC launch and a battery launch: model, alias,
  context, MTP, binary and environment are unchanged, so the same model and state layout are
  used and a session continues rather than being rebuilt.
* Team size does not change engine results or state. The engine side of that identity is
  already covered by the model-free test
  `mini_gguf_prefill_state_only_and_small_team` (`engine/crates/model/src/tests.rs`), which
  runs the same prefill and decode on different teams and masks and requires byte-identical
  state and logits. This design adds no numerical change; the launch fixtures additionally
  assert that a battery launch differs from the AC launch only in the CPU word.
* No efficiency, power, throughput or battery-lifetime claim is made, and none may be derived
  from this design. Zero or unavailable power readings cannot support such a claim.

## Fixtures (hosted CI only)

All fixtures are synthetic trees and responses under an isolated `TestEnv`; none reads a real
battery, a real mask, a device or a provider:

* `power_supply` trees: mains online (AC), discharging, charging, full/not-charging, a
  peripheral battery next to mains, a peripheral battery alone, a missing root, an empty root,
  a non-device entry, a permission-denied device directory, malformed `type`/`status`/`online`/
  `scope` values.
* Mask responses: a synthetic `/proc/self/status` with a list, without the line, with an empty
  or duplicate list, and with a reversed range.
* Launch wiring: the opt-in is consumed by the real child launch (`configured_engine_with`
  starts a fixture child that records its argv), the reduced team reaches that child, the
  installed team stays when the policy is off, and the power state is not read at all when off.
* The decision itself: off, explicit, AC/charging/unknown, mask restrictions, unusable and
  ambiguous declared teams, and the CPU-list grammar the engine accepts.

## Gates

* The seam (launch descriptor, mask handling and the single `--oxe-cpus` rewrite) needs
  engine-control and scheduler owner approval.
* Implementation and any future default need the pre-registered owner power row on the
  laptop: a valid power/energy source, a matched workload and throughput. Until then `Ac`,
  `Charging` and `Unknown` remain no-ops and nothing is enabled by default.
* Hard-coded team choices require the owner identity review and the pre-registered
  identity/threshold gate; helpers never access the laptop and never measure it.
* The parent tracker stays v0.2.2; the implementation/owner power row is proposed for v0.2.3.
