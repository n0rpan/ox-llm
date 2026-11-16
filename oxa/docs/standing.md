# Standing rules and owner decisions

`oxa_work::standing` stores the owner's rules, holds and questions for one repository. It
never opens runner task state, starts a model, writes a task receipt or creates a background
thread. The runner owns command/action wiring; the lead owns cards and tool registration.

## Protected root and revisions

The trusted frontend supplies a fixed per-repository root in owner configuration, outside
workspace and writable Git binds, and the repository's `Facts` instance. Neither path comes
from tool JSON. `Standing::initialize(root, Authority::Owner, facts)` is explicit. `open`
never turns missing, partial, corrupt, unsafe or inconsistent enforcement data into empty rules.
The existing private directory/file/no-follow primitives are reused.

`read()` returns the current revision, entries, asks and explicit history-truncation flag.
`read_facts()` returns that view paired with a shared situation checked at the same standing
revision. Cards use this pair rather than joining independently read versions.
`live_rules()` excludes superseded/retired rules and released constraints; `open_asks()`
excludes answered questions. Active rules and unanswered asks share a 128-item limit. Up to
16 terminal records are retained; they may shrink to fit the 256 KiB read view. Pending
metadata is capped at 1 MiB. Active data is never dropped to make room.

Every mutation supplies the document revision it read. A stale revision refuses without
changing the store. A live collision of kind/scope requires its exact `supersedes` ID.
IDs are derived from the checked revision, not from model text or time.

## Authority, values and checked application

`Authority` is supplied separately by the trusted interface. It is absent from `Rule`,
`Question` and `AnswerRequest`. A model cannot claim owner authority by adding a JSON field
or quoting a message. The frontend checks the source reference and binds owner operations to
actual owner input. `said` preserves those words; `said_by` attributes them separately from
inherited hold protection. Sources name a command or saved session entry.

Kinds are goal, priority, freeze, hold, budget, model, placement, gate, eligible, decided,
reserve and note. Text policy values remain explicit data. Budgets use integer USD micros
per day; the frontend presents/parses ordinary USD amounts. Gates contain an ordered T1/T2/T3
tier and a closed set of additional evidence requirements. They supplement existing policy
floors. Reserve values use the closed public/paid/credentials/destructive/desktop classes.

Only the owner raises a budget, establishes an initial positive budget/gate, weakens a gate,
changes reserves, or releases an owner constraint. Lead reductions and conjunctive gate
strengthening are allowed. An owner hold keeps its protection through a lead replacement;
replacing it cannot turn it into a releasable lead hold or change its release condition.

Storing a rule gives it `Stored` state. Gateway caps, model profiles, placement, eligibility,
priority and merge-policy application belong to their existing runner/runtime adapters.
They consume the locked read view; this store does not invent another policy engine.
`verify` accepts only a frontend-owned checker reporting the exact current revision, scope,
actual value and evidence reference. Mismatches leave the rule Stored. An accepted proposal
also starts Stored; it does not claim a runtime was reconfigured.

Reminders require owner context and a checked source. Counts, the latest source/time and
incident observations persist. After two reminders a live note reports that a conversion
should be offered. It is never converted automatically. An explicit owner decision may
supersede that note with an enforced kind.

## One action guard

`with_guard(action, subject, callback)` reloads and establishes the current policy's durability
under its stable lock. Subject identity is resolved by the trusted frontend; a missing PR
means checked absence, and a known PR must always be supplied. Admission can precede a PR.
Approve/enqueue/update require a positive PR identity. There is no head-dependent hold key.

A live hold refuses admit, push, approve, enqueue and update. Freeze refuses approve/enqueue.
Both apply immediately while Stored. `is_held(pr)` is a read for presentation; a preceding
read never authorizes an effect. Callback receives `&Read` with the exact guarded revision,
including the other policy values for their owning adapters, and runs only after guard success.
It must not recursively read/write this Standing instance.

The lock covers one effect and its result, never an entire worker, loop or batch. The caller
owns bounded execution/cancellation, side-effect idempotency and its receipt containing the
guarded revision. It must invoke another guard before the next effect. The store's guard
does not override existing tool, sandbox, gateway or publishing policy.

## Questions and proposals

The existing ask tool's payload is reused: question, 2–4 unique label/detail options and an
exact-label recommendation. Stored asks add ID, question revision, scope, time/source,
optional meanwhile plan and an optional owner answer. Unknown meanwhile remains `None`.
The existing UI/journal adapter is not replaced by a second tool in this module.

`ask` records the question. A recommendation never answers it or grants permission.
`answer` requires owner context, the current document revision and the exact question
revision. Option labels are checked; free text is stored as free text. Answers preserve the
owner's actual words/source. A generic answer labelled "Release" cannot release a hold.

`propose` binds an exact typed Rule to an ask and an accept label. Cards **must render that
rule's actual kind, scope and value**, not only model-written labels/details. Scope must match
the question. Only selecting that label through the owner interface applies the bound rule;
it cannot substitute a new value. A changed/superseded target refuses without closing the ask.
Free text has no implicit accept meaning. Other answers close only the question. This changes
standing data, never performs a GitHub/paid/tool side effect.

A named `PrMerged(N)` condition can release a hold/freeze without asking for a second decision
already authorized by the owner. `release_when` uses a frontend-owned checker and requires the
exact matching `PrMerged` Event retained in this same Facts store. The runner records that
verified merge first. Wrong PRs, invented events and unavailable observations refuse; prose,
a successful worker and a changed head are insufficient. The resulting release retains its
source words and records checked merge evidence. Only the separate merge fact is landed work.

## Durable fact handoff

One fixed `standing` emitter uses the document revision. `StandingChanged` projects revision,
goal, one ask transition and an optional incident. Full rule/option/answer data stays in this
protected file. It does not emit two different facts under one source/revision.

Event pages honor both the requested count (at most 64) and the serialized byte limit.
Large valid observations return smaller pages with `next` at the last included event;
readers keep advancing through retained history and receive explicit retention gaps.

Under Standing → Facts lock order, the facts module validates capacity/semantics and durably
reserves an unpublished observation **before** the callback saves the pending policy. Other
appenders cannot consume its projection capacity, even if the process dies in the callback.
Only after the policy is durable is the reservation made replayable and its fact appended;
the current policy is committed last. Ordinary facts keep their existing append path.

Recovery syncs a visible prepared policy, completes its reserved fact once, commits the policy
and establishes durability before another action. If the durable policy has no pending
mutation, a matching unpublished next-revision reservation has no prepared data: recovery
discards only that reservation, preserving the current rules, revision and committed facts.
A rejected initial projection leaves no policy mutation. A failed fact reservation therefore
cannot strand an ask; the owner can still answer an existing ask to free capacity.

Recover standing before replaying runner callbacks that append facts. A facts-only read reports
the last committed sequence; it never publishes an unfinished standing preparation. The
standing read/guard APIs perform this recovery under their stable lock. No background worker
or task-state writer is added, and a failed observation stops before an effect.

The runner owns `oxa work rule|hold|ask`, actual action guards and local command proofs. The
lead joins detailed asks by ID from `Read` with the small shared situation. These adapters
must be wired and proven before claiming the complete #629 L2 user flow is delivered.
