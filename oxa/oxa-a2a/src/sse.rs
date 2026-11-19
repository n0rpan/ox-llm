//! Bounded SSE data reader. Input may split anywhere, including inside UTF-8.
//! HTTP timeout/deadline enforcement belongs to the transport feeding this reader.
//!
//! Framing is `oxa_http::sse::Framer`; this adapter owns the limits (counted over every input
//! byte, comments and ignored fields included), UTF-8 validation, the leading BOM and EOF policy.
use oxa_http::sse::{Frame, Framer};
use std::io::{self, BufRead};

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

pub struct SseReader<R> {
    input: R,
    framer: Framer,
    max_record: usize,
    remaining: usize,
    record_bytes: usize,
    /// Bytes of the leading BOM matched so far; `None` once the first line has started.
    bom: Option<usize>,
    /// Incomplete UTF-8 sequence at the end of the input so far (at most three bytes).
    tail: Vec<u8>,
    failed: bool,
}

impl<R: BufRead> SseReader<R> {
    pub fn new(input: R, max_record: usize, max_total: usize) -> Self {
        Self {
            input,
            framer: Framer::default(),
            max_record,
            remaining: max_total,
            record_bytes: 0,
            bom: Some(0),
            tail: Vec::new(),
            failed: false,
        }
    }

    /// Yields one dispatched data record. Comments and non-data fields are
    /// ignored, but still count against limits. An unterminated event at EOF
    /// is discarded, as required by SSE; it never becomes a completed task.
    pub fn next_data(&mut self) -> io::Result<Option<String>> {
        if self.failed {
            return Err(invalid("SSE reader already failed"));
        }
        let result = self.read_record();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Check both limits for one more byte, then consume it.
    fn take(&mut self) -> io::Result<Option<u8>> {
        let Some(&byte) = self.input.fill_buf()?.first() else {
            return Ok(None);
        };
        if self.remaining == 0 || self.record_bytes >= self.max_record {
            return Err(invalid("SSE size limit exceeded"));
        }
        self.input.consume(1);
        self.remaining -= 1;
        self.record_bytes += 1;
        Ok(Some(byte))
    }

    fn read_record(&mut self) -> io::Result<Option<String>> {
        self.record_bytes = 0;
        loop {
            let Some(byte) = self.take()? else {
                // EOF: a partial UTF-8 sequence is invalid, a partial event is dropped.
                return if self.tail.is_empty() {
                    Ok(None)
                } else {
                    Err(invalid("invalid SSE UTF-8"))
                };
            };
            let blank = self.framer.pending_line() == 0;
            let frame = self.push(byte)?;
            if byte == b'\r' && self.input.fill_buf()?.first() == Some(&b'\n') {
                // CRLF is one line break and is counted with the line it ends.
                let lf = self.take()?.unwrap_or(b'\n');
                self.push(lf)?;
            }
            if blank && frame.is_none() && matches!(byte, b'\r' | b'\n') {
                // A blank line that dispatched nothing starts a fresh record budget.
                self.record_bytes = 0;
            }
            if let Some(Frame::Data(data)) = frame {
                return String::from_utf8(data)
                    .map(Some)
                    .map_err(|_| invalid("invalid SSE UTF-8"));
            }
        }
    }

    fn push(&mut self, byte: u8) -> io::Result<Option<Frame>> {
        self.validate(byte)?;
        if let Some(matched) = self.bom {
            if byte == BOM[matched] {
                if matched + 1 == BOM.len() {
                    self.bom = None;
                } else {
                    self.bom = Some(matched + 1);
                }
                return Ok(None);
            }
            // Not a BOM after all: replay the bytes held back, then this one.
            self.bom = None;
            for held in &BOM[..matched] {
                self.framer.push(*held);
            }
        }
        Ok(self.framer.push(byte))
    }

    /// Per-line UTF-8 validity equals stream validity: every line break is ASCII.
    fn validate(&mut self, byte: u8) -> io::Result<()> {
        if self.tail.is_empty() && byte.is_ascii() {
            return Ok(());
        }
        self.tail.push(byte);
        match std::str::from_utf8(&self.tail) {
            Ok(_) => self.tail.clear(),
            Err(e) if e.error_len().is_none() => {}
            Err(_) => return Err(invalid("invalid SSE UTF-8")),
        }
        Ok(())
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
