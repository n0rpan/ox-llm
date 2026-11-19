//! Byte-wise SSE framing. Callers own limits, JSON, EOF and terminal policy.

/// One framed item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// The joined data fields of one event, without the final framing newline.
    Data(Vec<u8>),
    /// A comment line: the bytes after the colon, not stripped.
    Comment(Vec<u8>),
}

/// Lines end at `\n`, `\r\n` or a lone `\r`. A blank line dispatches the data fields seen
/// (joined with `\n`) and nothing when there were none. Other fields are ignored. Bytes are not
/// decoded. There are no limits here: bound the buffers with the two accessors.
#[derive(Default)]
pub struct Framer {
    line: Vec<u8>,
    data: Vec<u8>,
    after_cr: bool,
}

impl Framer {
    pub fn push(&mut self, byte: u8) -> Option<Frame> {
        if byte == b'\n' && self.after_cr {
            self.after_cr = false;
            return None;
        }
        self.after_cr = byte == b'\r';
        if !matches!(byte, b'\r' | b'\n') {
            self.line.push(byte);
            return None;
        }
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            if self.data.is_empty() {
                return None;
            }
            let mut data = std::mem::take(&mut self.data);
            data.pop();
            return Some(Frame::Data(data));
        }
        if let Some(comment) = line.strip_prefix(b":") {
            return Some(Frame::Comment(comment.to_vec()));
        }
        let data = if line == b"data" {
            Some(&b""[..])
        } else {
            line.strip_prefix(b"data:")
        };
        if let Some(data) = data {
            self.data
                .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
            self.data.push(b'\n');
        }
        None
    }

    /// Bytes of the unfinished line.
    pub fn pending_line(&self) -> usize {
        self.line.len()
    }

    /// Bytes of the data fields of the unfinished event.
    pub fn pending_data(&self) -> usize {
        self.data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{Frame, Framer};

    fn frames(chunks: &[&[u8]]) -> Vec<Frame> {
        let mut framer = Framer::default();
        chunks
            .iter()
            .flat_map(|chunk| chunk.iter())
            .filter_map(|byte| framer.push(*byte))
            .collect()
    }
    fn data(text: &[u8]) -> Frame {
        Frame::Data(text.to_vec())
    }
    fn comment(text: &[u8]) -> Frame {
        Frame::Comment(text.to_vec())
    }

    #[test]
    fn frames_preserve_data_without_json_or_terminal_policy() {
        let got: Vec<_> = frames(&[b": ping\r\nevent: ignored\r\ndata: [DONE]\r\n\r\ndata: \xff\n\ndata:  first\rdata:second\r\rdata\r\rdata: pending\n"])
            .into_iter()
            .filter(|f| matches!(f, Frame::Data(_)))
            .collect();
        assert_eq!(
            got,
            vec![data(b"[DONE]"), data(&[0xff]), data(b" first\nsecond"), data(b"")]
        );
    }

    #[test]
    fn split_at_every_byte_offset_gives_the_same_frames() {
        let stream: &[u8] = b": prefill 10/200\r\ndata: one\r\n\r\nevent: error\ndata: a\ndata: b\n\n:\rdata: \xc3\xa9\r\n\r\ndata: [DONE]\n\n";
        let want = vec![
            comment(b" prefill 10/200"),
            data(b"one"),
            data(b"a\nb"),
            comment(b""),
            data("\u{e9}".as_bytes()),
            data(b"[DONE]"),
        ];
        assert_eq!(frames(&[stream]), want);
        for k in 0..=stream.len() {
            let (a, b) = stream.split_at(k);
            assert_eq!(frames(&[a, b]), want, "split at byte {k}");
        }
    }

    #[test]
    fn crlf_split_across_pushes_is_one_line_break() {
        assert_eq!(frames(&[b"data: x\r", b"\n\r", b"\n"]), vec![data(b"x")]);
        assert_eq!(frames(&[b": c\r", b"\n"]), vec![comment(b" c")]);
    }

    #[test]
    fn comments_are_frames_as_soon_as_the_line_ends() {
        let mut framer = Framer::default();
        let mut out = Vec::new();
        for byte in b":keep  alive\n".iter() {
            assert!(out.is_empty());
            out.extend(framer.push(*byte));
        }
        assert_eq!(out, vec![comment(b"keep  alive")]);
        // A comment between data lines does not dispatch or disturb the event.
        assert_eq!(
            frames(&[b"data: a\n:x\ndata: b\n\n"]),
            vec![comment(b"x"), data(b"a\nb")]
        );
    }

    #[test]
    fn blank_lines_without_data_dispatch_nothing() {
        assert!(frames(&[b"\n\r\n\revent: x\nid: 1\n\n"]).is_empty());
    }

    #[test]
    fn pending_accessors_track_the_unfinished_line_and_event() {
        let mut framer = Framer::default();
        for byte in b"data: ab\ndata: c".iter() {
            framer.push(*byte);
        }
        assert_eq!((framer.pending_line(), framer.pending_data()), (7, 3));
        for byte in b"\n\n".iter() {
            framer.push(*byte);
        }
        assert_eq!((framer.pending_line(), framer.pending_data()), (0, 0));
    }
}
