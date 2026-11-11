//! Running a helper program with a deadline and an output limit.

use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};

/// The process groups of helpers that are still running.
static RUNNING: Mutex<Running> = Mutex::new(Running {
    groups: Vec::new(),
    stopped: false,
});

struct Running {
    groups: Vec<Pid>,
    /// Whether the launcher is exiting, so that no more helpers may start.
    stopped: bool,
}

fn running() -> MutexGuard<'static, Running> {
    RUNNING.lock().unwrap_or_else(PoisonError::into_inner)
}

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
/// wrappers may leave descendants behind even after they exit. Fails once
/// [`stop_all`] has run.
pub fn run(command: &mut Command, deadline: Instant, max_bytes: u64) -> io::Result<Outcome> {
    let mut output = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(Stdio::null())
        .process_group(0);
    // Spawn under the lock, so that stop_all cannot miss the new group.
    let (mut child, group) = {
        let mut running = running();
        if running.stopped {
            return Err(io::Error::other("the launcher is exiting"));
        }
        let child = command.spawn()?;
        let group = Pid::from_child(&child);
        running.groups.push(group);
        (child, group)
    };
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
    running().groups.retain(|&known| known != group);
    let _ = kill_process_group(group, Signal::KILL);
    let _ = child.wait();
    outcome
}

/// Kill every helper that is still running, and refuse to start more. Call
/// this before the launcher exits or becomes Codex: either way the threads
/// waiting on helpers stop without killing them.
pub fn stop_all() {
    let mut running = running();
    running.stopped = true;
    for &group in &running.groups {
        let _ = kill_process_group(group, Signal::KILL);
    }
}
