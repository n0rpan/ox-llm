# Lead kanban store

`oxa_work::kanban` is one owner-private board of planned work per repository: the pull-based
list the lead keeps full and workers take from. It is not the GitHub PR policy board
(`oxa_work::board`, a read-only projection of open PRs) and not a standing rule
(`oxa_work::standing`). This page describes the store slice (K1): the typed board, its
operations and undo. The dispatcher, the GitHub reconciler, the web Board page and
`oxa lead board` are later slices that call these same operations; nothing in this slice
starts a worker, makes a GitHub call or reads a setting.

## Where it lives

The trusted frontend supplies a fixed directory per repository,
`<agent_dir>/kanban/<owner>__<repo>/`, outside workspace and writable Git binds. The store
creates it `0700` and validates it with the existing private-root rules: owner uid, no
group/other access, canonical, no symlinks. It reuses the private facts-root primitives
(`facts_io::Root`) rather than a second implementation: an atomic rename for the board, an
flock for writers, and synced append/rotate for the journals.

- `board.json` (`0600`): the whole `Kanban` — schema, revision, repo, goal, per-host capacity,
  dispatch state, items and the next id. Goal and capacity live here, not in `settings.json`,
  so they are live without a restart.
- `ops.jsonl` (`0600`, append): one line per applied operation `{seq, at, actor, op, inverse}`.
  It is the undo source, the audit trail and the input for flow metrics. Rotated by size; it is
  not read on the render path.
- `archive.jsonl` (`0600`, append): items that leave the live board (a remove, or a split
  parent). Rotated by size.

A kill at any point leaves a whole `board.json`: the write is a synced temporary file renamed
into place, so a reader sees the old board or the new one, never a torn file. The board is
committed first, then the op is logged; an op whose process dies before the log append is still
live on reload (it simply cannot be undone). A torn journal tail left by a kill mid-append is
dropped on the next open. A corrupt or unreadable board is refused, never turned into an empty
board.

## Actors and authority

The actor is supplied by the trusted frontend as a separate argument to `Store::apply`; it is
never a field in the operation or read from request/tool JSON (the same rule as
`standing::Authority`). All actors are the same OS user, so this is attribution and guard
rails, not a security boundary; the boundary stays where it is (who can reach the store, file
modes).

| Actor | Established by | May |
|---|---|---|
| Owner | verified web owner, or a terminal outside an agent tool | everything |
| Lead | the configured Lead session | add, edit, split, reorder, move Backlog↔Ready, stick/unstick, link a PR, tick a manual measure with evidence. Not capacity, goal, dispatch on/off, removing owner-created items, or ranking above an owner-placed card |
| Worker(item) | a worker given its item id | note and stick its own item only |
| System | dispatcher/reconciler | move into Running/PR open/In queue/Merged, automatic stuck causes, `GoalMet`/`Starved` |

`by` and `rank_by` on an item use `standing::Authority` (Owner/Lead). A lead re-rank that would
place a card above an owner-ranked card is refused. Dispatch defaults to `Paused`: adding the
feature never starts a worker by itself.

## Operations

Every change is one typed `Op` applied under the lock: `Add`, `Edit`, `Move`, `Reorder`,
`Split`, `SetStuck`, `Unstick`, `Remove`, `Note`, `SetCapacity`, `SetGoal`, `CheckMeasure`,
`SetDispatch`, `LinkPr`, `Undo`.

- Moves and reorders are relative ("put K41 before K12"), so two writers moving different cards
  both win; neither overwrites the other's whole board. Ranks stay dense within a column; the
  Ready rank is the pull order.
- Only a text edit carries the item `rev`. A stale edit (the item changed since it was opened)
  is refused with both safe versions — the current item and the item as the editor wanted it —
  so the front end can show them side by side. The refused edit changes nothing.
- A Ready item must have a `done_when`; that is the one content refusal. An item without an
  issue is allowed, and an over-large PR is a chip, not a refusal (warn and record).
- Text is bounded and single-line; a job kind follows the worker model-policy rule (lowercase
  letters, digits, dashes, at most 32). Capacity slots and item counts are bounded; the board
  file has a byte cap. Unknown stays unknown, never a manufactured zero.

## Undo

Each applied op records its inverse in `ops.jsonl`. `Undo{seq}` restores the prior state of the
affected item (or board field) as long as nothing else has touched that item since: an item op
is guarded by the item's revision, a split by its children's revisions. The owner may undo any
op; the lead undoes only its own ops; an undo cannot itself be undone in this slice. Undoing an
add removes the item; undoing a remove restores it; undoing an edit or move returns the prior
text or position.

## Metrics

`ops.jsonl` is the input for flow metrics (time in column, slot idle time) computed by a later
slice; it is never read on the request path. This slice records the ops; it does not evaluate
goal measures from GitHub (the reconciler does that later). A manual measure is ticked by the
owner, or by the lead with evidence.
