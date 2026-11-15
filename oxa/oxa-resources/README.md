# oxa-resources

Discovers what lives on disk and feeds the frozen prefix and the command list: context files,
`SYSTEM.md` / `APPEND_SYSTEM.md`, skills, prompt templates, packages, extension and theme paths.

| Module | Task | Provides |
|---|---|---|
| `resources.rs` | architect | `Resources`, `DiscoverOpts`, `Skill`, `Template`, `ContextFile`, `Diagnostic`, `ResourceSource`, `discover` (implemented glue) |
| `frontmatter.rs` | architect | `parse_frontmatter`, `frontmatter_get`, `frontmatter_bool` (implemented, unit-tested) |
| `context.rs` | B7 | `load_context`, `context_file_in`, `context_search_dirs` |
| `skills.rs` | B8 | `discover_skills`, `load_skill_file`, `validate_skill_name`, `skills_section`, `expand_skill_command` |
| `templates.rs` | B8 | `discover_templates`, `load_template_file`, `parse_template_args`, `expand_template`, `expand_template_command` |
| `sources.rs` | E10 | `PackageSource`, `parse_source`, `apply_filter` |
| `builtin.rs`, `builtin-skills/` | O-152 | `BUILTIN_SKILLS`, `install_builtin_skills`, `builtin_skill_path`, `read_identity`, `identity_path`, `write_atomic` |
| `packages.rs` | E10 | `PackageDecl`, `PackageManifest`, `PackageInfo`, `PackageResources`, `CommandRunner`, `PackageManager`, `package_resources`, `declared_packages`, `read_manifest` |

## Cross-task contracts

- `discover` is the only entry point used by `oxa-core`; it is called once per prefix epoch
  (session open, `/reload`). **W3**: its output is the input of `build_prefix`, so every
  function here is deterministic and preserves file bytes (no trimming, no reformatting).
- `discover` never fails and never uses the network. Problems are `Diagnostic`s.
- Until E10 lands, `package_resources` is a `todo!()`: B7/B8 tests call their own functions
  directly and pass `&[]` for `package_paths`.
- `skills_section` returns the exact `<skills>` text of pi; the empty string means "omit the
  section".
- Templates expand before the text becomes a message (`expand_template_command`); extension
  commands of the same name win (decided in `oxa-core/src/commands.rs`, C5).
- Trust: with `trusted == false` only context files are read from the project.
- Directories come from `oxa_config::Dirs` only; nothing reads `HOME` / `XDG_*` / `OXA_*`.
  The system skills directory (Omarchy's) is `Dirs::system_skills_dir`, `None` in tests.
- O-152: built-in skills are embedded and listed whether or not their files exist (the prefix
  never depends on install state); user skills of the same name win. The `ox-llm` skill names
  the v0.2.0 `oxc` CLI, paths and units (O-153 rename) with a `oxc` fallback note.
- Subagent definitions (`agents/*.md`, E11) reuse `parse_frontmatter`.

## Acceptance

- B7: agent dir + cwd + parents order; `AGENTS.override.md` same-dir rule; `SYSTEM.md` /
  `APPEND_SYSTEM.md` precedence; `-nc`.
- B8: Agent Skills frontmatter rules (warn, not fail); `<skills>` text; `$1 $@ ${1:-x}`;
  apostrophe and quote cases from the CHANGELOG.
- E10: install/remove/list/update for git + local; filters `!x +x -x`; `--local`; `npm:`
  sources are refused (`unsupported_in_oxa`).

## Used by

`oxa-core` (`build_prefix`, commands, epochs), `oxa-interactive`, `oxa-cli` (`pkg_cmd.rs`).
