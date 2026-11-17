//! B11 acceptance tests: Markdown and syntax highlight.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_tui::{
    highlight_code, language_from_path, language_supported, render_markdown, strip_ansi, visible_width,
    Appearance, Color, ColorMode, Component, Markdown, MarkdownOpts, Theme,
};
use std::collections::BTreeMap;

/// The snapshot document: headings, paragraphs with inline styles, a long paragraph, nested
/// lists, task items, an ordered list, a table, a quote, and code inside a list item and at top level.
const DOC: &str = "# Heading one

## Heading two

### Heading three

Paragraph with *emphasis*, **strong**, `code`, and a [link](https://example.com).

A long paragraph that keeps going past the eighty column boundary, so wrapping shows at narrow widths.

- first item
  - nested item
    1. deep ordered
- second item
- [x] task done
- [ ] task open

1. one
2. two

| name | size |
|---|---|
| alpha | 10 |
| beta with a longer name | 200 |

> quoted text
> - quoted list

- item with code:
  ```sh
  echo hi
  ```

```rust
fn main() {
    println!(\"hi\");
}
```
";

/// `DOC` at 80 columns with `Theme::plain()` and default options.
const GOLDEN_80: &str = "Heading one

Heading two

### Heading three

Paragraph with emphasis, strong, code, and a link (https://example.com).

A long paragraph that keeps going past the eighty column boundary, so wrapping
shows at narrow widths.
- first item
  - nested item
    1. deep ordered
- second item
- [x] task done
- [ ] task open

1. one
2. two

┌─────────────────────────┬──────┐
│ name                    │ size │
├─────────────────────────┼──────┤
│ alpha                   │ 10   │
│ beta with a longer name │ 200  │
└─────────────────────────┴──────┘

│ quoted text
│ - quoted list

- item with code:

  ┌─ sh ───────────────────────────────────────────────────────────────────────┐
  │  echo hi                                                                   │
  └────────────────────────────────────────────────────────────────────────────┘

┌─ rust ───────────────────────────────────────────────────────────────────────┐
│  fn main() {                                                                 │
│      println!(\"hi\");                                                         │
│  }                                                                           │
└──────────────────────────────────────────────────────────────────────────────┘";

/// `DOC` at 120 columns: the long paragraph stays on one line.
const GOLDEN_120: &str = "Heading one

Heading two

### Heading three

Paragraph with emphasis, strong, code, and a link (https://example.com).

A long paragraph that keeps going past the eighty column boundary, so wrapping shows at narrow widths.
- first item
  - nested item
    1. deep ordered
- second item
- [x] task done
- [ ] task open

1. one
2. two

┌─────────────────────────┬──────┐
│ name                    │ size │
├─────────────────────────┼──────┤
│ alpha                   │ 10   │
│ beta with a longer name │ 200  │
└─────────────────────────┴──────┘

│ quoted text
│ - quoted list

- item with code:

  ┌─ sh ───────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
  │  echo hi                                                                                                           │
  └────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘

┌─ rust ───────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│  fn main() {                                                                                                         │
│      println!(\"hi\");                                                                                                 │
│  }                                                                                                                   │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

fn plain() -> Theme {
    Theme::plain()
}

/// A theme with colours on the tokens the renderer uses, for the SGR checks.
fn colored() -> Theme {
    let mut colors = BTreeMap::new();
    let tokens: [(&str, u8); 14] = [
        ("text", 250),
        ("mdHeading", 33),
        ("mdCode", 180),
        ("mdLink", 39),
        ("mdLinkUrl", 244),
        ("mdListBullet", 36),
        ("mdCodeBlock", 107),
        ("mdCodeBlockBorder", 238),
        ("borderMuted", 240),
        ("mdQuote", 245),
        ("mdQuoteBorder", 240),
        ("syntaxKeyword", 130),
        ("syntaxFunction", 27),
        ("syntaxString", 64),
    ];
    for (name, n) in tokens {
        colors.insert(name.to_string(), Color::Indexed(n));
    }
    Theme {
        name: "test-colored".to_string(),
        appearance: Appearance::Dark,
        mode: ColorMode::Ansi256,
        colors,
        export: BTreeMap::new(),
        source: None,
    }
}

fn joined(lines: &[String]) -> String {
    lines.join("\n")
}

/// Every line of `lines` is a self-contained line that fits `width` and has no line breaks.
fn assert_valid_lines(lines: &[String], width: usize) {
    for l in lines {
        assert!(!l.contains('\n') && !l.contains('\r'), "line breaks in {l:?}");
        assert!(
            visible_width(l) <= width,
            "line {l:?} is {} wide, max {width}",
            visible_width(l)
        );
    }
}

/// lists, tables, nested code, quotes, headings
#[test]
fn snapshot_80_cols() {
    let lines = render_markdown(DOC, 80, &plain(), &MarkdownOpts::default());
    assert_valid_lines(&lines, 80);
    assert_eq!(joined(&lines), GOLDEN_80);
}

/// same document
#[test]
fn snapshot_120_cols() {
    let lines = render_markdown(DOC, 120, &plain(), &MarkdownOpts::default());
    assert_valid_lines(&lines, 120);
    assert_eq!(joined(&lines), GOLDEN_120);
}

/// Fences are markup, not transcript text; code has a labelled, themed frame.
#[test]
fn code_fences_render_as_labelled_frames() {
    let theme = colored();
    for (open, close) in [("```python", "```"), ("~~~python", "~~~"), ("```python", "")] {
        let doc = format!("{open}\ndef hello():\n    return 1\n{close}");
        let lines = render_markdown(&doc, 24, &theme, &MarkdownOpts::default());
        let text = strip_ansi(&joined(&lines));
        assert!(!text.contains("```") && !text.contains("~~~"), "{text}");
        assert_eq!(strip_ansi(&lines[0]), "┌─ python ─────────────┐");
        assert_eq!(strip_ansi(&lines[1]), "│  def hello():        │");
        assert_eq!(strip_ansi(&lines[2]), "│      return 1        │");
        assert_eq!(strip_ansi(lines.last().unwrap()), "└──────────────────────┘");
        assert!(lines[0].contains(&theme.fg_code("mdCodeBlockBorder")));
        assert!(lines[1].contains(&theme.fg_code("syntaxKeyword")));
        assert_valid_lines(&lines, 24);
    }
}

/// Wrapping preserves code (including wide characters) inside the frame.
#[test]
fn code_frames_fit_narrow_widths() {
    let doc = "```unknown\nabcdefghij日本語\n```";
    for width in [1, 2, 3, 4, 7, 12, 40] {
        let lines = render_markdown(doc, width, &plain(), &MarkdownOpts::default());
        assert_valid_lines(&lines, width as usize);
        assert!(!joined(&lines).contains('`'));
    }
    let opts = MarkdownOpts {
        code_block_indent: 0,
        ..MarkdownOpts::default()
    };
    assert_eq!(
        render_markdown("```\nabcdefghij\n```", 7, &plain(), &opts),
        ["┌─────┐", "│abcde│", "│fghij│", "└─────┘"]
    );
    let unknown = render_markdown(doc, 40, &colored(), &opts);
    assert!(unknown[1].contains(&colored().fg_code("mdCodeBlock")));
}

/// hyperlinks
#[test]
fn links_osc8() {
    let doc = "See [the docs](https://example.com/docs) now.";
    let on = MarkdownOpts {
        hyperlinks: true,
        ..MarkdownOpts::default()
    };
    let lines = render_markdown(doc, 80, &plain(), &on);
    let text = joined(&lines);
    assert_eq!(
        text,
        "See \x1b]8;;https://example.com/docs\x1b\\the docs\x1b]8;;\x1b\\ now."
    );
    assert_eq!(strip_ansi(&text), "See the docs now.");

    let off = MarkdownOpts::default();
    let lines = render_markdown(doc, 80, &plain(), &off);
    assert_eq!(joined(&lines), "See the docs (https://example.com/docs) now.");

    // A link whose text is its own URL shows the URL once.
    let auto = render_markdown("<https://example.com>", 80, &plain(), &off);
    assert_eq!(joined(&auto), "https://example.com");
    let auto_on = render_markdown("<https://example.com>", 80, &plain(), &on);
    assert_eq!(
        joined(&auto_on),
        "\x1b]8;;https://example.com\x1b\\https://example.com\x1b]8;;\x1b\\"
    );
}

/// codeBlockIndent
#[test]
fn code_block_indent_setting() {
    let doc = "```\nlet a = 1;\n  b\n```";
    let default = render_markdown(doc, 40, &plain(), &MarkdownOpts::default());
    assert_eq!(
        default,
        vec![
            "┌──────────────────────────────────────┐",
            "│  let a = 1;                          │",
            "│    b                                 │",
            "└──────────────────────────────────────┘",
        ]
    );

    for (opts, first, second) in [
        (
            MarkdownOpts {
                code_block_indent: 4,
                ..MarkdownOpts::default()
            },
            "│    let a = 1;",
            "│      b",
        ),
        (
            MarkdownOpts {
                code_block_indent: 0,
                ..MarkdownOpts::default()
            },
            "│let a = 1;",
            "│  b",
        ),
        (
            MarkdownOpts {
                code_block_prefix: Some("→ ".to_string()),
                ..MarkdownOpts::default()
            },
            "│→ let a = 1;",
            "│→   b",
        ),
    ] {
        let lines = render_markdown(doc, 40, &plain(), &opts);
        assert_eq!(lines[0], default[0]);
        assert_eq!(
            lines[1],
            format!("{first}{}│", " ".repeat(39 - visible_width(first)))
        );
        assert_eq!(
            lines[2],
            format!("{second}{}│", " ".repeat(39 - visible_width(second)))
        );
        assert_eq!(lines[3], default[3]);
        assert_valid_lines(&lines, 40);
    }
}

/// every prefix of a document renders
#[test]
fn streaming_partial_never_panics() {
    let rich = format!("{DOC}\n- list\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n~~strike~~ and ***both*** [x](y) <http://a.b> `c\n");
    let opts = MarkdownOpts::default();
    let opts_links = MarkdownOpts {
        hyperlinks: true,
        padding_x: 1,
        ..MarkdownOpts::default()
    };
    for (end, _) in rich.char_indices().chain(std::iter::once((rich.len(), ' '))) {
        let prefix = &rich[..end];
        for width in [1u16, 7, 40] {
            let lines = render_markdown(prefix, width, &plain(), &opts);
            assert_valid_lines(&lines, width as usize);
            let styled = render_markdown(prefix, width, &colored(), &opts_links);
            assert_valid_lines(&styled, width as usize);
        }
    }
    // The component caches per width and text and survives streaming updates.
    let mut md = Markdown::new("", opts.clone());
    let mut acc = String::new();
    for ch in rich.chars() {
        acc.push(ch);
        md.set_text(&acc);
        let lines = md.render(30, &plain());
        assert_valid_lines(&lines, 30);
    }
    assert_eq!(md.text(), rich);
}

/// highlighted block, no layout
#[test]
fn mermaid_as_code_block() {
    let doc = "```mermaid\ngraph TD\n  A-->B\n```";
    let lines = render_markdown(doc, 80, &plain(), &MarkdownOpts::default());
    // The diagram source is framed code, not rendered as a diagram.
    assert_eq!(lines.len(), 4);
    assert!(lines[0].starts_with("┌─ mermaid "));
    assert!(lines[1].starts_with("│  graph TD"));
    assert!(lines[2].starts_with("│    A-->B"));
    assert_eq!(lines[3], format!("└{}┘", "─".repeat(78)));
    assert_valid_lines(&lines, 80);
}

/// fallback
#[test]
fn syntax_highlight_known_and_unknown_lang() {
    let theme = colored();
    let known = highlight_code("fn main() {}", Some("rust"), &theme);
    assert_eq!(known.len(), 1);
    assert_eq!(strip_ansi(&known[0]), "fn main() {}");
    assert!(
        known[0].contains(&theme.fg_code("syntaxKeyword")),
        "keyword colour missing: {:?}",
        known[0]
    );
    assert!(language_supported("rust"));
    assert!(language_supported("rs"));

    let unknown = highlight_code("x = 1\ny = 2", Some("nosuchlang"), &theme);
    assert_eq!(
        unknown,
        vec![theme.fg("mdCodeBlock", "x = 1"), theme.fg("mdCodeBlock", "y = 2")]
    );
    assert_eq!(
        highlight_code("x = 1", None, &theme),
        vec![theme.fg("mdCodeBlock", "x = 1")]
    );
    assert!(!language_supported("nosuchlang"));
    assert!(!language_supported(""));

    // Never panics on odd input.
    assert!(highlight_code("", Some("rust"), &theme).is_empty());
    assert_eq!(highlight_code("a\tb", Some("rust"), &plain()), vec!["a   b"]);
    let _ = highlight_code("\"unterminated\n/* open", Some("c"), &theme);
    let _ = highlight_code("\u{0}\u{7f}\r\n", Some("rust"), &theme);
}

#[test]
fn language_from_path_table() {
    assert_eq!(language_from_path("src/main.rs"), Some("rust"));
    assert_eq!(language_from_path("app.TSX"), Some("typescript"));
    assert_eq!(language_from_path("build/Makefile"), Some("makefile"));
    assert_eq!(language_from_path("Dockerfile"), Some("dockerfile"));
    assert_eq!(language_from_path("CMakeLists.txt"), Some("cmake"));
    assert_eq!(language_from_path("archive.zzz"), None);
    assert_eq!(language_from_path(""), None);
}

#[test]
fn nested_quotes_and_task_lists_fit_narrow_widths() {
    let doc = "> > deeply quoted text that wraps across several lines at a small width\n\n- [x] a task whose text is long enough to wrap\n  1. nested ordered item with text";
    for width in [8u16, 12, 20] {
        let lines = render_markdown(doc, width, &plain(), &MarkdownOpts::default());
        assert_valid_lines(&lines, width as usize);
        assert!(lines.len() > 3);
    }
}

#[test]
fn table_cells_keep_columns_aligned_with_wide_characters() {
    let doc = "| 名前 | note |\n|---|---|\n| 日本語 | ok |\n| x | ascii only text |";
    let lines = render_markdown(doc, 30, &plain(), &MarkdownOpts::default());
    assert_valid_lines(&lines, 30);
    let widths: Vec<usize> = lines.iter().map(|l| visible_width(l)).collect();
    assert!(
        widths.iter().all(|&w| w == widths[0]),
        "ragged table: {widths:?}\n{}",
        joined(&lines)
    );
}

#[test]
fn hard_breaks_escapes_and_intraword_underscores() {
    let doc = "line one  \nline two\\\nline three\n\nsnake_case_name and \\*not emphasis\\* and a_b_c";
    let lines = render_markdown(doc, 80, &plain(), &MarkdownOpts::default());
    assert_eq!(
        lines,
        vec![
            "line one",
            "line two",
            "line three",
            "",
            "snake_case_name and *not emphasis* and a_b_c"
        ]
    );
}

#[test]
fn background_and_padding_fill_the_width() {
    let opts = MarkdownOpts {
        padding_x: 2,
        bg_token: Some("userMessageBg".to_string()),
        ..MarkdownOpts::default()
    };
    let lines = render_markdown("hi", 10, &plain(), &opts);
    assert_eq!(lines, vec!["  hi      "]);
}

#[test]
fn markdown_file_round_trip_in_test_env() {
    let env = TestEnv::new();
    let path = env.write("notes/readme.md", "# Notes\n\n- one\n- two\n");
    let text = std::fs::read_to_string(&path).expect("read back the markdown file");
    assert_eq!(language_from_path(path.to_str().unwrap_or("")), Some("markdown"));
    let lines = render_markdown(&text, 40, &plain(), &MarkdownOpts::default());
    assert_eq!(lines, vec!["Notes", "", "- one", "- two"]);
}

#[test]
fn colored_output_closes_its_styles_on_every_line() {
    let theme = colored();
    let opts = MarkdownOpts {
        hyperlinks: true,
        ..MarkdownOpts::default()
    };
    let lines = render_markdown(DOC, 50, &theme, &opts);
    assert_valid_lines(&lines, 50);
    for l in &lines {
        // Each line ends with its styles closed: no open SGR or OSC 8 at the end of a line.
        let opens = l.matches("\x1b[38;5;").count() + l.matches("\x1b[1m").count();
        let closes =
            l.matches("\x1b[39m").count() + l.matches("\x1b[22m").count() + l.matches("\x1b[0m").count();
        assert!(opens <= closes || opens == 0, "unbalanced line {l:?}");
        assert_eq!(
            l.matches("\x1b]8;;").count() % 2,
            0,
            "unclosed hyperlink in {l:?}"
        );
    }
}
