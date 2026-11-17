# Typed control lines

Version 1 of the #330 convention is one line, excluding its transport newline:

```text
<sender>> <subject> <VERB> <message-id>[/<reply-to>] [text]
chief> ox#123@eab40a5 APPROVED m42
worker> ox#123@eab40a5 HOLD m43/m42 waiting for the hardware receipt
worker> ~protocol PROP m44 add a verb
```

Subjects are `ox#N@commit` (issue or PR number, positive without leading zeros; 7–40 lowercase hex commit characters) or `~topic`. Senders start with an ASCII letter and are at most 32 bytes. IDs/topics start with an ASCII letter or digit and are at most 64 bytes. Their remaining characters are ASCII letters, digits, `-`, `_` or `.`. Message IDs identify messages; they are separate from a runner event's sequence number. A reply ID cannot equal its message ID. Transport delivery acknowledgements remain transport data.

The closed verbs are `ACK REQ Q ANS PROP VOTE(+1) VOTE(-1) VOTE(0) DECIDED BLESSED RETHINK APPROVED MERGED HOLD BLOCKED DONE`. `ACK` and `ANS` require a reply ID. `HOLD`, `BLOCKED`, `RETHINK` and `VOTE(-1)` require a reason. Votes, verdicts, decisions and holds require a code subject with its commit; they cannot refer only to a topic or PR number. A changed head needs a new approval. Express a condition as a hold or proposal, rather than putting a conditional meaning behind `APPROVED`.

Header fields have exactly one ASCII space between them; text preserves internal spaces and Unicode. Leading/trailing whitespace, control characters, Unicode line separators, unknown verbs and malformed references are errors. The entire UTF-8 line is limited to 2,048 bytes. Nothing is corrected or inferred from prose.

[`oxa_proto::control::ControlLine`](../oxa-proto/src/control.rs) parses borrowed fields and implements `Display` for a lossless round trip. It adds no dependency, thread or policy. Parsing does not authenticate the sender, grant a role, approve a merge, lift a standing hold or authorize publication. Consumers check those rules against their current state, independently of optional text. IDs, deduplication, transport delivery and event sequencing belong to the runner.
