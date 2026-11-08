//! Terminal input.
//!
//! stdin is read by one dedicated thread, outside the runtime, which hands
//! each line to whoever asks next through a channel.
//!
//! The obvious alternative - a `spawn_blocking` read per prompt - cannot be
//! taken back. A blocking `read_line` does not return because its future was
//! dropped, so an approval prompt abandoned by Ctrl-C would leave a read
//! behind that swallows the next line the human types, meant for the chat
//! prompt. With a channel, giving up on a prompt only stops *waiting*: the
//! line, whenever it comes, is still there for the next one.
//!
//! The thread starts on first use, so subcommands that never prompt never
//! read stdin, and it is never joined: it may be blocked in `read` at exit,
//! which is harmless for a thread the runtime does not know about.

use std::io::{BufRead, Write};
use std::sync::OnceLock;

use anyhow::Result;
use tokio::sync::{Mutex, mpsc};

type Line = std::io::Result<Option<String>>;

/// Lines from one reader thread, handed out one per request.
pub struct Lines {
    receiver: Mutex<mpsc::UnboundedReceiver<Line>>,
}

impl Lines {
    /// Starts a thread reading `input` line by line.
    pub fn spawn<R: BufRead + Send + 'static>(mut input: R) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        // A thread that cannot be started drops the sender, which reads as
        // end of input below: the same thing a closed stdin means, and what
        // the callers already handle.
        let _ = std::thread::Builder::new()
            .name("stdin".into())
            .spawn(move || {
                loop {
                    let mut buffer = String::new();
                    let line = match input.read_line(&mut buffer) {
                        Ok(0) => Ok(None),
                        Ok(_) => Ok(Some(buffer)),
                        Err(error) => Err(error),
                    };
                    let more = matches!(line, Ok(Some(_)));
                    if sender.send(line).is_err() || !more {
                        return;
                    }
                }
            });
        Self {
            receiver: Mutex::new(receiver),
        }
    }

    /// The next line, `None` at end of input.
    ///
    /// Cancel-safe: dropping this future before it resolves consumes nothing.
    pub async fn next(&self) -> Line {
        let mut receiver = self.receiver.lock().await;
        // A closed channel means the reader is gone after end of input or an
        // error it already reported; from here on it is end of input.
        receiver.recv().await.unwrap_or(Ok(None))
    }
}

/// The process's stdin, started on first use.
fn stdin() -> &'static Lines {
    static STDIN: OnceLock<Lines> = OnceLock::new();
    // `Stdin` buffers internally but is not `BufRead`; its lock is, but
    // cannot move to the reader thread. This thread is stdin's only reader,
    // so a second buffer in front of it holds nothing anyone else needs.
    STDIN.get_or_init(|| Lines::spawn(std::io::BufReader::new(std::io::stdin())))
}

/// Reads one line from stdin, writing the prompt to **stderr** so stdout stays
/// a clean channel for the model's output.
///
/// Returns `Ok(None)` on EOF (Ctrl-D). Dropping the future abandons the
/// prompt without losing what the human types next.
pub async fn prompt_line(prompt: &str) -> Result<Option<String>> {
    let mut stderr = std::io::stderr();
    write!(stderr, "{prompt}")?;
    stderr.flush()?;

    Ok(stdin().next().await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Read};
    use std::sync::mpsc as std_mpsc;

    /// Input that arrives only when the test types it, like a human would.
    struct Keyboard(std_mpsc::Receiver<Vec<u8>>);

    impl Read for Keyboard {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            match self.0.recv() {
                Ok(bytes) => {
                    buffer[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                // The test hung up: Ctrl-D.
                Err(_) => Ok(0),
            }
        }
    }

    fn keyboard() -> (std_mpsc::Sender<Vec<u8>>, Lines) {
        let (keys, typed) = std_mpsc::channel();
        (keys, Lines::spawn(BufReader::new(Keyboard(typed))))
    }

    #[tokio::test]
    async fn an_abandoned_prompt_does_not_swallow_the_next_line() {
        let (keys, lines) = keyboard();

        // An approval prompt starts waiting, and the turn is interrupted
        // before anyone answers.
        tokio::select! {
            biased;
            line = lines.next() => panic!("nothing was typed yet, got {line:?}"),
            () = std::future::ready(()) => {}
        }

        // What the human types next is for the chat prompt, and it gets it.
        keys.send(b"carry on\n".to_vec()).unwrap();
        let line = lines.next().await.unwrap();
        assert_eq!(line.as_deref(), Some("carry on\n"));
    }

    #[tokio::test]
    async fn lines_come_in_order_and_end_of_input_stays_ended() {
        let (keys, lines) = keyboard();
        keys.send(b"one\n".to_vec()).unwrap();
        keys.send(b"two\n".to_vec()).unwrap();
        drop(keys);

        assert_eq!(lines.next().await.unwrap().as_deref(), Some("one\n"));
        assert_eq!(lines.next().await.unwrap().as_deref(), Some("two\n"));
        assert_eq!(lines.next().await.unwrap(), None);
        assert_eq!(lines.next().await.unwrap(), None, "EOF is not forgotten");
    }
}
