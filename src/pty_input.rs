//! Input to the pty, written from a thread of its own.
//!
//! The frame loop only queues bytes; a writer thread writes them in order at
//! the pace the child reads, as Ghostty's I/O thread writes from a queue
//! (`src/termio/Exec.zig`, `queueWrite`, at a887df42). Writing from the frame
//! loop either froze the window while a child wasn't reading (Windows, a
//! blocking pipe) or dropped what didn't fit in the pty buffer (Unix, a
//! non-blocking master), so a big paste arrived truncated.

use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
};

/// Unwritten bytes past which replies are dropped. Replies are what tai
/// generates for the child (cursor and size reports, for example), so a
/// child that floods queries and never reads its input could otherwise pile
/// them up without limit. What the user types or pastes is never capped: a
/// paste is bounded by the clipboard, and dropping it would look like paste
/// not working.
const MAX_QUEUED_FOR_REPLIES: usize = 8 * 1024 * 1024;

/// The frame loop's end of the writer thread.
pub struct Input {
    pending: Sender<Vec<u8>>,
    /// Bytes queued and not yet written.
    queued: Arc<AtomicUsize>,
    /// Whether the last reply was dropped, so a burst of drops logs once.
    dropping: AtomicBool,
}

impl Input {
    /// Start a writer thread that writes queued input to `writer`. It ends
    /// when the Input is dropped or a write fails (the child has gone).
    pub fn spawn(writer: impl Write + Send + 'static) -> std::io::Result<Self> {
        let (pending, receiver) = mpsc::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let written = Arc::clone(&queued);
        std::thread::Builder::new()
            .name("pty-writer".into())
            .spawn(move || write_loop(writer, &receiver, &written))?;
        Ok(Self {
            pending,
            queued,
            dropping: AtomicBool::new(false),
        })
    }

    /// Queue what the user typed or pasted. Nothing here waits on the pty;
    /// only a write error (the child has gone) drops it.
    pub fn send(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.queued.fetch_add(data.len(), Ordering::Relaxed);
        _ = self.pending.send(data.to_vec());
    }

    /// Queue a reply tai generated for the child. Past
    /// `MAX_QUEUED_FOR_REPLIES` unwritten bytes it's dropped, and logged once
    /// per burst.
    pub fn send_reply(&self, data: &[u8]) {
        if self.queued.load(Ordering::Relaxed) + data.len() > MAX_QUEUED_FOR_REPLIES {
            if !self.dropping.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "dropping replies to the child: it isn't reading its input                      ({} bytes unwritten)",
                    self.queued.load(Ordering::Relaxed)
                );
            }
            return;
        }
        self.dropping.store(false, Ordering::Relaxed);
        self.send(data);
    }
}

fn write_loop(mut writer: impl Write, pending: &Receiver<Vec<u8>>, queued: &AtomicUsize) {
    for data in pending {
        if writer
            .write_all(&data)
            .and_then(|()| writer.flush())
            .is_err()
        {
            return;
        }
        queued.fetch_sub(data.len(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Read,
        time::{Duration, Instant},
    };

    use super::{Input, MAX_QUEUED_FOR_REPLIES};

    #[test]
    fn queued_input_waits_for_a_slow_reader_and_arrives_whole() {
        let (mut reader, writer) = std::io::pipe().unwrap();
        let input = Input::spawn(writer).unwrap();

        // A big paste, far more than a pipe holds, to a child that isn't
        // reading yet: queueing it must not wait on the pipe.
        let sent: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
        let start = Instant::now();
        for chunk in sent.chunks(4096) {
            input.send(chunk);
        }
        assert!(start.elapsed() < Duration::from_millis(500));

        // The child reads it later: all of it arrives, in order.
        std::thread::sleep(Duration::from_millis(200));
        drop(input);
        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        assert_eq!(got.len(), sent.len());
        assert!(got == sent);
    }

    #[test]
    fn replies_past_the_cap_are_dropped_but_pastes_are_not() {
        let (mut reader, writer) = std::io::pipe().unwrap();
        let input = Input::spawn(writer).unwrap();

        // A paste bigger than the cap goes through. It blocks the writer on
        // the full pipe, so it stays counted, and the reply after it would
        // pass the cap and is dropped. Typing after that still goes through.
        let paste = vec![1u8; MAX_QUEUED_FOR_REPLIES + 1];
        input.send(&paste);
        input.send_reply(b"[1;1R");
        input.send(b"a");

        std::thread::sleep(Duration::from_millis(100));
        drop(input);
        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        assert_eq!(got.len(), paste.len() + 1);
        assert!(got[..paste.len()] == paste[..]);
        assert_eq!(got.last(), Some(&b'a'));
    }
}
