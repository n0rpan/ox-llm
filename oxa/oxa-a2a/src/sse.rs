//! Bounded SSE data reader. Input may split anywhere, including inside UTF-8.
//! HTTP timeout/deadline enforcement belongs to the transport feeding this reader.
use std::io::{self, BufRead};

pub struct SseReader<R> {
    input: R,
    max_record: usize,
    remaining: usize,
    first_line: bool,
    failed: bool,
}

impl<R: BufRead> SseReader<R> {
    pub fn new(input: R, max_record: usize, max_total: usize) -> Self {
        Self {
            input,
            max_record,
            remaining: max_total,
            first_line: true,
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

    fn read_record(&mut self) -> io::Result<Option<String>> {
        let mut data = String::new();
        let mut has_data = false;
        let mut record_bytes = 0usize;
        loop {
            let mut line = Vec::new();
            let mut saw_input = false;
            // Read a line without allowing read_until to allocate unboundedly.
            loop {
                let buf = self.input.fill_buf()?;
                if buf.is_empty() {
                    break;
                }
                let n = buf
                    .iter()
                    .position(|b| *b == b'\n' || *b == b'\r')
                    .map_or(buf.len(), |i| i + 1);
                if n > self.remaining || n > self.max_record.saturating_sub(record_bytes) {
                    return Err(invalid("SSE size limit exceeded"));
                }
                let delimiter = matches!(buf[n - 1], b'\r' | b'\n');
                let cr = buf[n - 1] == b'\r';
                line.extend_from_slice(&buf[..n]);
                self.input.consume(n);
                self.remaining -= n;
                record_bytes += n;
                saw_input = true;
                if delimiter {
                    line.pop();
                    if cr && self.input.fill_buf()?.first() == Some(&b'\n') {
                        if self.remaining == 0 || record_bytes == self.max_record {
                            return Err(invalid("SSE size limit exceeded"));
                        }
                        self.input.consume(1);
                        self.remaining -= 1;
                        record_bytes += 1;
                    }
                    break;
                }
            }
            if !saw_input {
                return Ok(None);
            }
            let mut line = std::str::from_utf8(&line).map_err(|_| invalid("invalid SSE UTF-8"))?;
            if self.first_line {
                self.first_line = false;
                line = line.strip_prefix('\u{feff}').unwrap_or(line);
            }
            if line.is_empty() {
                if has_data {
                    data.pop(); // remove the newline after the last data field
                    return Ok(Some(data));
                }
                record_bytes = 0;
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            if field == "data" {
                has_data = true;
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
                data.push('\n');
            }
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
