//! Shared SSE data framing. Callers own limits, JSON, EOF and terminal policy.

#[derive(Default)]
pub(crate) struct Framer {
    line: Vec<u8>,
    data: Vec<u8>,
    after_cr: bool,
}

impl Framer {
    /// Return one assembled data field, without its final framing newline.
    pub(crate) fn push(&mut self, byte: u8) -> Option<Vec<u8>> {
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
            return Some(data);
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
}

#[cfg(test)]
mod tests {
    use super::Framer;

    #[test]
    fn frames_preserve_data_without_json_or_terminal_policy() {
        let mut framer = Framer::default();
        let frames: Vec<_> = b": ping\r\nevent: ignored\r\ndata: [DONE]\r\n\r\ndata: \xff\n\ndata:  first\rdata:second\r\rdata\r\rdata: pending\n"
            .iter()
            .filter_map(|byte| framer.push(*byte))
            .collect();
        assert_eq!(
            frames,
            vec![b"[DONE]".to_vec(), vec![0xff], b" first\nsecond".to_vec(), vec![]]
        );
    }
}
