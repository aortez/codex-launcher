//! Running a helper program with a deadline and an output limit.

use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How a bounded run ended.
pub enum Outcome {
    /// The program exited, and its output fit the limit.
    Exited(ExitStatus, Vec<u8>),
    /// The program wrote more than the limit to stdout.
    TooLarge,
    /// The deadline passed before the program exited.
    TimedOut,
}

/// Run `command` with no input, keeping up to `max_bytes` of its stdout, until
/// it exits or `deadline` passes. Its stderr is discarded, since it would draw
/// over the menu.
///
/// The program runs in its own process group, which is killed afterwards:
/// wrappers may leave descendants behind even after they exit.
pub fn run(command: &mut Command, deadline: Instant, max_bytes: u64) -> io::Result<Outcome> {
    let mut output = tempfile::tempfile()?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let group = rustix::process::Pid::from_child(&child);
    let outcome = (|| {
        loop {
            if output.metadata()?.len() > max_bytes {
                return Ok(Outcome::TooLarge);
            }
            if let Some(status) = child.try_wait()? {
                output.seek(SeekFrom::Start(0))?;
                let mut bytes = Vec::new();
                (&mut output).take(max_bytes + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > max_bytes {
                    return Ok(Outcome::TooLarge);
                }
                return Ok(Outcome::Exited(status, bytes));
            }
            if Instant::now() >= deadline {
                return Ok(Outcome::TimedOut);
            }
            std::thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    })();
    // Terminate the group on every path, then reap the immediate child.
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    let _ = child.wait();
    outcome
}
