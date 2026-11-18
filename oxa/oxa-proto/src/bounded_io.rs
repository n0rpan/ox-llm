//! Byte budgets for serializers writing to a local buffer or counting sink.
use std::io::{self, Write};

pub struct BoundedWriter<W, E> {
    output: W,
    remaining: usize,
    exceeded: E,
}

impl<W: Write, E: Fn() -> io::Error> BoundedWriter<W, E> {
    pub fn new(output: W, limit: usize, exceeded: E) -> Self {
        Self {
            output,
            remaining: limit,
            exceeded,
        }
    }
}

impl<W: Write, E: Fn() -> io::Error> Write for BoundedWriter<W, E> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err((self.exceeded)());
        }
        let written = self.output.write(bytes)?;
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
