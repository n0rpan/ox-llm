//! Pure HTTP/1.1 request framing for the hand-written servers. Each server keeps its own limits,
//! timeouts, status codes and routing; nothing here decides policy.
use std::io::{ErrorKind, Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadError {
    /// End of input before the blank line.
    Closed,
    TooLarge,
    NotUtf8,
    RequestLine,
    HeaderSyntax,
    HeaderName,
    HeaderValue,
    Io(ErrorKind),
}

const END: &[u8] = b"\r\n\r\n";

/// Read byte-wise until CRLFCRLF and never past it. The caller sets the socket timeouts.
/// A head of exactly `max_bytes` bytes, terminator included, is accepted.
pub fn read_head<R: Read>(reader: &mut R, max_bytes: usize) -> Result<Vec<u8>, HeadError> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => return Err(HeadError::Closed),
            Ok(_) => head.push(byte[0]),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(HeadError::Io(e.kind())),
        }
        if head.ends_with(END) {
            return Ok(head);
        }
        if head.len() >= max_bytes {
            return Err(HeadError::TooLarge);
        }
    }
}

pub struct RequestLine<'a> {
    pub method: &'a str,
    pub target: &'a str,
    pub version: &'a str,
}

/// Split a CRLFCRLF-terminated head into (request line, header block). The block has no final
/// CRLF and is empty when there are no headers.
pub fn split_head(head: &[u8]) -> Result<(&str, &str), HeadError> {
    let text = std::str::from_utf8(head).map_err(|_| HeadError::NotUtf8)?;
    let text = text.strip_suffix("\r\n\r\n").ok_or(HeadError::Closed)?;
    Ok(text.split_once("\r\n").unwrap_or((text, "")))
}

/// Exactly three non-empty parts separated by single spaces, no control byte, version `HTTP/..`.
pub fn parse_request_line(line: &str) -> Result<RequestLine<'_>, HeadError> {
    let parts: Vec<_> = line.split(' ').collect();
    let [method, target, version] = parts.as_slice() else {
        return Err(HeadError::RequestLine);
    };
    if parts.iter().any(|p| p.is_empty())
        || line.bytes().any(|b| b.is_ascii_control())
        || !version.starts_with("HTTP/")
    {
        return Err(HeadError::RequestLine);
    }
    Ok(RequestLine {
        method,
        target,
        version,
    })
}

/// Header fields in order: names as sent, values trimmed.
pub struct Headers<'a>(Vec<(&'a str, &'a str)>);

/// Every line has a colon; the name is non-empty `[A-Za-z0-9-]`; the value has no control byte
/// except HTAB.
pub fn parse_headers(block: &str) -> Result<Headers<'_>, HeadError> {
    let mut fields = Vec::new();
    if block.is_empty() {
        return Ok(Headers(fields));
    }
    for line in block.split("\r\n") {
        let (name, value) = line.split_once(':').ok_or(HeadError::HeaderSyntax)?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(HeadError::HeaderName);
        }
        if value.bytes().any(|b| b.is_ascii_control() && b != b'\t') {
            return Err(HeadError::HeaderValue);
        }
        fields.push((name, value.trim()));
    }
    Ok(Headers(fields))
}

impl<'a> Headers<'a> {
    /// The first value of `name` (ASCII case-insensitive).
    pub fn get(&self, name: &str) -> Option<&'a str> {
        self.all(name).next()
    }
    pub fn all<'s>(&'s self, name: &'s str) -> impl Iterator<Item = &'a str> + 's {
        self.0
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| *v)
    }
    pub fn count(&self, name: &str) -> usize {
        self.all(name).count()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&'a str, &'a str)> + '_ {
        self.0.iter().copied()
    }
}

/// At most one `Content-Length`, canonical decimal only (`010`, `+1`, ` -1` are errors).
pub fn content_length(headers: &Headers<'_>) -> Result<Option<usize>, HeadError> {
    let mut values = headers.all("content-length");
    let Some(text) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(HeadError::HeaderValue);
    }
    let canonical = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    match text.parse::<usize>() {
        Ok(n) if canonical => Ok(Some(n)),
        _ => Err(HeadError::HeaderValue),
    }
}

/// Write a complete response; adds `Content-Length` and `Connection: close`. `status` is e.g.
/// `200 OK`. A control byte in the status or a header is refused as `InvalidInput`.
pub fn write_response<W: Write>(
    w: &mut W,
    status: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    let bad = |s: &str| s.bytes().any(|b| b.is_ascii_control() && b != b'\t');
    if bad(status) || headers.iter().any(|(n, v)| bad(n) || bad(v) || n.is_empty()) {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "control byte in response head",
        ));
    }
    let mut out = format!("HTTP/1.1 {status}\r\n").into_bytes();
    for (name, value) in headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(body);
    w.write_all(&out)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn read_head_stops_at_the_blank_line_and_refuses_the_rest() {
        let mut ok = Cursor::new(b"GET / HTTP/1.1\r\nHost: a\r\n\r\nBODY".to_vec());
        assert_eq!(
            read_head(&mut ok, 64).unwrap(),
            b"GET / HTTP/1.1\r\nHost: a\r\n\r\n"
        );
        let mut rest = Vec::new();
        ok.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"BODY", "never reads past the terminator");
        let exact = b"GET / HTTP/1.1\r\n\r\n";
        assert!(read_head(&mut Cursor::new(exact.to_vec()), exact.len()).is_ok());
        let cases: [(&[u8], usize, HeadError); 5] = [
            (b"GET / HTTP/1.1\r\n\r\n", 17, HeadError::TooLarge),
            (b"GET / HTTP/1.1\r\nHost: a\r\n", 64, HeadError::Closed),
            (b"", 64, HeadError::Closed),
            (b"GET / HTTP/1.1\nHost: a\n\n", 64, HeadError::Closed),
            (&[b'a'; 100], 32, HeadError::TooLarge),
        ];
        for (input, max, want) in cases {
            assert_eq!(
                read_head(&mut Cursor::new(input.to_vec()), max),
                Err(want),
                "{input:?}"
            );
        }
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(ErrorKind::TimedOut.into())
            }
        }
        assert_eq!(read_head(&mut Broken, 8), Err(HeadError::Io(ErrorKind::TimedOut)));
    }

    #[test]
    fn split_head_separates_line_and_block() {
        assert_eq!(
            split_head(b"GET / HTTP/1.1\r\nA: b\r\nC: d\r\n\r\n"),
            Ok(("GET / HTTP/1.1", "A: b\r\nC: d"))
        );
        assert_eq!(split_head(b"GET / HTTP/1.1\r\n\r\n"), Ok(("GET / HTTP/1.1", "")));
        assert_eq!(split_head(b"GET / HTTP/1.1\r\n"), Err(HeadError::Closed));
        assert_eq!(split_head(b"GET /\xff HTTP/1.1\r\n\r\n"), Err(HeadError::NotUtf8));
    }

    #[test]
    fn request_line_is_exactly_three_single_space_parts() {
        let ok = parse_request_line("POST /chat HTTP/1.1").unwrap();
        assert_eq!((ok.method, ok.target, ok.version), ("POST", "/chat", "HTTP/1.1"));
        for bad in [
            "GET /",
            "GET / ",
            "GET  / HTTP/1.1",
            "GET / HTTP/1.1 x",
            "GET / HTTP/1.1\n",
            "GET /a\tb HTTP/1.1",
            "GET / 1.1",
            "",
        ] {
            assert_eq!(
                parse_request_line(bad).err(),
                Some(HeadError::RequestLine),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn headers_are_strict_and_case_insensitive_to_read() {
        let h = parse_headers("Host: a\r\nX-A:\t b \r\nx-a: c").unwrap();
        assert_eq!(h.get("HOST"), Some("a"));
        assert_eq!(h.all("x-a").collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!((h.count("x-a"), h.count("none"), h.iter().count()), (2, 0, 3));
        assert_eq!(h.iter().next(), Some(("Host", "a")));
        assert_eq!(parse_headers("").unwrap().iter().count(), 0);
        let cases = [
            ("no colon", HeadError::HeaderSyntax),
            (": v", HeadError::HeaderName),
            ("Bad Name: v", HeadError::HeaderName),
            ("Bad_Name: v", HeadError::HeaderName),
            (" Lead: v", HeadError::HeaderName),
            ("A: v\u{0}x", HeadError::HeaderValue),
            ("A: v\rx", HeadError::HeaderValue),
            ("A: v\nB: w", HeadError::HeaderValue),
            ("A: \u{7f}", HeadError::HeaderValue),
        ];
        for (block, want) in cases {
            assert_eq!(parse_headers(block).err(), Some(want), "{block:?}");
        }
    }

    #[test]
    fn content_length_is_single_and_canonical() {
        let len = |block| content_length(&parse_headers(block).unwrap());
        assert_eq!(len(""), Ok(None));
        assert_eq!(len("Content-Length: 0"), Ok(Some(0)));
        assert_eq!(len("content-length: 4096"), Ok(Some(4096)));
        for bad in [
            "Content-Length: 010",
            "Content-Length: +1",
            "Content-Length: -1",
            "Content-Length: 1.0",
            "Content-Length: 0x10",
            "Content-Length:",
            "Content-Length: 1 2",
            "Content-Length: 99999999999999999999999",
            "Content-Length: 1\r\ncontent-length: 1",
            "Content-Length: 1\r\nContent-Length: 2",
        ] {
            assert_eq!(len(bad), Err(HeadError::HeaderValue), "{bad:?}");
        }
    }

    #[test]
    fn write_response_adds_length_and_close() {
        let mut out = Vec::new();
        write_response(
            &mut out,
            "404 Not Found",
            &[("Content-Type", "text/plain")],
            b"nope",
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope"
        );
        for (status, headers) in [
            ("200 OK\r\nX: y", vec![]),
            ("200 OK", vec![("A", "b\r\nC: d")]),
            ("200 OK", vec![("", "v")]),
        ] {
            let err = write_response(&mut Vec::new(), status, &headers, b"").unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput);
        }
    }
}
