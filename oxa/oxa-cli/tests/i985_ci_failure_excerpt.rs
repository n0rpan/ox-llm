//! Issue #985: the guidance read a failing run as `gh run view <run-id> --log-failed | tail -100`.
//! A hundred lines can end inside one failing test's output, so the excerpt no longer names the
//! test, and `--log-failed` carries every failed job (both architectures and every shard), so the
//! same excerpt can mix lines from jobs that ran concurrently. The guidance now names the failed
//! jobs and selects the failure lines inside one job, which carry the test name.

const GUIDANCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../AGENTS.md");

/// The guidance with its line breaks flattened, so rewrapping a paragraph still matches.
fn guidance() -> String {
    let text = std::fs::read_to_string(GUIDANCE).unwrap_or_else(|e| panic!("{GUIDANCE}: {e}"));
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The failure-reading paragraph: from the `On failure` sentence to the follow-up sentence.
fn failure_paragraph(text: &str) -> &str {
    let start = text
        .find("On failure,")
        .expect("the guidance does not say how to read a failed run");
    let tail = &text[start..];
    let end = match tail.find("Fix the failure,") {
        Some(at) => at,
        None => tail
            .find("## Definition of done")
            .expect("the failure paragraph has no end"),
    };
    &tail[..end]
}

#[test]
fn a_fixed_tail_of_every_failed_job_is_not_the_failure_excerpt() {
    let text = guidance();
    let paragraph = failure_paragraph(&text);
    assert!(
        !paragraph.contains("tail -100"),
        "a fixed tail can end inside one test's output and mix in another job's lines: {paragraph}"
    );
    assert!(
        paragraph.contains("--json jobs") && paragraph.contains("--job <job-id>"),
        "the excerpt has to name the failed job before reading it: {paragraph}"
    );
}

#[test]
fn the_excerpt_keeps_the_lines_that_carry_the_failing_test_name() {
    let text = guidance();
    let paragraph = failure_paragraph(&text);
    assert!(
        paragraph.contains("names the failing test"),
        "the paragraph has to say what the excerpt must still contain: {paragraph}"
    );
    assert!(
        paragraph.contains("every failed job"),
        "the paragraph has to say why a whole-run excerpt mixes jobs: {paragraph}"
    );
    assert!(
        paragraph.contains("grep -nE"),
        "the failure lines are selected from the named job rather than taken from a tail: {paragraph}"
    );
}
