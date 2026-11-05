//! The control socket that `codex-launcher ctl` uses to read and drive a menu.
//!
//! Each menu listens on `<runtime dir>/<pid>.sock`. A client sends one JSON
//! request line and reads one JSON response line, then the server closes the
//! connection. The menu applies a request, redraws, and only then replies, so
//! a reply always matches the screen.

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const DIR_NAME: &str = "codex-launcher";
const SOCKET_SUFFIX: &str = ".sock";
/// How long the server waits for a client to send a request or take a reply.
const SERVER_IO_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a client waits for the menu to answer.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_BYTES: u64 = 64 << 10;
const MAX_RESPONSE_BYTES: u64 = 16 << 20;

/// A control request, sent as one JSON line such as `{"cmd":"key","keys":["down"]}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    State,
    Screen,
    Key {
        keys: Vec<String>,
    },
    Select {
        profile: String,
    },
    Effort {
        level: String,
    },
    #[serde(rename = "save-effort")]
    SaveEffort,
    Launch,
    Quit,
}

pub fn state_response<T: Serialize>(state: &T) -> Value {
    json!({ "ok": true, "state": state })
}

pub fn screen_response(screen: String) -> Value {
    json!({ "ok": true, "screen": screen })
}

pub fn error_response(message: impl Into<String>) -> Value {
    json!({ "ok": false, "error": message.into() })
}

/// The directory that holds the control sockets of this user's launchers.
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir.join(DIR_NAME),
        _ => {
            std::env::temp_dir().join(format!("{DIR_NAME}-{}", rustix::process::getuid().as_raw()))
        }
    }
}

fn socket_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}{SOCKET_SUFFIX}"))
}

fn create_private_dir(dir: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error).with_context(|| format!("cannot create {}", dir.display()));
        }
    }
    check_private_dir(dir)
}

/// Refuse a socket directory that another user could have planted or can enter.
fn check_private_dir(dir: &Path) -> Result<()> {
    let meta =
        fs::symlink_metadata(dir).with_context(|| format!("cannot inspect {}", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    if meta.uid() != rustix::process::getuid().as_raw() {
        bail!("{} belongs to another user", dir.display());
    }
    if meta.mode() & 0o077 != 0 {
        bail!(
            "{} is open to other users; run: chmod 700 {}",
            dir.display(),
            dir.display()
        );
    }
    Ok(())
}

/// A request waiting for the menu loop, with the connection to answer on.
pub struct Pending {
    pub request: Request,
    stream: UnixStream,
}

impl Pending {
    /// Answer the request. Write errors are ignored because the client may be gone.
    pub fn reply(self, response: &Value) {
        write_line(&self.stream, response);
    }
}

fn write_line(mut stream: &UnixStream, response: &Value) {
    let mut bytes = serde_json::to_vec(response).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    bytes.push(b'\n');
    let _ = stream.set_write_timeout(Some(SERVER_IO_TIMEOUT));
    let _ = stream.write_all(&bytes);
}

/// The listening side of the control socket. Dropping it removes the socket.
pub struct Server {
    path: PathBuf,
    requests: Receiver<Pending>,
}

impl Server {
    /// Listen on `<dir>/<pid>.sock` and accept requests on a background thread.
    pub fn start(dir: &Path) -> Result<Self> {
        create_private_dir(dir)?;
        let path = socket_path(dir, std::process::id());
        // Only a process that had this pid before us can have left this file.
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot remove stale socket {}", path.display()));
            }
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("cannot listen on {}", path.display()))?;
        let (sender, requests) = mpsc::channel();
        let server = Self { path, requests };
        fs::set_permissions(&server.path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot restrict {}", server.path.display()))?;
        thread::Builder::new()
            .name("control".to_owned())
            .spawn(move || accept_loop(&listener, &sender))
            .context("cannot start the control thread")?;
        Ok(server)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The next waiting request, without blocking.
    pub fn try_next(&self) -> Option<Pending> {
        self.requests.try_recv().ok()
    }

    /// Wait for the next request.
    pub fn next(&self) -> Option<Pending> {
        self.requests.recv().ok()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn accept_loop(listener: &UnixListener, sender: &Sender<Pending>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            // Back off instead of spinning, for example when out of descriptors.
            thread::sleep(Duration::from_millis(100));
            continue;
        };
        match read_request(&stream) {
            Ok(request) => {
                if let Err(mpsc::SendError(pending)) = sender.send(Pending { request, stream }) {
                    pending.reply(&error_response("the launcher is exiting"));
                    return;
                }
            }
            Err(message) => write_line(&stream, &error_response(message)),
        }
    }
}

fn read_request(stream: &UnixStream) -> Result<Request, String> {
    stream
        .set_read_timeout(Some(SERVER_IO_TIMEOUT))
        .map_err(|error| format!("cannot set a read timeout: {error}"))?;
    let mut line = String::new();
    BufReader::new(stream.take(MAX_REQUEST_BYTES))
        .read_line(&mut line)
        .map_err(|error| format!("cannot read the request: {error}"))?;
    serde_json::from_str(line.trim_end()).map_err(|error| format!("invalid request: {error}"))
}

/// Send one request to the launcher listening on `path` and return its reply.
pub fn send(path: &Path, request: &Request) -> Result<Value> {
    let stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
            // Nobody is listening, so the socket outlived its launcher.
            let _ = fs::remove_file(path);
            bail!(
                "the launcher at {} has exited; removed its stale socket",
                path.display()
            );
        }
        Err(error) => {
            return Err(error).with_context(|| format!("cannot connect to {}", path.display()));
        }
    };
    stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    (&stream)
        .write_all(&bytes)
        .context("cannot send the request")?;
    let mut response = Vec::new();
    match (&stream)
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut response)
    {
        Ok(_) => {}
        Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            bail!(
                "no reply within {} seconds; the launcher may be suspended",
                CLIENT_TIMEOUT.as_secs()
            );
        }
        Err(error) => return Err(error).context("cannot read the reply"),
    }
    if response.is_empty() {
        bail!("the launcher closed the connection without replying; it may be exiting");
    }
    serde_json::from_slice(&response).context("the reply is not valid JSON")
}

/// A running launcher's control socket.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub pid: u32,
    pub path: PathBuf,
}

/// Find running launchers, removing sockets whose process has exited.
pub fn endpoints(dir: &Path) -> Result<Vec<Endpoint>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", dir.display())),
    };
    check_private_dir(dir)?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot read {}", dir.display()))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(SOCKET_SUFFIX))
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        let path = entry.path();
        if !is_alive(pid) {
            let _ = fs::remove_file(&path);
            continue;
        }
        found.push(Endpoint { pid, path });
    }
    found.sort_by_key(|endpoint| endpoint.pid);
    Ok(found)
}

/// Choose the launcher to control: the one with `pid`, or the only one running.
pub fn pick(dir: &Path, pid: Option<u32>) -> Result<Endpoint> {
    let endpoints = endpoints(dir)?;
    if let Some(pid) = pid {
        return endpoints
            .into_iter()
            .find(|endpoint| endpoint.pid == pid)
            .with_context(|| format!("no running codex-launcher has pid {pid}"));
    }
    match endpoints.as_slice() {
        [] => bail!("no running codex-launcher found"),
        [only] => Ok(only.clone()),
        many => {
            let pids: Vec<_> = many
                .iter()
                .map(|endpoint| endpoint.pid.to_string())
                .collect();
            bail!(
                "{} launchers are running (pids {}); choose one with --pid",
                many.len(),
                pids.join(", ")
            )
        }
    }
}

fn is_alive(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    !matches!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_dir() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("control");
        (temp, dir)
    }

    #[test]
    fn request_wire_format() {
        let request = Request::Key {
            keys: vec!["down".into()],
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"cmd":"key","keys":["down"]}"#
        );
        let parsed: Request = serde_json::from_str(r#"{"cmd":"select","profile":"x"}"#).unwrap();
        assert_eq!(
            parsed,
            Request::Select {
                profile: "x".into()
            }
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"state"}"#).unwrap(),
            Request::State
        );
    }

    #[test]
    fn round_trip_and_cleanup() {
        let (_temp, dir) = private_dir();
        let server = Server::start(&dir).unwrap();
        let mode = fs::metadata(&dir).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);

        let path = server.path().to_path_buf();
        let client = thread::spawn(move || send(&path, &Request::State).unwrap());
        let pending = server.next().unwrap();
        assert_eq!(pending.request, Request::State);
        pending.reply(&state_response(&json!({ "selected": "default" })));
        let response = client.join().unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(response["state"]["selected"], "default");

        let endpoints = endpoints(&dir).unwrap();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].pid, std::process::id());

        let socket = server.path().to_path_buf();
        drop(server);
        assert!(!socket.exists());
    }

    #[test]
    fn malformed_request_gets_an_error_reply() {
        let (_temp, dir) = private_dir();
        let server = Server::start(&dir).unwrap();
        let mut stream = UnixStream::connect(server.path()).unwrap();
        stream.write_all(b"{\"cmd\":\"dance\"}\n").unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["ok"], false);
        assert!(
            reply["error"].as_str().unwrap().contains("invalid request"),
            "{reply}"
        );
    }

    #[test]
    fn stale_sockets_are_removed() {
        let (_temp, dir) = private_dir();
        create_private_dir(&dir).unwrap();
        // No process can have this pid: it is above the kernel's pid limit.
        let stale = socket_path(&dir, 999_999_999);
        fs::write(&stale, "").unwrap();
        fs::write(dir.join("notes.txt"), "").unwrap();
        assert!(endpoints(&dir).unwrap().is_empty());
        assert!(!stale.exists());
        assert!(dir.join("notes.txt").exists());
    }

    #[test]
    fn refused_connection_removes_the_socket() {
        let (_temp, dir) = private_dir();
        create_private_dir(&dir).unwrap();
        let path = socket_path(&dir, std::process::id());
        drop(UnixListener::bind(&path).unwrap());
        let error = send(&path, &Request::State).unwrap_err().to_string();
        assert!(error.contains("has exited"), "{error}");
        assert!(!path.exists());
    }

    #[test]
    fn pick_needs_a_pid_when_several_run() {
        let (_temp, dir) = private_dir();
        assert!(
            pick(&dir, None)
                .unwrap_err()
                .to_string()
                .contains("no running")
        );
        let server = Server::start(&dir).unwrap();
        assert_eq!(pick(&dir, None).unwrap().pid, std::process::id());
        // A second live process: this test's parent, which outlives the test.
        let parent = rustix::process::getppid().map_or(1, |pid| pid.as_raw_nonzero().get());
        let parent = u32::try_from(parent).unwrap();
        fs::write(socket_path(&dir, parent), "").unwrap();
        let error = pick(&dir, None).unwrap_err().to_string();
        assert!(error.contains("--pid"), "{error}");
        assert_eq!(pick(&dir, Some(parent)).unwrap().pid, parent);
        assert!(pick(&dir, Some(1_000_000_000)).is_err());
        drop(server);
    }

    #[test]
    fn rejects_a_directory_that_others_can_enter() {
        let (_temp, dir) = private_dir();
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let error = Server::start(&dir).err().unwrap().to_string();
        assert!(error.contains("chmod 700"), "{error}");
        assert!(endpoints(&dir).is_err());
    }
}
