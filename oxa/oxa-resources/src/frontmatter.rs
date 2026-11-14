//! Architect-owned (W0, implemented): the hand-rolled frontmatter reader shared by skills,
//! templates and subagent definitions. No YAML dependency.

/// Split a Markdown document into its frontmatter pairs and its body.
///
/// The frontmatter is the block between a first line that is exactly `---` and the next line
/// that is exactly `---` (trailing spaces and a `\r` are tolerated on both; a leading BOM is
/// skipped). Inside it, every `key: value` line yields a pair, in file order:
///
/// * the key is the text before the first `:`, trimmed; lines without `:`, blank lines, lines
///   starting with `#` and indented lines (nested YAML, list items) are skipped;
/// * the value is trimmed; one pair of matching surrounding quotes (`"` or `'`) is removed; in
///   a double-quoted value `\"` and `\\` are unescaped; an unquoted value ends at ` #`
///   (a comment);
/// * a value of `|` or `>` (block scalar) takes the following indented lines, dedented, joined
///   with `\n` (`|`) or with one space (`>`).
///
/// Without a complete frontmatter block the result is `(vec![], src)` (BOM still skipped).
/// The body starts after the closing fence's line break.
pub fn parse_frontmatter(src: &str) -> (Vec<(String, String)>, &str) {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    let mut lines = LineIter { rest: src };
    match lines.next() {
        Some(first) if is_fence(first) => {}
        _ => return (Vec::new(), src),
    }
    let mut raw: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if is_fence(line) {
            closed = true;
            break;
        }
        raw.push(line.strip_suffix('\r').unwrap_or(line));
    }
    if !closed {
        return (Vec::new(), src);
    }
    let body = lines.rest;
    let mut pairs = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let line = raw[i];
        i += 1;
        if line.trim().is_empty() || line.starts_with([' ', '\t', '#']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        if value == "|" || value == ">" || value == "|-" || value == ">-" {
            let mut block: Vec<&str> = Vec::new();
            while i < raw.len() && (raw[i].starts_with([' ', '\t']) || raw[i].trim().is_empty()) {
                block.push(raw[i]);
                i += 1;
            }
            while block.last().is_some_and(|l| l.trim().is_empty()) {
                block.pop();
            }
            let indent = block
                .iter()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.len() - l.trim_start().len())
                .min()
                .unwrap_or(0);
            let parts: Vec<&str> = block
                .iter()
                .map(|l| if l.len() >= indent { &l[indent..] } else { l.trim() })
                .collect();
            let sep = if value.starts_with('|') { "\n" } else { " " };
            pairs.push((key.to_string(), parts.join(sep)));
        } else {
            pairs.push((key.to_string(), unquote(value)));
        }
    }
    (pairs, body)
}

/// First value of `key` in parsed frontmatter pairs.
pub fn frontmatter_get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// `true` for `true`, `yes`, `on`, `1` (case-insensitive); everything else is `false`.
pub fn frontmatter_bool(pairs: &[(String, String)], key: &str) -> bool {
    matches!(
        frontmatter_get(pairs, key)
            .map(|v| v.to_ascii_lowercase())
            .as_deref(),
        Some("true" | "yes" | "on" | "1")
    )
}

fn is_fence(line: &str) -> bool {
    line.trim_end_matches([' ', '\t', '\r']) == "---"
}

fn unquote(v: &str) -> String {
    let b = v.as_bytes();
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        let inner = &v[1..v.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        return out;
    }
    if b.len() >= 2 && b[0] == b'\'' && b[b.len() - 1] == b'\'' {
        return v[1..v.len() - 1].replace("''", "'");
    }
    match v.find(" #") {
        Some(i) => v[..i].trim_end().to_string(),
        None => v.to_string(),
    }
}

/// Lines without their `\n`, keeping track of the unread remainder.
struct LineIter<'a> {
    rest: &'a str,
}

impl<'a> Iterator for LineIter<'a> {
    type Item = &'a str;
    fn next(&mut self) -> Option<&'a str> {
        if self.rest.is_empty() {
            return None;
        }
        match self.rest.find('\n') {
            Some(i) => {
                let line = &self.rest[..i];
                self.rest = &self.rest[i + 1..];
                Some(line)
            }
            None => {
                let line = self.rest;
                self.rest = "";
                Some(line)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(src: &str) -> Vec<(String, String)> {
        parse_frontmatter(src).0
    }

    #[test]
    fn plain_pairs_and_body() {
        let (p, body) =
            parse_frontmatter("---\nname: pdf-tools\ndescription: Work with PDFs\n---\n# Body\ntext\n");
        assert_eq!(
            p,
            vec![
                ("name".into(), "pdf-tools".into()),
                ("description".into(), "Work with PDFs".into())
            ]
        );
        assert_eq!(body, "# Body\ntext\n");
    }

    #[test]
    fn quotes_are_stripped() {
        let p = pairs("---\nargument-hint: \"[focus]\"\na: 'it''s'\nb: \"say \\\"hi\\\"\"\nc: don't\n---\n");
        assert_eq!(frontmatter_get(&p, "argument-hint"), Some("[focus]"));
        assert_eq!(frontmatter_get(&p, "a"), Some("it's"));
        assert_eq!(frontmatter_get(&p, "b"), Some("say \"hi\""));
        assert_eq!(frontmatter_get(&p, "c"), Some("don't"));
    }

    #[test]
    fn no_or_unclosed_frontmatter_is_all_body() {
        assert_eq!(parse_frontmatter("hello\n---\n"), (vec![], "hello\n---\n"));
        assert_eq!(parse_frontmatter("---\nname: x\n"), (vec![], "---\nname: x\n"));
        assert_eq!(parse_frontmatter(""), (vec![], ""));
    }

    #[test]
    fn crlf_bom_comments_and_nested_lines() {
        let (p, body) = parse_frontmatter("\u{feff}---\r\nname: x # the name\r\n# comment\r\nmetadata:\r\n  k: v\r\nurl: http://a/b\r\n---\r\nbody");
        assert_eq!(
            p,
            vec![
                ("name".into(), "x".into()),
                ("metadata".into(), "".into()),
                ("url".into(), "http://a/b".into())
            ]
        );
        assert_eq!(body, "body");
    }

    #[test]
    fn block_scalars() {
        let p = pairs("---\ndescription: >\n  one\n  two\nnotes: |\n  a\n   b\nlast: z\n---\n");
        assert_eq!(frontmatter_get(&p, "description"), Some("one two"));
        assert_eq!(frontmatter_get(&p, "notes"), Some("a\n b"));
        assert_eq!(frontmatter_get(&p, "last"), Some("z"));
    }

    #[test]
    fn bools_and_empty_body() {
        let (p, body) = parse_frontmatter("---\ndisable-model-invocation: True\nx: no\n---");
        assert!(frontmatter_bool(&p, "disable-model-invocation"));
        assert!(!frontmatter_bool(&p, "x"));
        assert!(!frontmatter_bool(&p, "missing"));
        assert_eq!(body, "");
    }
}
