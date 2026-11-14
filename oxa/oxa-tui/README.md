# oxa-tui

The terminal UI library: a Rust port of pi-tui (MIT). **A component renders lines for a width;
the renderer diffs lines.** It knows nothing about agents or sessions. Depends on `oxa-proto`
(`KeyId`) and `oxa-config` (`Keymap`); used by `oxa-interactive` (and by `oxa-ext` only through
the `oxa_proto::UiComponent` trait, never directly).

| Module | Owner | Provides |
|---|---|---|
| `lib.rs`, `caps.rs`, `testterm.rs`, `theme_core.rs`, `builtin_themes.rs`, `components/mod.rs`, `assets/themes/*` | architect (frozen, complete) | `TermCaps`, `TermMode`, `CursorPos`, `ImageProtocol`, `ColorMode`, `TestTerminal`, `Theme` (+ `Theme::plain()`), `THEME_TOKENS`, pi's `dark.json` / `light.json`, `Spacer`, `Container` |
| `keys.rs` | A12 | `Key`, `InputEvent`, `Mouse`, `parse_input`, `flush_pending`, `encode_legacy`, `ESC_TIMEOUT_MS` |
| `term.rs` | A12 | `Terminal` trait, `ProcessTerminal`, `mode_sequence`, `restore_sequence`, `install_panic_hook` |
| `render.rs` | A13 | `Component`, `Interactive`, `Renderer`, `Frame`, `ScreenMode`, `render_stack` |
| `width.rs` | A13 | `visible_width`, `wrap_ansi`, `truncate_to_width`, `slice_by_column`, `pad_to_width`, `strip_ansi`, `cluster_boundaries`, `compose_overlay` |
| `editor.rs` | B10 | `Editor`, `EditorAction`, `word_wrap_line` |
| `markdown.rs`, `highlight.rs` | B11 | `render_markdown`, `Markdown`, `highlight_code`, `language_from_path` |
| `components/{text,boxed,select_list,input,loader,border,scrollbar}.rs` | B12 | `Text`, `TruncatedText`, `Boxed`, `SelectList`, `Input`, `Loader`, `Border`, `frame_lines`, `scrollbar_*` |
| `theme.rs` | B13 | `parse_theme`, `builtin_theme`, `system_theme`, `resolve_theme`, `rgb_to_256`, `parse_osc_color`, `css_color`, `list_themes` |
| `autocomplete.rs`, `fuzzy.rs` | C13 | `AutocompleteProvider`, `CombinedProvider`, `FileIndex`, `fuzzy_match`, `fuzzy_filter` |
| `image.rs`, `osc.rs` | C14 | `detect_caps`, `encode_kitty`, `encode_iterm2`, `render_image`, `image_dims`, `hyperlink`, `osc52_copy` |

## Cross-task contracts (frozen)
1. **Line validity.** A rendered line has no `\n`/`\r`/cursor movement, fits `width` columns as
   measured by `visible_width`, and closes its own styles. Everyone measures with
   `visible_width`; nobody calls `unicode-width` directly outside `width.rs`.
2. **Keys only through the keymap.** `Editor`, `SelectList` and `Input` resolve keys through
   `oxa_config::Keymap::action_for(&key.id)`; no component hard-codes a key. Tests build a
   keymap from the defaults.
3. **Colour only through `Theme`.** No SGR colour literals outside `theme_core.rs`.
   `Theme::plain()` exists from W0 so every wave-1/2 test can render without B13.
4. **`TestTerminal`** is complete in W0: renderer goldens compare `take_output()`; mode
   switches are recorded in `modes`, not written to `out`.
5. **No timers, no threads** in components: time enters through `Loader::tick` and
   `Terminal::poll(timeout)`.
6. **`rgb_to_256`** (B13) is called by `Theme` for RGB colours in 256-colour mode; until B13
   lands only `Theme::plain()` and indexed colours work.
7. Private fields: stubs carry `_owner_defines_fields: ()`; the owning task replaces it. Public
   names, signatures and enum variants do not change.

## Same-wave dependencies
A13 tests use `TestTerminal` + `Theme::plain()` (both complete). B10 depends on A13 (`width.rs`),
C13 trait (declared) and B12 `SelectList` (declared): its popup tests are `#[ignore]`d until the
wave checkpoint if those bodies are still `todo!()`.

## Not ported (plan sec. 1)
Mermaid layout (shown as a highlighted code block), LaTeX rendering, native modifier modules
(Windows/macOS), the 3D easter egg.
