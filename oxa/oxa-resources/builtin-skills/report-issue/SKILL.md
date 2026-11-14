---
name: report-issue
description: Draft a bug report for the public ox-llm repository from a scrubbed system digest (no email, no home paths, no prompts or transcripts, no secrets). Shows the user the exact title and body and posts only after the user confirms in their own terminal. Use when the user wants to report a problem with ox-llm, oxa, oxc or the engine.
---

# Report an ox-llm issue

Never post anything yourself and never paste conversation text, prompts, file contents, paths
under the home directory, names, emails, keys or tokens into a report.

1. Ask the user for a one-line title and, in their words, what they did, what they expected and
   what happened. Keep it short; no transcripts and no prompt text.
2. Write that description to a file in the session's temporary directory and draft the issue:

   ```
   oxa report-issue draft --title "TITLE" --notes-file NOTES.txt
   ```

   The command collects a scrubbed digest (oxa version, machine, OS, models, the controller's
   status and health digest), scrubs the notes too, saves the draft and prints the draft path
   followed by the exact title and body.
3. Show the user the printed title and body exactly as printed, and the draft path. Ask whether
   anything should be removed. To change it, edit the notes and run the draft command again.
4. Posting needs the user's explicit confirmation. Tell the user to run, in their own terminal:

   ```
   oxa report-issue submit DRAFT-PATH
   ```

   It prints the text again and posts with `gh issue create` only after the user types `yes` on
   the terminal. It refuses when there is no terminal, so running it from a tool cannot post.
5. Reporting is off until the public repository exists: `submit` then says so and leaves the
   draft in place so the user can file it by hand. The target is the setting
   `oxa.reportIssue.repo` (`OWNER/NAME`) in `~/.oxa/agent/settings.json`.
