//! C13: fuzzy matching. Owner: task C13. Signatures below are frozen; bodies are yours.
// ported from pi packages/tui/src/fuzzy.ts

/// Result of a fuzzy match. Lower `score` = better (pi's convention); `score` is meaningful
/// only when `matches`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

/// Lowercase a string per character, keeping one `char` per input `char` so that indices into
/// the result are character indices of the input.
fn lower_chars(s: &str) -> Vec<char> {
    s.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect()
}

/// pi's word-boundary set: whitespace and `- _ . / :`.
fn is_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '-' | '_' | '.' | '/' | ':')
}

/// pi's `scorePositions`. `positions` are ascending character indices into `text`.
/// Note pi starts `lastMatchIndex` at -1, so a match at index 0 counts as consecutive.
fn score_positions(text: &[char], positions: &[i64]) -> f64 {
    let mut score = 0.0_f64;
    let mut last: i64 = -1;
    let mut consecutive: i64 = 0;
    for &i in positions {
        if last == i - 1 {
            consecutive += 1;
            score -= consecutive as f64 * 5.0;
        } else {
            consecutive = 0;
            if last >= 0 {
                score += ((i - last - 1) * 2) as f64;
            }
        }
        if i == 0 || is_boundary(text[(i - 1) as usize]) {
            score -= 10.0;
        }
        score += i as f64 * 0.1;
        last = i;
    }
    score
}

/// pi's `matchQuery` on already-lowercased character slices.
fn match_query(query: &[char], text: &[char]) -> FuzzyMatch {
    if query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if query.len() > text.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    // Greedy alignment: each query character at its first occurrence after the previous one.
    let mut positions: Vec<i64> = Vec::with_capacity(query.len());
    let mut last: usize = 0; // first index that may still be used
    for &qc in query {
        match text[last..].iter().position(|&c| c == qc) {
            Some(off) => {
                let i = last + off;
                positions.push(i as i64);
                last = i + 1;
            }
            None => {
                return FuzzyMatch {
                    matches: false,
                    score: 0.0,
                }
            }
        }
    }

    let mut score = score_positions(text, &positions);

    // The greedy alignment can scatter the query over an earlier, unrelated part of the text
    // (pi: "opus" in "openrouter/anthropic/claude-opus"). Also score every contiguous
    // occurrence and keep the best alignment.
    for start in 0..=(text.len() - query.len()) {
        if text[start..start + query.len()] == *query {
            for (k, p) in positions.iter_mut().enumerate() {
                *p = (start + k) as i64;
            }
            score = score.min(score_positions(text, &positions));
        }
    }

    if query == text {
        score -= 100.0;
    }

    FuzzyMatch { matches: true, score }
}

/// `^[a-z]+[0-9]+$` or `^[0-9]+[a-z]+$` on a lowercased query: returns the letters-digits
/// swapped form (pi's retry), or `None`.
fn swapped_query(query: &[char]) -> Option<Vec<char>> {
    let is_letter = |c: &char| c.is_ascii_lowercase();
    let is_digit = |c: &char| c.is_ascii_digit();
    // letters then digits
    let letters_end = query.iter().take_while(|c| is_letter(c)).count();
    if letters_end > 0 && letters_end < query.len() && query[letters_end..].iter().all(is_digit) {
        let mut out: Vec<char> = query[letters_end..].to_vec();
        out.extend_from_slice(&query[..letters_end]);
        return Some(out);
    }
    // digits then letters
    let digits_end = query.iter().take_while(|c| is_digit(c)).count();
    if digits_end > 0 && digits_end < query.len() && query[digits_end..].iter().all(is_letter) {
        let mut out: Vec<char> = query[digits_end..].to_vec();
        out.extend_from_slice(&query[..digits_end]);
        return Some(out);
    }
    None
}

/// Match `query` against `text`: every query character must occur in order, case-insensitively.
/// Scoring as pi: consecutive matches and matches at word boundaries (after `/ - _ .` space, or
/// a lower-to-upper case change) are rewarded, gaps are penalised; an empty query matches
/// everything with score 0. If the query does not match as typed, pi's swapped
/// letters-digits retry applies (`"v2ab"` vs `"ab2v"` forms).
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let q = lower_chars(query);
    let t = lower_chars(text);

    let primary = match_query(&q, &t);
    if primary.matches {
        return primary;
    }

    match swapped_query(&q) {
        None => primary,
        Some(swapped) => {
            let m = match_query(&swapped, &t);
            if m.matches {
                FuzzyMatch {
                    matches: true,
                    score: m.score + 5.0,
                }
            } else {
                primary
            }
        }
    }
}

/// Indices of the items that match, best first; ties keep input order (stable). A query with
/// spaces is split into tokens that must all match (scores summed).
///
/// Tokens are split on whitespace and `/` (pi's `[\s/]+`), so `"src/ma"` means `src` and `ma`.
/// An empty query returns every index in input order.
pub fn fuzzy_filter(items: &[String], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..items.len()).collect();
    }
    let tokens: Vec<&str> = query
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return (0..items.len()).collect();
    }

    let mut results: Vec<(usize, f64)> = Vec::new();
    'items: for (idx, text) in items.iter().enumerate() {
        let mut total = 0.0_f64;
        for token in &tokens {
            let m = fuzzy_match(token, text);
            if !m.matches {
                continue 'items;
            }
            total += m.score;
        }
        results.push((idx, total));
    }
    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    results.into_iter().map(|(idx, _)| idx).collect()
}
