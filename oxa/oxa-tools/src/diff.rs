//! A7: exact-text edit matching and diff rendering. Owner: task A7. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/tools/edit-diff.ts
//
// Known gap: pi normalises with NFKC. Without a `unicode-normalization` dependency (architect
// owns Cargo.toml) only the fullwidth ASCII block (U+FF01..U+FF5E) and U+3000 are folded, so
// canonical composition (`e` + U+0301 against `é`) does not match.
use crate::descriptions::{default_edit, errors};
use similar::{ChangeTag, TextDiff};
use std::borrow::Cow;

/// Context lines shown around each change (pi's `generateDiffString` default).
const CONTEXT: usize = 4;

/// The error text of a replacement that cannot be mapped back onto the original lines.
const OUTSIDE_BASE: &str = "Replacement range is outside the base content.";
const DIFFERENT_LINE_COUNT: &str =
    "Cannot preserve unchanged lines because the base content has a different line count.";

/// One replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

/// Result of applying edits to a file's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOutcome {
    /// The new file content, BOM and original line endings restored.
    pub content: String,
    /// Unified diff with pi's line-number format, for display.
    pub diff: String,
    /// 1-based line of the first change in the new content.
    pub first_changed_line: usize,
}

/// Remove a UTF-8 BOM. Returns `(bom, rest)` with `bom` = `"\u{feff}"` or `""`.
pub fn strip_bom(content: &str) -> (&str, &str) {
    match content.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", content),
    }
}

/// The dominant line ending of `content`: `"\r\n"` or `"\n"` (pi's `detectLineEnding`).
/// `"\r\n"` only when the first line feed is preceded by a carriage return.
pub fn detect_line_ending(content: &str) -> &'static str {
    match content.find('\n') {
        Some(lf) if lf > 0 && content.as_bytes()[lf - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

/// Apply `edits` to `content` with pi's algorithm: matching on LF-normalised text, exact match
/// first, then pi's fuzzy normalisation (trailing whitespace, smart quotes, unicode dashes and
/// spaces); every `old_text` must match exactly once; matches must not overlap; all edits are
/// matched against the original content, not sequentially. Mixed line endings: untouched
/// lines keep their own ending. `Err` is pi's error text for the case (`path` is used in the
/// text): not found, found N times, overlap, empty old text, no change.
pub fn apply_edits(content: &str, edits: &[Edit], path: &str) -> Result<EditOutcome, String> {
    let (bom, text) = strip_bom(content);
    let normalized = normalize_to_lf(text);
    let applied = apply_to_normalized(&normalized, edits, path)?;
    let body = if text == normalized {
        applied.new_content.clone()
    } else {
        apply_replacements_to_original(text, &applied.replacements)
    };
    let (diff, first) = display_diff(&normalized, &applied.new_content, CONTEXT);
    Ok(EditOutcome {
        content: format!("{bom}{body}"),
        diff,
        first_changed_line: first.unwrap_or(1),
    })
}

/// OXA-LOCAL-FIRST T7: [`apply_edits`], and where its matching cannot place an
/// edit, the indentation-tolerant unique match (a), or the closest-text error (b), or the
/// several-places error (c). Only reached when `apply_edits` fails, so every input that
/// succeeds today gives the same bytes. Each edit is placed by its own position in the
/// normalised text; the rest (overlap, no change, line endings, diff) is `apply_edits`' own.
pub fn apply_edits_tolerant(content: &str, edits: &[Edit], path: &str) -> Result<EditOutcome, String> {
    let err = match apply_edits(content, edits, path) {
        Ok(outcome) => return Ok(outcome),
        Err(e) => e,
    };
    let (bom, text) = strip_bom(content);
    let normalized = normalize_to_lf(text);
    let total = edits.len();

    let mut placed: Vec<Placed> = Vec::with_capacity(total);
    for (i, edit) in edits.iter().enumerate() {
        let old = normalize_to_lf(&edit.old_text);
        if old.is_empty() {
            return Err(err);
        }
        let new = normalize_to_lf(&edit.new_text);
        let occurrences = count_occurrences(&normalized, &old);
        if occurrences > 1 {
            // The text is in the file more than once: it stays a duplicate. The tolerant
            // line match never picks one of the places (a text inside a longer line has no
            // line region, and one whole-line region among several hits is a guess).
            let regions = tolerant_regions(&normalized, &old);
            return Err(if regions.len() > 1 {
                let lines: Vec<usize> = regions.iter().map(|r| r.first + 1).collect();
                default_edit::not_unique(path, &lines)
            } else {
                errors::edit_duplicate(path, i, total, occurrences)
            });
        }
        if occurrences == 1 {
            // Today's matching places it. Only an exact hit has a position in `normalized`.
            let Some(start) = normalized.find(&old) else {
                return Err(err);
            };
            placed.push(Placed {
                edit_index: i,
                start,
                len: old.len(),
                new_text: new,
            });
            continue;
        }
        let regions = tolerant_regions(&normalized, &old);
        match regions.as_slice() {
            [] => {
                return Err(match closest_region(&normalized, &old) {
                    Some(closest) => {
                        default_edit::not_found(path, closest.first, closest.last, &closest.text)
                    }
                    None => errors::edit_not_found(path, i, total),
                });
            }
            [one] => {
                let file_region = &normalized[one.start..one.end];
                let new_text = reindent(
                    &new,
                    leading_indent(first_content_line(&old)),
                    leading_indent(first_content_line(file_region)),
                );
                placed.push(Placed {
                    edit_index: i,
                    start: one.start,
                    len: one.end - one.start,
                    new_text,
                });
            }
            many => {
                let lines: Vec<usize> = many.iter().map(|r| r.first + 1).collect();
                return Err(default_edit::not_unique(path, &lines));
            }
        }
    }

    placed.sort_by_key(|p| p.start);
    for pair in placed.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        if prev.start + prev.len > cur.start {
            return Err(errors::edit_overlap(prev.edit_index, cur.edit_index, path));
        }
    }
    let replacements: Vec<TextReplacement> = placed
        .iter()
        .map(|p| TextReplacement {
            match_index: p.start,
            match_len: p.len,
            new_text: p.new_text.clone(),
        })
        .collect();
    let new_content = apply_replacements(&normalized, &replacements, 0);
    if new_content == normalized {
        return Err(errors::edit_no_change(path, total));
    }
    let body = if text == normalized {
        new_content.clone()
    } else {
        apply_replacements_to_original(text, &replacements)
    };
    let (diff, first) = display_diff(&normalized, &new_content, CONTEXT);
    Ok(EditOutcome {
        content: format!("{bom}{body}"),
        diff,
        first_changed_line: first.unwrap_or(1),
    })
}

/// One edit placed by `apply_edits_tolerant`: byte range in the normalised text.
struct Placed {
    edit_index: usize,
    start: usize,
    len: usize,
    new_text: String,
}

/// A run of file lines matched by [`tolerant_regions`]: 0-based first line, and the byte range
/// `[start, end)` of the normalised text it replaces.
struct Region {
    first: usize,
    start: usize,
    end: usize,
}

/// The lines of `text` as byte ranges without their line feed. A final line feed ends the
/// last line; it does not start an empty one.
fn line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            out.push((start, i));
            start = i + 1;
        }
    }
    if start < text.len() {
        out.push((start, text.len()));
    }
    out
}

/// The lines of `old` for matching: a final line feed is not a line of its own.
fn old_lines(old: &str) -> Vec<&str> {
    old.strip_suffix('\n').unwrap_or(old).split('\n').collect()
}

/// What the tolerant match compares: leading spaces and tabs and trailing whitespace ignored.
fn line_key(line: &str) -> &str {
    line.trim_start_matches([' ', '\t']).trim_end()
}

/// The leading spaces and tabs of `line`.
fn leading_indent(line: &str) -> &str {
    let rest = line.trim_start_matches([' ', '\t']);
    &line[..line.len() - rest.len()]
}

/// The first line of `text` that is not blank (`""` when there is none).
fn first_content_line(text: &str) -> &str {
    text.split('\n').find(|l| !l.trim().is_empty()).unwrap_or("")
}

/// Every window of `old`'s lines in `normalized` whose lines match by [`line_key`].
fn tolerant_regions(normalized: &str, old: &str) -> Vec<Region> {
    let lines = line_ranges(normalized);
    let want = old_lines(old);
    let n = want.len();
    if n == 0 || n > lines.len() {
        return Vec::new();
    }
    let line = |k: usize| &normalized[lines[k].0..lines[k].1];
    let ends_with_lf = old.ends_with('\n');
    (0..=lines.len() - n)
        .filter(|&i| (0..n).all(|k| line_key(line(i + k)) == line_key(want[k])))
        .map(|i| {
            let last_end = lines[i + n - 1].1;
            let end = if ends_with_lf && last_end < normalized.len() {
                last_end + 1
            } else {
                last_end
            };
            Region {
                first: i,
                start: lines[i].0,
                end,
            }
        })
        .collect()
}

/// The window of `old`'s line count with the most lines equal (by [`line_key`]) at the same
/// position, the first on a tie, if it has at least one: 1-based first and last line, and the
/// text, cut to [`default_edit::CLOSEST_MAX_BYTES`] at a line boundary (inside a single long line,
/// at a character boundary).
struct Closest {
    first: usize,
    last: usize,
    text: String,
}

fn closest_region(normalized: &str, old: &str) -> Option<Closest> {
    let lines = line_ranges(normalized);
    let want = old_lines(old);
    let n = want.len();
    if n == 0 || n > lines.len() {
        return None;
    }
    let line = |k: usize| &normalized[lines[k].0..lines[k].1];
    let mut best: Option<usize> = None;
    let mut best_score = 0;
    for i in 0..=lines.len() - n {
        let score = (0..n)
            .filter(|&k| line_key(line(i + k)) == line_key(want[k]))
            .count();
        if score > best_score {
            best_score = score;
            best = Some(i);
        }
    }
    let i = best?;

    let cap = default_edit::CLOSEST_MAX_BYTES;
    let mut text = String::new();
    let mut shown = 0;
    for k in 0..n {
        let piece = line(i + k);
        let added = if shown == 0 { piece.len() } else { piece.len() + 1 };
        if text.len() + added > cap {
            break;
        }
        if shown > 0 {
            text.push('\n');
        }
        text.push_str(piece);
        shown += 1;
    }
    if shown == 0 {
        let first = line(i);
        let mut cut = cap.min(first.len());
        while !first.is_char_boundary(cut) {
            cut -= 1;
        }
        text.push_str(&first[..cut]);
        shown = 1;
    }
    Some(Closest {
        first: i + 1,
        last: i + shown,
        text,
    })
}

/// (a): `new` with each non-blank line that starts with `old_indent` having that prefix
/// replaced by `file_indent`. With an empty `old_indent` the prefix is added to each non-blank
/// line. Other lines stay as they are.
fn reindent(new: &str, old_indent: &str, file_indent: &str) -> String {
    new.split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                line.to_string()
            } else if let Some(rest) = line.strip_prefix(old_indent) {
                format!("{file_indent}{rest}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// pi's display diff (`generateDiffString`): unified, `context` lines around changes, line
/// numbers padded to equal width.
pub fn unified_diff(old: &str, new: &str, context: usize) -> String {
    display_diff(old, new, context).0
}

// ---------------------------------------------------------------------------------------------
// Matching (edit-diff.ts: applyEditsToNormalizedContent)

#[derive(Debug, Clone, PartialEq, Eq)]
struct TextReplacement {
    match_index: usize,
    match_len: usize,
    new_text: String,
}

struct Matched {
    edit_index: usize,
    match_index: usize,
    match_len: usize,
    new_text: String,
}

struct Applied {
    new_content: String,
    replacements: Vec<TextReplacement>,
}

struct FuzzyHit {
    index: usize,
    len: usize,
    fuzzy: bool,
}

fn apply_to_normalized(normalized: &str, edits: &[Edit], path: &str) -> Result<Applied, String> {
    let total = edits.len();
    let norm_edits: Vec<(String, String)> = edits
        .iter()
        .map(|e| (normalize_to_lf(&e.old_text), normalize_to_lf(&e.new_text)))
        .collect();
    for (i, (old, _)) in norm_edits.iter().enumerate() {
        if old.is_empty() {
            return Err(errors::edit_empty_old_text(path, i, total));
        }
    }

    let used_fuzzy = norm_edits
        .iter()
        .any(|(old, _)| fuzzy_find(normalized, old).is_some_and(|h| h.fuzzy));
    let base: Cow<str> = if used_fuzzy {
        Cow::Owned(normalize_for_fuzzy_match(normalized))
    } else {
        Cow::Borrowed(normalized)
    };

    let mut matched = Vec::with_capacity(total);
    for (i, (old, new)) in norm_edits.iter().enumerate() {
        let hit = fuzzy_find(&base, old).ok_or_else(|| errors::edit_not_found(path, i, total))?;
        let occurrences = count_occurrences(&base, old);
        if occurrences > 1 {
            return Err(errors::edit_duplicate(path, i, total, occurrences));
        }
        matched.push(Matched {
            edit_index: i,
            match_index: hit.index,
            match_len: hit.len,
            new_text: new.clone(),
        });
    }

    matched.sort_by_key(|m| m.match_index);
    for pair in matched.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        if prev.match_index + prev.match_len > cur.match_index {
            return Err(errors::edit_overlap(prev.edit_index, cur.edit_index, path));
        }
    }

    let replacements: Vec<TextReplacement> = if used_fuzzy {
        widen_replacements_to_lines(normalized, &base, &matched)?
    } else {
        matched
            .iter()
            .map(|m| TextReplacement {
                match_index: m.match_index,
                match_len: m.match_len,
                new_text: m.new_text.clone(),
            })
            .collect()
    };
    let new_content = apply_replacements(normalized, &replacements, 0);
    if new_content == normalized {
        return Err(errors::edit_no_change(path, total));
    }
    Ok(Applied {
        new_content,
        replacements,
    })
}

/// Exact match first; otherwise the match in fuzzy-normalised space.
fn fuzzy_find(content: &str, old: &str) -> Option<FuzzyHit> {
    if let Some(index) = content.find(old) {
        return Some(FuzzyHit {
            index,
            len: old.len(),
            fuzzy: false,
        });
    }
    let fc = normalize_for_fuzzy_match(content);
    let fo = normalize_for_fuzzy_match(old);
    let index = fc.find(&fo)?;
    Some(FuzzyHit {
        index,
        len: fo.len(),
        fuzzy: true,
    })
}

/// pi counts occurrences after fuzzy normalisation of both sides (non-overlapping, as `split`).
fn count_occurrences(content: &str, old: &str) -> usize {
    let fc = normalize_for_fuzzy_match(content);
    let fo = normalize_for_fuzzy_match(old);
    if fo.is_empty() {
        // JS `"abc".split("")` has length 3, so the count is one less than the UTF-16 length.
        return fc.encode_utf16().count().saturating_sub(1);
    }
    fc.matches(fo.as_str()).count()
}

fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let end = offset + line.len();
        out.push((offset, end));
        offset = end;
    }
    out
}

/// The line span `[first, end)` (indices into `lines`) that a replacement `[start, end)` touches.
fn line_range(lines: &[(usize, usize)], start: usize, end: usize) -> Result<(usize, usize), String> {
    let first = lines
        .iter()
        .position(|&(s, e)| start >= s && start < e)
        .ok_or_else(|| OUTSIDE_BASE.to_string())?;
    let mut last = first;
    while last < lines.len() && lines[last].1 < end {
        last += 1;
    }
    if last >= lines.len() {
        return Err(OUTSIDE_BASE.to_string());
    }
    Ok((first, last + 1))
}

/// Turn replacements matched against `base` (a fuzzy-normalised view of `original`) into
/// replacements of `original` that rewrite only the lines they touch, so untouched lines keep
/// their original bytes (and line endings).
fn widen_replacements_to_lines(
    original: &str,
    base: &str,
    matched: &[Matched],
) -> Result<Vec<TextReplacement>, String> {
    let original_lines = line_spans(original);
    let base_lines = line_spans(base);
    if original_lines.len() != base_lines.len() {
        return Err(DIFFERENT_LINE_COUNT.to_string());
    }

    // Groups of replacements whose line ranges overlap: (first line, end line, members).
    let mut groups: Vec<(usize, usize, Vec<TextReplacement>)> = Vec::new();
    for m in matched {
        let (first, end) = line_range(&base_lines, m.match_index, m.match_index + m.match_len)?;
        let rep = TextReplacement {
            match_index: m.match_index,
            match_len: m.match_len,
            new_text: m.new_text.clone(),
        };
        if let Some(cur) = groups.last_mut() {
            if first < cur.1 {
                cur.1 = cur.1.max(end);
                cur.2.push(rep);
                continue;
            }
        }
        groups.push((first, end, vec![rep]));
    }

    Ok(groups
        .into_iter()
        .map(|(first, end, members)| {
            let group_start = base_lines[first].0;
            let group_end = base_lines[end - 1].1;
            let orig_start = original_lines[first].0;
            TextReplacement {
                match_index: orig_start,
                match_len: original_lines[end - 1].1 - orig_start,
                new_text: apply_replacements(&base[group_start..group_end], &members, group_start),
            }
        })
        .collect())
}

/// Splice sorted, disjoint replacements into `content`. `offset` maps replacement indices onto
/// `content` (0 for the whole text, the group start for a slice). Applied last to first so the
/// earlier indices stay valid.
fn apply_replacements(content: &str, replacements: &[TextReplacement], offset: usize) -> String {
    let mut result = content.to_string();
    for r in replacements.iter().rev() {
        let at = r.match_index - offset;
        result = format!("{}{}{}", &result[..at], r.new_text, &result[at + r.match_len..]);
    }
    result
}

// ---------------------------------------------------------------------------------------------
// Line endings (edit-diff.ts: applyReplacementsToOriginal)

/// Replacements of `original`-normalised offsets, spliced into `original` itself. Text outside
/// the replaced ranges keeps its bytes. Line breaks in new text take the ending of the line
/// where the replaced range starts.
fn apply_replacements_to_original(original: &str, replacements: &[TextReplacement]) -> String {
    // Only CRLF shrinks when normalised: the LF of the n-th CRLF sits at `p - n` in the
    // normalised text.
    let mut crlf_before = 0usize;
    let mut next_crlf = original.find("\r\n");
    let mut to_original = |offset: usize| -> usize {
        while let Some(p) = next_crlf {
            if p - crlf_before < offset {
                crlf_before += 1;
                next_crlf = original
                    .get(p + 2..)
                    .and_then(|s| s.find("\r\n"))
                    .map(|q| q + p + 2);
            } else {
                break;
            }
        }
        offset + crlf_before
    };

    let mut out = String::with_capacity(original.len());
    let mut ends_with_cr = false;
    let mut position = 0usize;
    for r in replacements {
        let start = to_original(r.match_index);
        push_part(
            &mut out,
            &mut ends_with_cr,
            original.get(position..start).unwrap_or(""),
        );
        let ending = line_ending_at(original, start);
        if ending == "\n" {
            push_part(&mut out, &mut ends_with_cr, &r.new_text);
        } else {
            push_part(&mut out, &mut ends_with_cr, &r.new_text.replace('\n', ending));
        }
        position = to_original(r.match_index + r.match_len);
    }
    push_part(
        &mut out,
        &mut ends_with_cr,
        original.get(position..).unwrap_or(""),
    );
    out
}

/// Append `part`. A carriage return left at the end of the previous part and a line feed at the
/// start of this one would merge into one CRLF, so an extra LF keeps them apart.
fn push_part(out: &mut String, ends_with_cr: &mut bool, part: &str) {
    if part.is_empty() {
        return;
    }
    if *ends_with_cr && part.starts_with('\n') {
        out.push('\n');
    }
    out.push_str(part);
    *ends_with_cr = part.ends_with('\r');
}

/// Line ending of the line containing `offset`: its own LF or CRLF, or that of the line before
/// for an unterminated last line. A lone CR only counts in files without any LF.
fn line_ending_at(content: &str, offset: usize) -> &'static str {
    let b = content.as_bytes();
    let from = offset.min(b.len());
    let lf = b[from..]
        .iter()
        .position(|&c| c == b'\n')
        .map(|p| p + from)
        .or_else(|| b[..(offset + 1).min(b.len())].iter().rposition(|&c| c == b'\n'));
    match lf {
        None => {
            if content.contains('\r') {
                "\r"
            } else {
                "\n"
            }
        }
        Some(lf) => {
            if lf > 0 && b[lf - 1] == b'\r' {
                "\r\n"
            } else {
                "\n"
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Normalisation (edit-diff.ts: normalizeToLF, normalizeForFuzzyMatch)

fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// The fold pi applies for fuzzy matching, one character at a time: smart quotes, unicode
/// dashes, special spaces and (NFKC subset) fullwidth ASCII.
fn fold_char(c: char) -> char {
    match c {
        '\u{2018}'..='\u{201B}' => '\'',
        '\u{201C}'..='\u{201F}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        _ => c,
    }
}

/// Normalise text for fuzzy matching: fold characters, then strip trailing whitespace per line.
pub(crate) fn normalize_for_fuzzy_match(text: &str) -> String {
    let folded: String = text.chars().map(fold_char).collect();
    let mut out = String::with_capacity(folded.len());
    for (i, line) in folded.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end_matches(|c: char| c.is_whitespace() || c == '\u{feff}'));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Display diff (edit-diff.ts: generateDiffString, formatDisplayDiff)

fn count_lines(text: &str) -> usize {
    text.matches('\n').count() + 1
}

/// Group the line changes of `old` -> `new` into runs of one tag, each run with its lines
/// (without line feeds).
fn change_runs(old: &str, new: &str) -> Vec<(ChangeTag, Vec<String>)> {
    let diff = TextDiff::from_lines(old, new);
    let mut runs: Vec<(ChangeTag, Vec<String>)> = Vec::new();
    for change in diff.iter_all_changes() {
        let tag = change.tag();
        let value = change.value();
        let line = value.strip_suffix('\n').unwrap_or(value).to_string();
        match runs.last_mut() {
            Some((t, lines)) if *t == tag => lines.push(line),
            _ => runs.push((tag, vec![line])),
        }
    }
    runs
}

/// Returns the display diff and the 1-based first changed line in the new content.
fn display_diff(old: &str, new: &str, context: usize) -> (String, Option<usize>) {
    let width = count_lines(old).max(count_lines(new)).to_string().len();
    let runs = change_runs(old, new);
    let num = |n: usize| format!("{n:>width$}");
    let skip_marker = format!(" {} ...", " ".repeat(width));
    let context_line = |new_no: usize, line: &str| format!(" {} {}", num(new_no), line);
    let mut out: Vec<String> = Vec::new();
    let mut old_no = 1usize;
    let mut new_no = 1usize;
    let mut last_was_change = false;
    let mut first_changed: Option<usize> = None;

    for (i, (tag, raw)) in runs.iter().enumerate() {
        match tag {
            ChangeTag::Insert | ChangeTag::Delete => {
                first_changed.get_or_insert(new_no);
                for line in raw {
                    if *tag == ChangeTag::Insert {
                        out.push(format!("+{} {}", num(new_no), line));
                        new_no += 1;
                    } else {
                        out.push(format!("-{} {}", num(old_no), line));
                        old_no += 1;
                    }
                }
                last_was_change = true;
            }
            ChangeTag::Equal => {
                let next_is_change = runs.get(i + 1).is_some_and(|(t, _)| *t != ChangeTag::Equal);
                let n = raw.len();
                if last_was_change && next_is_change {
                    if n <= context * 2 {
                        for line in raw {
                            out.push(context_line(new_no, line));
                            old_no += 1;
                            new_no += 1;
                        }
                    } else {
                        for line in &raw[..context] {
                            out.push(context_line(new_no, line));
                            old_no += 1;
                            new_no += 1;
                        }
                        out.push(skip_marker.clone());
                        let skipped = n - 2 * context;
                        old_no += skipped;
                        new_no += skipped;
                        for line in &raw[n - context..] {
                            out.push(context_line(new_no, line));
                            old_no += 1;
                            new_no += 1;
                        }
                    }
                } else if last_was_change {
                    let shown = n.min(context);
                    for line in &raw[..shown] {
                        out.push(context_line(new_no, line));
                        old_no += 1;
                        new_no += 1;
                    }
                    let skipped = n - shown;
                    if skipped > 0 {
                        out.push(skip_marker.clone());
                        old_no += skipped;
                        new_no += skipped;
                    }
                } else if next_is_change {
                    let skipped = n.saturating_sub(context);
                    if skipped > 0 {
                        out.push(skip_marker.clone());
                        old_no += skipped;
                        new_no += skipped;
                    }
                    for line in &raw[skipped..] {
                        out.push(context_line(new_no, line));
                        old_no += 1;
                        new_no += 1;
                    }
                } else {
                    old_no += n;
                    new_no += n;
                }
                last_was_change = false;
            }
        }
    }
    (out.join("\n"), first_changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bom_and_line_ending_detection() {
        assert_eq!(strip_bom("\u{feff}x"), ("\u{feff}", "x"));
        assert_eq!(strip_bom("x"), ("", "x"));
        assert_eq!(detect_line_ending("a\r\nb\n"), "\r\n");
        assert_eq!(detect_line_ending("a\nb\r\n"), "\n");
        assert_eq!(detect_line_ending("no newline"), "\n");
        assert_eq!(detect_line_ending("a\rb\nc"), "\n");
    }

    #[test]
    fn normalise_folds_quotes_dashes_and_spaces() {
        assert_eq!(
            normalize_for_fuzzy_match("\u{201C}a\u{201D} \u{2018}b\u{2019}"),
            "\"a\" 'b'"
        );
        assert_eq!(normalize_for_fuzzy_match("1\u{2013}2\u{2014}3"), "1-2-3");
        assert_eq!(normalize_for_fuzzy_match("x\u{00A0}y   \nz\t"), "x y\nz");
        assert_eq!(normalize_for_fuzzy_match("\u{FF21}\u{FF0C}"), "A,");
    }

    #[test]
    fn line_ending_at_follows_pi() {
        assert_eq!(line_ending_at("a\r\nb", 0), "\r\n");
        assert_eq!(line_ending_at("a\nb\r\n", 2), "\r\n");
        assert_eq!(line_ending_at("a\rb", 0), "\r");
        assert_eq!(line_ending_at("a\nb", 2), "\n");
    }

    #[test]
    fn display_diff_skips_long_context() {
        let old: String = (1..=20).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l10\n", "L10\n");
        let (diff, first) = display_diff(&old, &new, 2);
        assert_eq!(first, Some(10));
        // Width 2 (21 lines). The skip marker is " " + two-space number field + " ...".
        assert_eq!(
            diff,
            "    ...\n  8 l8\n  9 l9\n-10 l10\n+10 L10\n 11 l11\n 12 l12\n    ..."
        );
    }

    #[test]
    fn context_lines_use_new_line_numbers() {
        // Insertion: two lines inserted after line 33, the unchanged line 34 must be shown
        // with its new-file number 36 (33 + 2 inserted + the original 33 stays 33).
        let old: String = (1..=40).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l33\n", "l33\nNEW1\nNEW2\n");
        let (diff, _) = display_diff(&old, &new, 1);
        let lines: Vec<&str> = diff.lines().collect();
        assert!(lines.contains(&"+34 NEW1"));
        assert!(lines.contains(&"+35 NEW2"));
        assert!(lines.contains(&" 36 l34"), "diff:\n{diff}");

        // Deletion: deleting line 5 shifts the context line below it from 6 to 5.
        let old: String = (1..=10).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l5\n", "");
        let (diff, _) = display_diff(&old, &new, 1);
        let lines: Vec<&str> = diff.lines().collect();
        // Context lines are rendered as ` {number:>width} `, so check the trimmed number.
        let ctx = |lines: &[&str], n: usize| {
            lines
                .iter()
                .any(|l| l.trim_start().starts_with(&format!("{n} l")))
        };
        assert!(ctx(&lines, 5), "diff:\n{diff}");
        assert!(!ctx(&lines, 6), "diff:\n{diff}");

        // Replacement: a one-to-one change keeps numbers in sync (no shift).
        let old: String = (1..=10).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l5\n", "L5\n");
        let (diff, _) = display_diff(&old, &new, 1);
        let lines: Vec<&str> = diff.lines().collect();
        assert!(ctx(&lines, 6), "diff:\n{diff}");
    }
}
