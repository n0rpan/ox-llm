//! V020 / OXA-LOCAL-FIRST T7: the `edit` failure path (the tolerant match behind
//! [`oxa_tools::apply_edits`]). Owner: lane l4-edit.
//!
//! Every case checks the expected outcome and the bytes left in the file. Inputs that match
//! exactly keep pi's output and take the same path.

use oxa_fake::TestEnv;
use oxa_proto::{
    codes, CancelToken, ExecResult, ExecSpec, Mode, OxaError, Sandbox, Tool, ToolCtx, ToolOutput, ToolTune,
    Value,
};
use oxa_tools::descriptions::errors;
use oxa_tools::EditTool;
use serde_json::json;

const PATH: &str = "f.txt";
const F1: &str = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";

/// A sandbox that never runs anything: the edit tool does not use it.
struct NoBash;

impl Sandbox for NoBash {
    fn check(&self) -> Result<(), OxaError> {
        Err(OxaError::new(codes::NO_SANDBOX, "edit tests run no commands"))
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult::default()
    }
}

/// Write `content` to a fresh file, run `args` with `tune`, and return the output and the
/// file's bytes afterwards.
fn run(content: &[u8], args: &Value, tune: ToolTune) -> (ToolOutput, Vec<u8>) {
    let env = TestEnv::new();
    std::fs::write(env.root().join(PATH), content).expect("write fixture");
    let sandbox = NoBash;
    let envs: Vec<(String, String)> = Vec::new();
    let cancel = CancelToken::new();
    let cx = ToolCtx {
        checked: &[],
        cwd: env.root(),
        spill_dir: env.root(),
        sandbox: &sandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &envs,
        tune,
        hidden: None,
    };
    let out = EditTool::new().run(args, &cx, &mut |_: &str| {});
    let after = std::fs::read(env.root().join(PATH)).expect("read back");
    (out, after)
}

/// `edits: [{oldText, newText}]` with one edit.
fn single(old: &str, new: &str) -> Value {
    json!({"path": PATH, "edits": [{"oldText": old, "newText": new}]})
}

enum Want {
    /// The file after a successful edit, as text.
    File(String),
    /// The expected error text.
    Error(String),
}

struct Case {
    name: &'static str,
    file: String,
    args: Value,
    want: Want,
}

fn nf() -> String {
    errors::edit_not_found(PATH, 0, 1)
}

fn case(name: &'static str, file: &str, args: Value, want: Want) -> Case {
    Case {
        name,
        file: file.to_string(),
        args,
        want,
    }
}

fn file(s: &str) -> Want {
    Want::File(s.to_string())
}

fn err(s: &str) -> Want {
    Want::Error(s.to_string())
}

fn corpus() -> Vec<Case> {
    let tab_f = "fn main() {\n\tlet a = 1;\n\tlet b = 2;\n}\n";
    let dup = "fn a() {\n    x();\n}\nfn b() {\n    x();\n}\n";
    let closest_ab = "Could not find oldText in f.txt. The closest text in the file is lines 1-3:\nfn main() {\n    let a = 1;\n    let b = 2;\nCopy oldText exactly from the file.";
    let closest_23 = "Could not find oldText in f.txt. The closest text in the file is lines 2-3:\n    let a = 1;\n    let b = 2;\nCopy oldText exactly from the file.";

    // 40 rows is more than 400 bytes of text; the closest region must be cut at a line.
    let rows: Vec<String> = (1..=40)
        .map(|i| format!("row {i:02} abcdefghijklmnopq"))
        .collect();
    let rows_file: String = rows.iter().map(|r| format!("{r}\n")).collect();
    let rows_old = format!("{}\nwrong", rows[..19].join("\n"));
    let rows_region = rows[..16].join("\n");
    let rows_closest = format!("Could not find oldText in f.txt. The closest text in the file is lines 1-16:\n{rows_region}\nCopy oldText exactly from the file.");

    let x500 = "x".repeat(500);
    let long_file = format!("{x500}\nshort\n");
    let long_old = format!("{x500}\nwrong");
    let long_closest = format!("Could not find oldText in f.txt. The closest text in the file is lines 1-1:\n{}\nCopy oldText exactly from the file.", "x".repeat(400));

    let e300 = "é".repeat(300);
    let uni_file = format!("a{e300}\nshort\n");
    let uni_old = format!("a{e300}\nwrong");
    let uni_closest = format!("Could not find oldText in f.txt. The closest text in the file is lines 1-1:\na{}\nCopy oldText exactly from the file.", "é".repeat(199));

    let twelve = "\tfoo();\n".repeat(12);
    let twelve_list = "oldText matches 12 places in f.txt, at lines 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, ... Add surrounding lines to make it unique.";

    let uni_s = "fn main() {\n    let s = \"héllo\";\n}\n";
    let uni_closest_s = "Could not find oldText in f.txt. The closest text in the file is lines 2-3:\n    let s = \"héllo\";\n}\nCopy oldText exactly from the file.";

    vec![
        // (a) indentation: tabs and spaces, deeper and shallower, mixed.
        case("tabs_oldtext_spaces_file", F1, single("\tlet a = 1;", "\tlet a = 10;"),
            file("fn main() {\n    let a = 10;\n    let b = 2;\n}\n")),
        case("spaces_oldtext_tabs_file", tab_f, single("    let a = 1;", "    let a = 10;"),
            file("fn main() {\n\tlet a = 10;\n\tlet b = 2;\n}\n")),
        case("spaces_multiline_into_tabs", tab_f,
            single("    let a = 1;\n    let b = 2;", "    let a = 1;\n    let b = 3;"),
            file("fn main() {\n\tlet a = 1;\n\tlet b = 3;\n}\n")),
        case("tabs_multiline_into_spaces", F1,
            single("\tlet a = 1;\n\tlet b = 2;", "\tlet a = 5;\n\tlet b = 2;"),
            file("fn main() {\n    let a = 5;\n    let b = 2;\n}\n")),
        case("deeper_single_line", F1, single("        let b = 2;", "        let b = 4;"),
            file("fn main() {\n    let a = 1;\n    let b = 4;\n}\n")),
        case("deeper_multiline", F1,
            single("        let a = 1;\n        let b = 2;", "        let a = 1;\n        let b = 8;"),
            file("fn main() {\n    let a = 1;\n    let b = 8;\n}\n")),
        case("deeper_newtext_adds_inner_line", F1,
            single("        let b = 2;", "        let b = 2;\n            let c = 3;"),
            file("fn main() {\n    let a = 1;\n    let b = 2;\n        let c = 3;\n}\n")),
        case("shallower_multiline", F1, single("let a = 1;\nlet b = 2;", "let a = 1;\nlet b = 6;"),
            file("fn main() {\n    let a = 1;\n    let b = 6;\n}\n")),
        case("shallower_newtext_blank_line_kept_empty", F1,
            single("let a = 1;\nlet b = 2;", "let a = 1;\n\nlet b = 6;"),
            file("fn main() {\n    let a = 1;\n\n    let b = 6;\n}\n")),
        case("shallower_newtext_already_indented_line", F1,
            single("let a = 1;\nlet b = 2;", "    let z = 0;\nlet a = 1;\nlet b = 2;"),
            file("fn main() {\n        let z = 0;\n    let a = 1;\n    let b = 2;\n}\n")),
        case("mixed_indent_inside_oldtext", F1,
            single("\tlet a = 1;\n    let b = 2;", "\tlet a = 1;\n    let b = 3;"),
            file("fn main() {\n    let a = 1;\n    let b = 3;\n}\n")),
        case("tab_pair_for_single_tab", tab_f, single("\t\tlet a = 1;", "\t\tlet a = 4;"),
            file("fn main() {\n\tlet a = 4;\n\tlet b = 2;\n}\n")),
        case("mixed_tab_then_spaces_single_line", F1, single("\t    let a = 1;", "\t    let a = 2;"),
            file("fn main() {\n    let a = 2;\n    let b = 2;\n}\n")),
        // (b) trailing whitespace and blank lines.
        case("trailing_spaces_in_file_region", "fn main() {\n    let a = 1;   \n    let b = 2;\n}\n",
            single("\tlet a = 1;\n\tlet b = 2;", "\tlet a = 1;\n\tlet b = 7;"),
            file("fn main() {\n    let a = 1;\n    let b = 7;\n}\n")),
        case("trailing_tab_from_newtext_kept", F1, single("\tlet b = 2;\t", "\tlet b = 9;\t"),
            file("fn main() {\n    let a = 1;\n    let b = 9;\t\n}\n")),
        case("blank_line_with_spaces_in_file", "fn main() {\n    let a = 1;\n    \n    let b = 2;\n}\n",
            single("let a = 1;\n\nlet b = 2;", "let a = 1;\n\nlet b = 3;"),
            file("fn main() {\n    let a = 1;\n\n    let b = 3;\n}\n")),
        case("blank_line_missing_from_file", F1, single("let a = 1;\n\nlet b = 2;", "x"),
            err(closest_ab)),
        // (c) the closest text (b) and the not-found text.
        case("one_wrong_line_of_two", F1, single("    let a = 1;\n    let b = 3;", "x"),
            err(closest_23)),
        case("one_wrong_line_tab_indent", F1, single("\tlet a = 1;\n\tlet b = 3;", "x"),
            err(closest_23)),
        case("no_line_matches_so_the_not_found_text", F1, single("\tlet c = 3;", "x"), err(&nf())),
        case("closest_tie_takes_the_first", "x\ny\nx\ny\n", single("x\nz", "q"),
            err("Could not find oldText in f.txt. The closest text in the file is lines 1-2:\nx\ny\nCopy oldText exactly from the file.")),
        case("closest_region_cut_at_400_bytes", &rows_file, single(&rows_old, "x"),
            err(&rows_closest)),
        case("closest_first_line_over_400_bytes", &long_file, single(&long_old, "x"),
            err(&long_closest)),
        case("closest_cut_at_a_character_boundary", &uni_file, single(&uni_old, "x"),
            err(&uni_closest)),
        case("crlf_file_no_match_closest_is_lf_text", "fn main() {\r\n    let a = 1;\r\n}\r\n",
            single("\tlet a = 1;\n\tlet c = 2;", "x"),
            err("Could not find oldText in f.txt. The closest text in the file is lines 2-3:\n    let a = 1;\n}\nCopy oldText exactly from the file.")),
        // (d) several places.
        case("duplicate_block_exact_indent", dup, single("    x();", "    y();"),
            err("oldText matches 2 places in f.txt, at lines 2, 5. Add surrounding lines to make it unique.")),
        case("duplicate_block_tab_oldtext", dup, single("\tx();", "\ty();"),
            err("oldText matches 2 places in f.txt, at lines 2, 5. Add surrounding lines to make it unique.")),
        case("many_places_list_is_capped_at_ten", &twelve, single("foo();", "bar();"),
            err(twelve_list)),
        // (e) CRLF, BOM, unicode, empty file, oldText longer than the file.
        case("crlf_file_tab_oldtext", "fn main() {\r\n    let a = 1;\r\n    let b = 2;\r\n}\r\n",
            single("\tlet a = 1;", "\tlet a = 10;"),
            file("fn main() {\r\n    let a = 10;\r\n    let b = 2;\r\n}\r\n")),
        case("crlf_file_multiline_reindent", "fn main() {\r\n\tlet a = 1;\r\n\tlet b = 2;\r\n}\r\n",
            single("    let a = 1;\n    let b = 2;", "    let a = 1;\n    let b = 3;"),
            file("fn main() {\r\n\tlet a = 1;\r\n\tlet b = 3;\r\n}\r\n")),
        case("bom_file_tab_oldtext", "\u{feff}fn main() {\n    let a = 1;\n}\n",
            single("\tlet a = 1;", "\tlet a = 2;"),
            file("\u{feff}fn main() {\n    let a = 2;\n}\n")),
        case("bom_and_crlf_file_space_oldtext", "\u{feff}fn main() {\r\n\tlet a = 1;\r\n}\r\n",
            single("    let a = 1;", "    let a = 2;"),
            file("\u{feff}fn main() {\r\n\tlet a = 2;\r\n}\r\n")),
        case("unicode_tab_oldtext", uni_s, single("\tlet s = \"héllo\";", "\tlet s = \"wörld\";"),
            file("fn main() {\n    let s = \"wörld\";\n}\n")),
        case("unicode_newtext_reindented", uni_s,
            single("\tlet s = \"héllo\";", "\tlet s = \"日本語\";\n\t// ✓"),
            file("fn main() {\n    let s = \"日本語\";\n    // ✓\n}\n")),
        case("unicode_wrong_line_closest", uni_s, single("    let s = \"hallo\";\n}", "x"),
            err(uni_closest_s)),
        case("empty_file_no_closest", "", single("let a = 1;", "x"), err(&nf())),
        case("oldtext_longer_than_file", "a\n", single("\tb\n\tc\n\td", "x"), err(&nf())),
        // (f) oldText ending in a line feed, file without a final line feed, legacy arguments.
        case("oldtext_with_trailing_newline", F1, single("\tlet b = 2;\n", "\tlet b = 9;\n"),
            file("fn main() {\n    let a = 1;\n    let b = 9;\n}\n")),
        case("last_line_without_final_newline", "fn main() {\n    let a = 1;\n    let b = 2;\n}",
            single("\t}", "\t};"),
            file("fn main() {\n    let a = 1;\n    let b = 2;\n};")),
        case("legacy_top_level_oldtext", F1,
            json!({"path": PATH, "oldText": "\tlet a = 1;", "newText": "\tlet a = 5;"}),
            file("fn main() {\n    let a = 5;\n    let b = 2;\n}\n")),
        // (g) several edits: each by the same rules.
        case("two_edits_both_mangled", F1, json!({"path": PATH, "edits": [
                {"oldText": "\tlet a = 1;", "newText": "\tlet a = 5;"},
                {"oldText": "\tlet b = 2;", "newText": "\tlet b = 6;"}]}),
            file("fn main() {\n    let a = 5;\n    let b = 6;\n}\n")),
        case("two_edits_exact_and_mangled", F1, json!({"path": PATH, "edits": [
                {"oldText": "    let a = 1;", "newText": "    let a = 5;"},
                {"oldText": "\tlet b = 2;", "newText": "\tlet b = 6;"}]}),
            file("fn main() {\n    let a = 5;\n    let b = 6;\n}\n")),
        case("two_edits_one_has_no_match", F1, json!({"path": PATH, "edits": [
                {"oldText": "\tlet a = 1;", "newText": "\tlet a = 5;"},
                {"oldText": "let zzz = 0;", "newText": "x"}]}),
            err(&errors::edit_not_found(PATH, 1, 2))),
        case("two_edits_overlap_after_matching", F1, json!({"path": PATH, "edits": [
                {"oldText": "\tlet a = 1;\n\tlet b = 2;", "newText": "\tX"},
                {"oldText": "\tlet b = 2;", "newText": "\tY"}]}),
            err(&errors::edit_overlap(0, 1, PATH))),
        case("tolerant_match_that_changes_nothing", F1, single("\tlet a = 1;", "\tlet a = 1;"),
            err(&errors::edit_no_change(PATH, 1))),
        // (h) every error keeps pi's meaning. A text found more than once stays a duplicate,
        // also when the places are inside longer lines or when one of them is a whole line (the
        // tolerant match never picks one of several places).
        case("duplicate_inside_lines", "abc\ndef\nghi\nx x\n", single("x", "y"),
            err(&errors::edit_duplicate(PATH, 0, 1, 2))),
        case("duplicate_one_whole_line_among_substrings", "x\nx x\n", single("x", "y"),
            err(&errors::edit_duplicate(PATH, 0, 1, 3))),
        case("duplicate_in_second_edit", "abc\nx x\n", json!({"path": PATH, "edits": [
                {"oldText": "abc", "newText": "A"}, {"oldText": "x", "newText": "y"}]}),
            err(&errors::edit_duplicate(PATH, 1, 2, 2))),
        case("duplicate_flat_form", "abc\nx x\n",
            json!({"path": PATH, "oldText": "x", "newText": "y"}),
            err(&errors::edit_duplicate(PATH, 0, 1, 2))),
        case("empty_oldtext", "abc\n", single("", "y"),
            err(&errors::edit_empty_old_text(PATH, 0, 1))),
        case("overlap_exact", "abc\ndef\nghi\n", json!({"path": PATH, "edits": [
                {"oldText": "abc\ndef", "newText": "1"}, {"oldText": "def\nghi", "newText": "2"}]}),
            err(&errors::edit_overlap(0, 1, PATH))),
        case("not_found_nothing_close", "abc\ndef\n", single("zzz", "q"), err(&nf())),
        case("no_change_exact", "abc\n", single("abc", "abc"),
            err(&errors::edit_no_change(PATH, 1))),
    ]
}

#[test]
fn corpus_has_at_least_forty_cases() {
    assert!(corpus().len() >= 40, "{} cases", corpus().len());
}

#[test]
fn mangled_oldtext_gives_the_expected_outcome() {
    let cases = corpus();
    let total = cases.len();
    for c in &cases {
        let before = c.file.as_bytes().to_vec();
        let (out, after) = run(&before, &c.args, ToolTune::default());
        match &c.want {
            Want::File(expected) => {
                assert!(!out.is_error, "{}: unexpected error {:?}", c.name, out.text);
                assert_eq!(
                    String::from_utf8(after).expect("utf8"),
                    *expected,
                    "{}: file bytes",
                    c.name
                );
                let edits = c.args["edits"].as_array().map_or(1, Vec::len);
                assert_eq!(out.text, errors::edit_ok(edits, PATH), "{}", c.name);
                assert!(
                    out.ui["diff"].as_str().is_some_and(|d| !d.is_empty()),
                    "{}: diff",
                    c.name
                );
            }
            Want::Error(expected) => {
                assert!(out.is_error, "{}: expected an error, got {:?}", c.name, out.text);
                assert_eq!(out.text, *expected, "{}: error text", c.name);
                assert_eq!(after, before, "{}: file must be untouched", c.name);
            }
        }
    }
    eprintln!("v020_edit_fallback: {total} mangled-oldText cases checked");
}

/// Inputs that match exactly, or that pi's own fuzzy match already accepted, still succeed.
#[test]
fn inputs_that_succeeded_before_still_succeed() {
    let crlf_bom = "\u{feff}fn main() {\r\n    let a = 1;\r\n}\r\n";
    let inputs: Vec<(&str, Value)> = vec![
        (F1, single("    let a = 1;", "    let a = 5;")),
        (crlf_bom, single("    let a = 1;", "    let a = 5;")),
        // A shallower single line is a substring of the file: pi matches it.
        (F1, single("let b = 2;", "let b = 7;")),
        // Trailing whitespace is pi's fuzzy match.
        (F1, single("let a = 1;   ", "let a = 5;")),
        (
            F1,
            json!({"path": PATH, "edits": [
            {"oldText": "    let a = 1;", "newText": "    let a = 5;"},
            {"oldText": "    let b = 2;", "newText": "    let b = 6;"}]}),
        ),
        // Exactly one occurrence of a tab-indented line.
        (
            "fn a() {\n    x();\n}\nfn b() {\n\tx();\n}\n",
            single("\tx();", "\ty();"),
        ),
    ];
    for (file, args) in inputs {
        let (out, after) = run(file.as_bytes(), &args, ToolTune::default());
        assert!(!out.is_error, "{args}: {}", out.text);
        let edits = args["edits"].as_array().map_or(1, Vec::len);
        assert_eq!(out.text, errors::edit_ok(edits, PATH), "{args}");
        assert!(
            out.ui["diff"].as_str().is_some_and(|d| !d.is_empty()),
            "{args}: diff"
        );
        assert_ne!(after, file.as_bytes(), "{args}: the edit changed the file");
    }
}

/// `diet` does not change edit's own texts.
#[test]
fn diet_does_not_change_a_successful_edit_text() {
    let args = single("    let a = 1;", "    let a = 5;");
    let diet_only = ToolTune {
        diet: true,
        ..Default::default()
    };
    let (a, _) = run(F1.as_bytes(), &args, ToolTune::default());
    let (b, _) = run(F1.as_bytes(), &args, diet_only);
    assert_eq!(a.text, b.text);
}
