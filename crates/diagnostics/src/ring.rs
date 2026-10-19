// SPDX-License-Identifier: GPL-3.0-or-later

//! The flight recorder: a bounded ring of the most recent formatted events,
//! kept in memory so a crash report can carry what led up to it.
//!
//! The ring's own filter is wider than the log file's, so an event that
//! never reaches the file is still in the report.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// How many formatted event lines the ring holds -- roughly 100 KB of text
/// at the length a formatted event usually reaches.
pub(crate) const RING_CAPACITY: usize = 512;

/// A cloneable, thread-safe handle to the flight recorder.
#[derive(Clone)]
pub(crate) struct Ring {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl Ring {
    pub(crate) fn new() -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY))),
        }
    }

    /// The lines currently held, oldest first.
    ///
    /// Taken with `try_lock`: a thread that panics while formatting an
    /// event still holds the lock when the panic hook runs on that same
    /// thread, and a blocking lock there would deadlock the process it is
    /// meant to report on. A held or poisoned lock yields no lines.
    pub(crate) fn dump(&self) -> Vec<String> {
        match self.lines.try_lock() {
            Ok(lines) => lines.iter().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }
}

impl Write for Ring {
    /// One call is one formatted event: `fmt::Layer` writes each event with
    /// a single `write_all`, so the buffer is stored as one line.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let line = String::from_utf8_lossy(buf).trim_end().to_string();
        let mut lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        if lines.len() == RING_CAPACITY {
            lines.pop_front();
        }
        lines.push_back(line);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Ring {
    type Writer = Ring;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn ring_keeps_the_most_recent_lines_and_drops_the_oldest() {
        let ring = Ring::new();
        let mut writer = ring.clone();
        for i in 0..RING_CAPACITY * 2 {
            writer.write_all(format!("event {i}\n").as_bytes()).unwrap();
        }
        let dump = ring.dump();
        assert_eq!(dump.len(), RING_CAPACITY);
        assert_eq!(dump[0], format!("event {RING_CAPACITY}"));
        assert_eq!(
            dump[RING_CAPACITY - 1],
            format!("event {}", RING_CAPACITY * 2 - 1)
        );
    }

    #[test]
    fn ring_dump_is_empty_while_the_lock_is_held() {
        let ring = Ring::new();
        ring.clone().write_all(b"kept\n").unwrap();
        let held = ring.lines.lock().unwrap();
        assert!(ring.dump().is_empty());
        drop(held);
        assert_eq!(ring.dump(), vec!["kept".to_string()]);
    }
}
