---
name: review
description: Review a scoped change for actionable defects and return structured findings.
---

# Review rubric

- Review only the supplied diff and the surrounding code needed to understand it.
  Follow repository instructions. Do not modify files or expand into unrelated cleanup.
- Report defects introduced by this change that the author would reasonably fix:
  correctness, security, data loss, or concrete regressions. Skip style preferences,
  pre-existing problems, speculative risks, and issues already handled by the code.
- Verify the failing path and explain its trigger and impact. Cite the smallest useful
  line range in the changed code; do not invent test results or claim unrun checks passed.
- Assign priority independently of confidence: P0 blocks use broadly; P1 needs urgent
  correction; P2 is a normal fix; P3 is a low-impact fix. Confidence is a number from 0 to 1
  expressing how certain you are that the defect is real. Use a short imperative title
  prefixed with the matching `[P0]` through `[P3]`. Keep each body concise and actionable.
- Return only a JSON object matching the adjacent `findings.schema.json`: `findings`
  is an array of objects containing `title`, `body`, `priority`, `confidence`, `path`,
  `line_start`, and `line_end`; `overall_verdict` is `correct` or `incorrect`.
  Paths are repository-relative; line ranges are one-based, inclusive, and ordered
  (`line_end >= line_start`). Consumers must check path/range semantics against the diff;
  schema validation alone does not establish them.
- Use `incorrect` if there are actionable findings, otherwise `correct` with an empty
  findings array. The verdict covers the reviewed scope, not proof of correctness.

This skill and schema are data only. They are not registered in `BUILTIN_SKILLS`, do
not change any session prefix, and do not run a reviewer. A runner or lead may wire
this task kind later; schema validation/repair is a separate concern (#645).
