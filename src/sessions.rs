//! Codex sessions: what their logs record, and the profiles that can resume
//! them.
//!
//! Codex writes each session's log under `$CODEX_HOME/sessions/YYYY/MM/DD/`
//! and names sessions in `$CODEX_HOME/session_index.jsonl`. Both are Codex
//! internals with no stability promise, so the launcher reads only the records
//! it needs, never transcript content, and lists a log it cannot read as
//! unknown instead of failing.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::profile::{BUILTIN_PROVIDERS, Catalog, Profile};

const SESSIONS_DIR: &str = "sessions";
const INDEX_FILE: &str = "session_index.jsonl";
const LOG_PREFIX: &str = "rollout-";
const LOG_SUFFIX: &str = ".jsonl";
const COMPRESSED_SUFFIX: &str = ".jsonl.zst";
/// How many directory levels below `sessions` hold logs. Codex uses three:
/// year, month, and day.
const MAX_DEPTH: usize = 4;
/// How much of a log, once decompressed, is read to find its first turn.
/// Logs can run to hundreds of megabytes, and the first turn comes early.
const MAX_HEAD_BYTES: u64 = 8 << 20;
/// How many records after `session_meta` are read to find the first turn.
const MAX_HEAD_RECORDS: usize = 200;
const MAX_INDEX_BYTES: u64 = 16 << 20;
/// The sources of sessions that ran in Codex's terminal interface or an
/// editor, which `codex resume` lists without `--include-non-interactive`.
const INTERACTIVE_SOURCES: &[&str] = &["cli", "vscode"];

/// One Codex session, merged from its logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// The session's ID, in Codex's lowercase hyphenated form.
    pub id: String,
    /// The latest name that the session index gives it.
    pub name: Option<String>,
    pub cwd: PathBuf,
    pub branch: Option<String>,
    pub provider: String,
    /// The model of the session's first turn, which reflects the profile that
    /// started it. A later model switch does not change the profile.
    pub model: Option<String>,
    /// Whether it ran in Codex's terminal interface or an editor, rather than
    /// as a `codex exec` run or a subagent thread.
    pub interactive: bool,
    /// When its newest log last changed.
    pub updated: SystemTime,
    /// Its logs, oldest first.
    pub logs: Vec<PathBuf>,
    /// Whether it ran in the launcher's current directory.
    pub here: bool,
}

impl Session {
    /// What the list shows as its name: its name, its git branch, or its ID.
    pub fn title(&self) -> &str {
        self.name
            .as_deref()
            .or(self.branch.as_deref())
            .unwrap_or(&self.id)
    }
}

/// A log that the launcher could not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unknown {
    pub path: PathBuf,
    /// The directory that the log records, when that much could be read.
    pub cwd: Option<PathBuf>,
    pub updated: SystemTime,
    pub error: String,
    /// Whether the log records the launcher's current directory.
    pub here: bool,
}

/// One row of the sessions list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Session(Session),
    Unknown(Unknown),
}

impl Entry {
    pub fn session(&self) -> Option<&Session> {
        match self {
            Self::Session(session) => Some(session),
            Self::Unknown(_) => None,
        }
    }

    pub fn updated(&self) -> SystemTime {
        match self {
            Self::Session(session) => session.updated,
            Self::Unknown(unknown) => unknown.updated,
        }
    }

    /// Whether the list shows this entry: an interactive session, or a log
    /// that could not be read, which might be one. `all` shows every
    /// directory rather than only the current one.
    pub fn is_listed(&self, all: bool) -> bool {
        match self {
            Self::Session(session) => session.interactive && (all || session.here),
            Self::Unknown(unknown) => all || unknown.here,
        }
    }
}

/// The sessions of one Codex home.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sessions {
    /// Sessions and unreadable logs, newest first.
    pub entries: Vec<Entry>,
    /// Problems that did not stop loading, such as unreadable logs.
    pub warnings: Vec<String>,
}

impl Sessions {
    /// The session that `codex resume <arg>` resumes, from any directory, or
    /// why there is no single one.
    ///
    /// An argument that parses as a UUID is an ID, as in Codex. Anything else
    /// must be the latest name of exactly one session.
    pub fn find(&self, arg: &str) -> Result<&Session, String> {
        let mut sessions = self.entries.iter().filter_map(Entry::session);
        if let Some(id) = parse_uuid(arg) {
            return sessions
                .find(|session| session.id == id)
                .ok_or_else(|| format!("No session has ID {id}."));
        }
        let named: Vec<&Session> = sessions
            .filter(|session| session.name.as_deref() == Some(arg))
            .collect();
        match named.as_slice() {
            [session] => Ok(session),
            [] => Err(format!("No session is named {:?}.", clean(arg))),
            several => Err(format!(
                "{} sessions are named {:?}.",
                several.len(),
                clean(arg)
            )),
        }
    }
}

/// Sessions read on a background thread, since reading them all takes a moment.
pub struct Loader {
    receiver: Receiver<Sessions>,
}

impl Loader {
    /// Start reading the sessions of `codex_home`. `current_dir` decides which
    /// ran here.
    pub fn start(codex_home: PathBuf, current_dir: Option<PathBuf>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("sessions".to_owned())
            .spawn(move || {
                // Sending fails only once the menu has closed.
                let _ = sender.send(load(&codex_home, current_dir.as_deref()));
            });
        if let Err(error) = spawned {
            let (sender, failed) = mpsc::channel();
            let _ = sender.send(Sessions {
                entries: Vec::new(),
                warnings: vec![format!("cannot start reading sessions: {error}")],
            });
            return Self { receiver: failed };
        }
        Self { receiver }
    }

    /// The sessions, once they have been read.
    pub fn poll(&self) -> Option<Sessions> {
        match self.receiver.try_recv() {
            Ok(sessions) => Some(sessions),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Sessions {
                entries: Vec::new(),
                warnings: vec!["reading sessions failed".to_owned()],
            }),
        }
    }
}

/// Read every session under `codex_home`. `current_dir` decides which
/// sessions ran here.
pub fn load(codex_home: &Path, current_dir: Option<&Path>) -> Sessions {
    let mut warnings = Vec::new();
    let names = read_index(&codex_home.join(INDEX_FILE), &mut warnings);
    let mut paths = Vec::new();
    find_logs(&codex_home.join(SESSIONS_DIR), 0, &mut paths, &mut warnings);
    paths.sort();
    let here = current_dir.map(Here::new);
    let ran_here = |cwd: &Path| here.as_ref().is_some_and(|here| here.contains(cwd));

    let mut by_id: HashMap<String, Vec<Log>> = HashMap::new();
    let mut entries = Vec::new();
    for path in paths {
        match read_log(&path) {
            Ok(log) => by_id.entry(log.meta.id.clone()).or_default().push(log),
            Err(failure) => entries.push(Entry::Unknown(Unknown {
                here: failure.cwd.as_deref().is_some_and(ran_here),
                path,
                cwd: failure.cwd,
                updated: failure.updated,
                error: failure.error,
            })),
        }
    }
    let unreadable = entries.len();
    for (id, mut logs) in by_id {
        // The log that started the session comes first. A log without a start
        // time sorts last, and file names break ties.
        logs.sort_by(|a, b| {
            (a.meta.started.is_none(), &a.meta.started, &a.path).cmp(&(
                b.meta.started.is_none(),
                &b.meta.started,
                &b.path,
            ))
        });
        let model = logs.iter().find_map(|log| log.model.clone());
        let updated = logs
            .iter()
            .map(|log| log.updated)
            .max()
            .unwrap_or(UNIX_EPOCH);
        let meta = &logs[0].meta;
        entries.push(Entry::Session(Session {
            name: names.get(&id).cloned(),
            here: ran_here(&meta.cwd),
            cwd: meta.cwd.clone(),
            branch: meta.branch.clone(),
            provider: meta.provider.clone(),
            model,
            interactive: meta.interactive,
            updated,
            logs: logs.into_iter().map(|log| log.path).collect(),
            id,
        }));
    }
    // Newest first; the ID or path keeps the order of equal times stable.
    entries.sort_by(|a, b| {
        b.updated()
            .cmp(&a.updated())
            .then_with(|| sort_key(a).cmp(sort_key(b)))
    });
    if unreadable > 0 {
        warnings.push(if unreadable == 1 {
            "1 session log could not be read".to_owned()
        } else {
            format!("{unreadable} session logs could not be read")
        });
    }
    Sessions { entries, warnings }
}

fn sort_key(entry: &Entry) -> &Path {
    match entry {
        Entry::Session(session) => Path::new(&session.id),
        Entry::Unknown(unknown) => &unknown.path,
    }
}

/// The current directory, compared as given and with symbolic links resolved.
struct Here {
    dir: PathBuf,
    canonical: Option<PathBuf>,
}

impl Here {
    fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            canonical: dir.canonicalize().ok(),
        }
    }

    fn contains(&self, cwd: &Path) -> bool {
        cwd == self.dir
            || self
                .canonical
                .as_ref()
                .is_some_and(|canonical| cwd.canonicalize().is_ok_and(|cwd| cwd == *canonical))
    }
}

/// The latest name of each session in the index. Later lines replace earlier
/// ones, and a line that does not parse is skipped.
fn read_index(path: &Path, warnings: &mut Vec<String>) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return names,
        Err(error) => {
            warnings.push(format!("cannot read {INDEX_FILE}: {error}"));
            return names;
        }
    };
    #[derive(Deserialize)]
    struct IndexEntry {
        id: String,
        thread_name: Option<String>,
    }
    for line in BufReader::new(file.take(MAX_INDEX_BYTES)).split(b'\n') {
        let Ok(line) = line else {
            break;
        };
        let Ok(entry) = serde_json::from_slice::<IndexEntry>(&line) else {
            continue;
        };
        let Some(id) = parse_uuid(&entry.id) else {
            continue;
        };
        match entry.thread_name.filter(|name| !name.trim().is_empty()) {
            Some(name) => names.insert(id, name),
            None => names.remove(&id),
        };
    }
    names
}

/// Collect the log files under `dir`, `depth` levels below the sessions
/// directory.
fn find_logs(dir: &Path, depth: usize, paths: &mut Vec<PathBuf>, warnings: &mut Vec<String>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if depth == 0 && error.kind() == ErrorKind::NotFound => return,
        Err(error) => {
            warnings.push(format!("cannot read {}: {error}", dir.display()));
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            if depth < MAX_DEPTH {
                find_logs(&path, depth + 1, paths, warnings);
            }
        } else if metadata.is_file() && is_log_name(&entry.file_name().to_string_lossy()) {
            paths.push(path);
        }
    }
}

fn is_log_name(name: &str) -> bool {
    name.starts_with(LOG_PREFIX)
        && (name.ends_with(LOG_SUFFIX) || name.ends_with(COMPRESSED_SUFFIX))
}

/// What one log records about its session.
struct Log {
    path: PathBuf,
    updated: SystemTime,
    meta: Meta,
    /// The model of the log's first turn.
    model: Option<String>,
}

/// The fields of a `session_meta` record that the launcher uses.
struct Meta {
    id: String,
    cwd: PathBuf,
    provider: String,
    interactive: bool,
    branch: Option<String>,
    /// When the log started, as Codex records it.
    started: Option<String>,
}

/// Why a log could not be read, with what could be read of it.
struct Failure {
    cwd: Option<PathBuf>,
    updated: SystemTime,
    error: String,
}

/// Read a log's `session_meta` record and the model of its first turn.
fn read_log(path: &Path) -> Result<Log, Failure> {
    let failure = |updated, error: String| Failure {
        cwd: None,
        updated,
        error,
    };
    let file =
        File::open(path).map_err(|error| failure(UNIX_EPOCH, format!("cannot open: {error}")))?;
    let updated = file
        .metadata()
        .and_then(|metadata| metadata.modified())
        .unwrap_or(UNIX_EPOCH);
    let compressed = path.to_string_lossy().ends_with(COMPRESSED_SUFFIX);
    // Decompression errors surface as read errors, since decoding is lazy.
    let read_error = |error: std::io::Error| {
        let action = if compressed { "decompress" } else { "read" };
        failure(updated, format!("cannot {action}: {error}"))
    };
    let source: Box<dyn Read> = if compressed {
        Box::new(zstd::stream::read::Decoder::new(file).map_err(read_error)?)
    } else {
        Box::new(file)
    };
    let mut reader = BufReader::new(source.take(MAX_HEAD_BYTES));
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).map_err(read_error)?;
    let meta = parse_meta(&line).map_err(|(cwd, error)| Failure {
        cwd,
        updated,
        error,
    })?;
    Ok(Log {
        path: path.to_path_buf(),
        updated,
        meta,
        model: first_model(&mut reader, &mut line),
    })
}

/// The fields of a log's first record, which must be its `session_meta`.
/// A failure keeps the directory, when the record gives one.
fn parse_meta(line: &[u8]) -> Result<Meta, (Option<PathBuf>, String)> {
    #[derive(Deserialize)]
    struct Record {
        #[serde(rename = "type")]
        kind: String,
        payload: Map<String, Value>,
    }
    if line.iter().all(u8::is_ascii_whitespace) {
        return Err((None, "the log is empty".to_owned()));
    }
    let record: Record = serde_json::from_slice(line)
        .map_err(|error| (None, format!("the first record does not parse: {error}")))?;
    if record.kind != "session_meta" {
        return Err((None, "the first record is not session_meta".to_owned()));
    }
    let payload = record.payload;
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let cwd = text("cwd").map(PathBuf::from);
    let missing = |key: &str| (cwd.clone(), format!("session_meta records no {key}"));
    let id = text("id").ok_or_else(|| missing("id"))?;
    let id = parse_uuid(id).ok_or_else(|| {
        (
            cwd.clone(),
            "session_meta records an ID that is not a UUID".to_owned(),
        )
    })?;
    let provider = text("model_provider").ok_or_else(|| missing("model_provider"))?;
    let interactive = match payload.get("source") {
        Some(Value::String(source)) => INTERACTIVE_SOURCES.contains(&source.as_str()),
        // Subagent threads record an object.
        Some(Value::Object(_)) => false,
        _ => return Err(missing("source")),
    };
    let cwd = cwd.clone().ok_or_else(|| missing("cwd"))?;
    Ok(Meta {
        id,
        cwd,
        provider: provider.to_owned(),
        interactive,
        branch: payload
            .get("git")
            .and_then(|git| git.get("branch"))
            .and_then(Value::as_str)
            .filter(|branch| !branch.is_empty())
            .map(str::to_owned),
        started: text("timestamp").map(str::to_owned),
    })
}

/// The model of the first `turn_context` record among the next records.
fn first_model(reader: &mut impl BufRead, line: &mut Vec<u8>) -> Option<String> {
    #[derive(Deserialize)]
    struct Kind {
        #[serde(rename = "type")]
        kind: String,
    }
    #[derive(Deserialize)]
    struct Turn {
        payload: TurnPayload,
    }
    #[derive(Deserialize)]
    struct TurnPayload {
        model: Option<String>,
    }
    for _ in 0..MAX_HEAD_RECORDS {
        line.clear();
        match reader.read_until(b'\n', line) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        // Only the record type is parsed, so other records' contents are
        // skipped rather than kept.
        if serde_json::from_slice::<Kind>(line).is_ok_and(|record| record.kind == "turn_context") {
            return serde_json::from_slice::<Turn>(line)
                .ok()?
                .payload
                .model
                .filter(|model| !model.is_empty());
        }
    }
    None
}

/// The canonical form of a UUID given in any form that Codex parses:
/// hyphenated, simple, braced, or as a URN.
pub fn parse_uuid(text: &str) -> Option<String> {
    let hyphenated = text.strip_prefix("urn:uuid:").or_else(|| {
        text.strip_prefix('{')
            .and_then(|rest| rest.strip_suffix('}'))
    });
    let hex: String = match hyphenated.unwrap_or(text) {
        inner if inner.len() == 36 => {
            let bytes = inner.as_bytes();
            if [8, 13, 18, 23].iter().any(|&index| bytes[index] != b'-') {
                return None;
            }
            inner.chars().filter(|&c| c != '-').collect()
        }
        inner if inner.len() == 32 && hyphenated.is_none() => inner.to_owned(),
        _ => return None,
    };
    if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let hex = hex.to_ascii_lowercase();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// The profiles that can resume a session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Match {
    /// Indexes into the catalog's profiles. Several mean that the user picks
    /// one; none mean that the session is not resumable.
    pub profiles: Vec<usize>,
    /// Whether the session's model differs from that of every matched profile.
    pub model_differs: bool,
    /// Whether the match is the default entry, because no profile uses the
    /// session's provider but plain `codex resume` knows it.
    pub fallback: bool,
}

/// The profiles that can resume `session`.
///
/// A profile matches when its effective provider is the one the session
/// recorded. When several do, those with the session's model narrow the
/// match. When none do, the default entry matches if config.toml defines the
/// provider or Codex has it built in, since plain `codex resume` can resume
/// the session then. Harness entries never match, and neither does a profile
/// whose file has an error or whose provider is defined nowhere, since Codex
/// cannot run it.
pub fn matching(session: &Session, catalog: &Catalog) -> Match {
    let usable = |profile: &Profile| {
        profile.harness.is_none() && profile.error.is_none() && !profile.provider.missing
    };
    let by_provider: Vec<usize> = catalog
        .profiles
        .iter()
        .enumerate()
        .filter(|(_, profile)| usable(profile) && profile.provider.id == session.provider)
        .map(|(index, _)| index)
        .collect();
    let differs = |index: &usize| {
        session
            .model
            .as_deref()
            .is_some_and(|model| catalog.profiles[*index].model_name() != Some(model))
    };
    if by_provider.is_empty() {
        let known = catalog.base_providers.contains(&session.provider)
            || BUILTIN_PROVIDERS.contains(&session.provider.as_str());
        return match catalog
            .profiles
            .iter()
            .position(|profile| profile.is_default() && usable(profile))
        {
            Some(index) if known => Match {
                model_differs: differs(&index),
                profiles: vec![index],
                fallback: true,
            },
            _ => Match::default(),
        };
    }
    let by_model: Vec<usize> = by_provider
        .iter()
        .copied()
        .filter(|index| !differs(index))
        .collect();
    if by_model.is_empty() || session.model.is_none() {
        Match {
            model_differs: session.model.is_some(),
            profiles: by_provider,
            fallback: false,
        }
    } else {
        Match {
            profiles: by_model,
            model_differs: false,
            fallback: false,
        }
    }
}

/// `text` as one line, with control characters as spaces so that it cannot
/// move the cursor.
pub fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// How long before `now` that `time` was, briefly, such as `5m ago`.
pub fn ago(time: SystemTime, now: SystemTime) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const MONTH: u64 = 30 * DAY;
    const YEAR: u64 = 365 * DAY;
    // A time ahead of the clock counts as now.
    let seconds = now
        .duration_since(time)
        .map_or(0, |elapsed| elapsed.as_secs());
    match seconds {
        _ if seconds < MINUTE => "just now".to_owned(),
        _ if seconds < HOUR => format!("{}m ago", seconds / MINUTE),
        _ if seconds < DAY => format!("{}h ago", seconds / HOUR),
        _ if seconds < WEEK => format!("{}d ago", seconds / DAY),
        _ if seconds < 4 * WEEK => format!("{}w ago", seconds / WEEK),
        _ if seconds < YEAR => format!("{}mo ago", (seconds / MONTH).max(1)),
        _ => format!("{}y ago", seconds / YEAR),
    }
}

/// `time` in UTC to the second, as RFC 3339, such as `2026-11-21T14:03:12Z`.
pub fn utc(time: SystemTime) -> String {
    let (date, seconds) = utc_parts(time);
    format!(
        "{date}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// `time` in UTC to the minute, for people, such as `2026-11-21 14:03 UTC`.
pub fn utc_minute(time: SystemTime) -> String {
    let (date, seconds) = utc_parts(time);
    format!("{date} {:02}:{:02} UTC", seconds / 3600, seconds / 60 % 60)
}

/// The UTC date of `time` as `YYYY-MM-DD`, and the seconds into that day.
fn utc_parts(time: SystemTime) -> (String, u64) {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let (year, month, day) = civil_date(days);
    (format!("{year:04}-{month:02}-{day:02}"), seconds % 86_400)
}

/// The proleptic Gregorian date `days` after 1970-01-01, from Howard Hinnant's
/// `civil_from_days`.
fn civil_date(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * shifted_month + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    })
    .unwrap_or(1);
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;
    use std::time::Duration;

    use serde_json::json;

    const HOUR: Duration = Duration::from_secs(3600);

    fn id(n: u8) -> String {
        format!("019a0000-0000-7000-8000-0000000000{n:02}")
    }

    fn meta(n: u8, cwd: &str, provider: &str, source: Value) -> Value {
        json!({
            "timestamp": "2026-11-20T10:00:00.000Z",
            "type": "session_meta",
            "payload": {
                "id": id(n),
                "session_id": id(n),
                "timestamp": "2026-11-20T10:00:00.000Z",
                "cwd": cwd,
                "originator": "codex_cli_rs",
                "cli_version": "0.171.0",
                "source": source,
                "model_provider": provider,
                "base_instructions": {"text": "You are Codex."},
                "git": {"branch": "main", "commit_hash": "abc", "repository_url": null},
            },
        })
    }

    fn turn(model: &str) -> Value {
        json!({"timestamp": "2026-11-20T10:00:01.000Z", "type": "turn_context", "payload": {"cwd": "/w", "model": model}})
    }

    fn message() -> Value {
        json!({"timestamp": "2026-11-20T10:00:00.500Z", "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "mentions turn_context"}]}})
    }

    /// Write a log of `records` under the sessions directory, `age` old.
    fn write(home: &Path, name: &str, records: &[Value], age: Duration) -> PathBuf {
        let text: String = records.iter().map(|record| format!("{record}\n")).collect();
        write_bytes(home, name, text.as_bytes(), age)
    }

    fn write_bytes(home: &Path, name: &str, bytes: &[u8], age: Duration) -> PathBuf {
        let path = home.join(SESSIONS_DIR).join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = if name.ends_with(".zst") {
            zstd::encode_all(bytes, 0).unwrap()
        } else {
            bytes.to_vec()
        };
        fs::write(&path, bytes).unwrap();
        let file = File::options().write(true).open(&path).unwrap();
        file.set_times(FileTimes::new().set_modified(SystemTime::now() - age))
            .unwrap();
        path
    }

    fn sessions(entries: &[Entry]) -> Vec<&Session> {
        entries.iter().filter_map(Entry::session).collect()
    }

    #[test]
    fn logs_merge_by_recorded_id_and_keep_the_first_turns_model() {
        let home = tempfile::tempdir().unwrap();
        let first = meta(1, "/work", "gateway", json!("cli"));
        let mut later = first.clone();
        later["payload"]["timestamp"] = json!("2026-11-21T08:00:00.000Z");
        // The continuation sits under a later date, compressed, and records
        // another model after a switch. Its file name leads with another ID.
        write(
            home.path(),
            "2026/11/20/rollout-2026-11-20T10-00-00-a.jsonl",
            &[first, message(), turn("gpt-team")],
            3 * HOUR,
        );
        write(
            home.path(),
            "2026/11/21/rollout-2026-11-21T08-00-00-b_c.jsonl.zst",
            &[later, turn("gpt-other")],
            HOUR,
        );
        let loaded = load(home.path(), Some(Path::new("/work")));
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let sessions = sessions(&loaded.entries);
        assert_eq!(sessions.len(), 1);
        let session = sessions[0];
        assert_eq!(session.id, id(1));
        assert_eq!(session.model.as_deref(), Some("gpt-team"));
        assert_eq!(session.provider, "gateway");
        assert!(session.interactive && session.here);
        assert_eq!(session.logs.len(), 2);
        assert!(session.logs[0].ends_with("rollout-2026-11-20T10-00-00-a.jsonl"));
        // The newest log decides when the session was last updated.
        let age = SystemTime::now().duration_since(session.updated).unwrap();
        assert!(age < HOUR + Duration::from_secs(60), "{age:?}");
        assert_eq!(session.title(), "main");
    }

    #[test]
    fn names_come_from_the_latest_index_entry_then_the_branch_then_the_id() {
        let home = tempfile::tempdir().unwrap();
        let mut unbranched = meta(3, "/work", "openai", json!("vscode"));
        unbranched["payload"]["git"] = Value::Null;
        for (n, record) in [
            (1u8, meta(1, "/work", "openai", json!("cli"))),
            (2, meta(2, "/work", "openai", json!("cli"))),
            (3, unbranched),
        ] {
            write(
                home.path(),
                &format!("2026/11/20/rollout-{n}.jsonl"),
                &[record],
                HOUR * u32::from(n),
            );
        }
        let index = [
            json!({"id": id(1), "thread_name": "first name", "updated_at": "2026-11-20T10:00:00Z"}),
            json!({"id": id(2), "thread_name": "dropped", "updated_at": "2026-11-20T10:00:00Z"}),
            json!({"id": id(1), "thread_name": "renamed", "updated_at": "2026-11-20T11:00:00Z"}),
            json!({"id": id(2), "thread_name": "", "updated_at": "2026-11-20T11:00:00Z"}),
        ];
        let index: String = index
            .iter()
            .map(|entry| format!("{entry}\nnot json\n"))
            .collect();
        fs::write(home.path().join(INDEX_FILE), index).unwrap();
        let loaded = load(home.path(), None);
        let titles: Vec<_> = sessions(&loaded.entries)
            .iter()
            .map(|session| session.title().to_owned())
            .collect();
        assert_eq!(titles, ["renamed", "main", &id(3)]);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    }

    #[test]
    fn only_interactive_sessions_are_listed_and_only_here_unless_all() {
        let home = tempfile::tempdir().unwrap();
        let subagent =
            json!({"subagent": {"thread_spawn": {"parent_thread_id": id(1), "depth": 1}}});
        for (n, cwd, source) in [
            (1, "/work", json!("cli")),
            (2, "/work", json!("vscode")),
            (3, "/elsewhere", json!("cli")),
            (4, "/work", json!("exec")),
            (5, "/work", subagent),
        ] {
            write(
                home.path(),
                &format!("2026/11/20/rollout-{n}.jsonl"),
                &[meta(n, cwd, "openai", source)],
                HOUR * u32::from(n),
            );
        }
        let loaded = load(home.path(), Some(Path::new("/work")));
        let listed = |all: bool| -> Vec<String> {
            loaded
                .entries
                .iter()
                .filter(|entry| entry.is_listed(all))
                .filter_map(|entry| Some(entry.session()?.id.clone()))
                .collect()
        };
        assert_eq!(listed(false), [id(1), id(2)]);
        assert_eq!(listed(true), [id(1), id(2), id(3)]);
        // Codex resumes any session by ID, so the rest stay findable.
        assert_eq!(loaded.find(&id(4)).unwrap().id, id(4));
        assert!(!loaded.find(&id(5)).unwrap().interactive);
    }

    #[test]
    fn the_current_directory_matches_through_symbolic_links() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let link = home.path().join("link");
        std::os::unix::fs::symlink(work.path(), &link).unwrap();
        write(
            home.path(),
            "2026/11/20/rollout-1.jsonl",
            &[meta(
                1,
                work.path().to_str().unwrap(),
                "openai",
                json!("cli"),
            )],
            HOUR,
        );
        let loaded = load(home.path(), Some(&link));
        assert!(sessions(&loaded.entries)[0].here);
    }

    #[test]
    fn unreadable_logs_are_unknown_entries_with_one_warning() {
        let home = tempfile::tempdir().unwrap();
        let mut no_provider = meta(2, "/work", "openai", json!("cli"));
        no_provider["payload"]
            .as_object_mut()
            .unwrap()
            .remove("model_provider");
        let mut bad_id = meta(3, "/work", "openai", json!("cli"));
        bad_id["payload"]["id"] = json!("../../etc");
        write(
            home.path(),
            "2026/11/20/rollout-1.jsonl",
            &[meta(1, "/work", "openai", json!("cli"))],
            HOUR,
        );
        write(
            home.path(),
            "2026/11/20/rollout-2.jsonl",
            &[no_provider],
            HOUR,
        );
        write(home.path(), "2026/11/20/rollout-3.jsonl", &[bad_id], HOUR);
        write_bytes(
            home.path(),
            "2026/11/20/rollout-4.jsonl",
            b"{not json\n",
            HOUR,
        );
        write_bytes(home.path(), "2026/11/20/rollout-5.jsonl", b"", HOUR);
        write(
            home.path(),
            "2026/11/20/rollout-6.jsonl",
            &[message()],
            HOUR,
        );
        write_bytes(home.path(), "2026/11/20/rollout-7.jsonl.zst", b"", HOUR);
        // Not plain zstd: the frame magic is wrong.
        let path = home
            .path()
            .join(SESSIONS_DIR)
            .join("2026/11/20/rollout-8.jsonl.zst");
        fs::write(&path, b"plain text\n").unwrap();
        // Other files are not logs.
        write_bytes(home.path(), "2026/11/20/notes.txt", b"x", HOUR);
        write_bytes(home.path(), "2026/11/20/rollout-9.json", b"x", HOUR);

        let loaded = load(home.path(), Some(Path::new("/work")));
        assert_eq!(loaded.warnings, ["7 session logs could not be read"]);
        let unknown: Vec<&Unknown> = loaded
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Unknown(unknown) => Some(unknown),
                Entry::Session(_) => None,
            })
            .collect();
        let errors: Vec<(String, &str, bool)> = unknown
            .iter()
            .map(|unknown| {
                (
                    unknown
                        .path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    unknown.error.as_str(),
                    unknown.here,
                )
            })
            .collect();
        let error = |name: &str| {
            errors
                .iter()
                .find(|(file, ..)| file == name)
                .map(|(_, error, here)| (*error, *here))
                .unwrap()
        };
        assert_eq!(
            error("rollout-2.jsonl"),
            ("session_meta records no model_provider", true)
        );
        assert_eq!(
            error("rollout-3.jsonl"),
            ("session_meta records an ID that is not a UUID", true)
        );
        assert!(
            error("rollout-4.jsonl")
                .0
                .starts_with("the first record does not parse")
        );
        assert_eq!(error("rollout-5.jsonl"), ("the log is empty", false));
        assert_eq!(
            error("rollout-6.jsonl"),
            ("the first record is not session_meta", false)
        );
        assert_eq!(error("rollout-7.jsonl.zst"), ("the log is empty", false));
        assert!(
            error("rollout-8.jsonl.zst")
                .0
                .starts_with("cannot decompress")
        );
        // A log whose directory is unknown is listed only under every directory.
        let listed_here = loaded
            .entries
            .iter()
            .filter(|entry| entry.is_listed(false))
            .count();
        assert_eq!(listed_here, 3);
        assert_eq!(sessions(&loaded.entries).len(), 1);
    }

    #[test]
    fn only_the_first_records_are_read() {
        let home = tempfile::tempdir().unwrap();
        let filler = json!({"type": "event_msg", "payload": {"type": "token_count"}});
        let mut late: Vec<Value> = vec![meta(1, "/work", "openai", json!("cli"))];
        late.extend(std::iter::repeat_n(filler, MAX_HEAD_RECORDS));
        late.push(turn("too-late"));
        write(home.path(), "2026/11/20/rollout-1.jsonl", &late, HOUR);
        // A record larger than the budget ends the search too, and a turn
        // without a model leaves the model unknown.
        let big = json!({"type": "response_item", "payload": {"text": "x".repeat(MAX_HEAD_BYTES as usize)}});
        write(
            home.path(),
            "2026/11/20/rollout-2.jsonl",
            &[
                meta(2, "/work", "openai", json!("cli")),
                big,
                turn("hidden"),
            ],
            HOUR,
        );
        let unmodeled = json!({"type": "turn_context", "payload": {"cwd": "/work"}});
        write(
            home.path(),
            "2026/11/20/rollout-3.jsonl",
            &[
                meta(3, "/work", "openai", json!("cli")),
                unmodeled,
                turn("second"),
            ],
            HOUR,
        );
        let loaded = load(home.path(), None);
        for session in sessions(&loaded.entries) {
            assert_eq!(session.model, None, "{}", session.id);
        }
    }

    #[test]
    fn missing_or_empty_homes_have_no_sessions() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(load(home.path(), None), Sessions::default());
        fs::create_dir(home.path().join(SESSIONS_DIR)).unwrap();
        assert_eq!(load(home.path(), None), Sessions::default());
    }

    #[test]
    fn find_takes_any_uuid_form_or_one_exact_name() {
        let home = tempfile::tempdir().unwrap();
        for n in 1..=3 {
            write(
                home.path(),
                &format!("2026/11/20/rollout-{n}.jsonl"),
                &[meta(n, "/work", "openai", json!("cli"))],
                HOUR,
            );
        }
        let index = [
            json!({"id": id(1), "thread_name": "docs"}),
            json!({"id": id(2), "thread_name": "tests"}),
            json!({"id": id(3), "thread_name": "tests"}),
        ];
        let index: String = index.iter().map(|entry| format!("{entry}\n")).collect();
        fs::write(home.path().join(INDEX_FILE), index).unwrap();
        let loaded = load(home.path(), None);
        let found = |arg: &str| loaded.find(arg).map(|session| session.id.clone());
        let upper = id(1).to_uppercase();
        let simple = id(1).replace('-', "");
        for form in [
            id(1),
            upper.clone(),
            simple.clone(),
            format!("{{{}}}", id(1)),
            format!("urn:uuid:{}", id(1)),
        ] {
            assert_eq!(found(&form), Ok(id(1)), "{form}");
        }
        assert_eq!(found("docs"), Ok(id(1)));
        assert_eq!(
            found("tests"),
            Err("2 sessions are named \"tests\".".to_owned())
        );
        assert_eq!(
            found("Docs"),
            Err("No session is named \"Docs\".".to_owned())
        );
        let absent = "019a0000-0000-7000-8000-0000000000ff";
        assert_eq!(found(absent), Err(format!("No session has ID {absent}.")));
        for not_uuid in [
            &format!("{{{simple}}}"),
            &format!("urn:uuid:{simple}"),
            &simple[1..],
            &id(1).replacen('-', "x", 1),
            "zz9a0000-0000-7000-8000-000000000001",
        ] {
            assert_eq!(parse_uuid(not_uuid), None, "{not_uuid}");
        }
    }

    /// A catalog of these profile files over a config.toml of `base`.
    fn catalog(base: &str, profiles: &[(&str, &str)]) -> (tempfile::TempDir, Catalog) {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("config.toml"), base).unwrap();
        for (name, text) in profiles {
            fs::write(home.path().join(format!("{name}.config.toml")), text).unwrap();
        }
        let catalog = crate::profile::load(home.path()).unwrap();
        (home, catalog)
    }

    fn session(provider: &str, model: Option<&str>) -> Session {
        Session {
            id: id(1),
            name: None,
            cwd: PathBuf::from("/work"),
            branch: None,
            provider: provider.to_owned(),
            model: model.map(str::to_owned),
            interactive: true,
            updated: UNIX_EPOCH,
            logs: Vec::new(),
            here: true,
        }
    }

    fn matched(
        catalog: &Catalog,
        provider: &str,
        model: Option<&str>,
    ) -> (Vec<String>, bool, bool) {
        let result = matching(&session(provider, model), catalog);
        let names = result
            .profiles
            .iter()
            .map(|&index| catalog.profiles[index].name.clone())
            .collect();
        (names, result.model_differs, result.fallback)
    }

    const GATEWAY: &str =
        "[model_providers.gateway]\nname = 'Gateway'\nbase_url = 'https://gateway.example/v1'\n";

    #[test]
    fn profiles_match_by_provider_and_then_by_model() {
        let gateway =
            |model: &str| format!("model = '{model}'\nmodel_provider = 'gateway'\n{GATEWAY}");
        let (_home, catalog) = catalog(
            "model = 'gpt-base'\n",
            &[
                ("fast", &gateway("gpt-fast")),
                ("team", &gateway("gpt-team")),
                ("twin", &gateway("gpt-team")),
            ],
        );
        let names = |list: &[&str]| {
            list.iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            matched(&catalog, "gateway", Some("gpt-fast")),
            (names(&["fast"]), false, false)
        );
        // Several profiles still match, so the user picks one.
        assert_eq!(
            matched(&catalog, "gateway", Some("gpt-team")),
            (names(&["team", "twin"]), false, false)
        );
        // No profile has the model, so the provider alone decides.
        assert_eq!(
            matched(&catalog, "gateway", Some("gpt-old")),
            (names(&["fast", "team", "twin"]), true, false)
        );
        assert_eq!(
            matched(&catalog, "gateway", None),
            (names(&["fast", "team", "twin"]), false, false)
        );
        // One profile with another model still matches, and says so.
        assert_eq!(
            matched(&catalog, "openai", Some("gpt-other")),
            (names(&["default"]), true, false)
        );
    }

    #[test]
    fn the_default_entry_resumes_providers_that_plain_codex_knows() {
        let (_home, catalog) = catalog(
            &format!(
                "model = 'gpt-base'\nmodel_provider = 'gateway'\n{GATEWAY}[model_providers.lab]\nbase_url = 'http://lab.example/v1'\n"
            ),
            &[("work", "model = 'work-model'\n")],
        );
        let names = |list: &[&str]| {
            list.iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        };
        // Both entries use the gateway, so the model decides between them.
        assert_eq!(
            matched(&catalog, "gateway", Some("work-model")),
            (names(&["work"]), false, false)
        );
        // No profile uses these, but config.toml defines one, and Codex
        // has the other built in.
        assert_eq!(
            matched(&catalog, "lab", Some("lab-model")),
            (names(&["default"]), true, true)
        );
        assert_eq!(
            matched(&catalog, "ollama", None),
            (names(&["default"]), false, true)
        );
        assert_eq!(
            matched(&catalog, "elsewhere", None),
            (Vec::new(), false, false)
        );
    }

    #[test]
    fn harnesses_broken_profiles_and_undefined_providers_never_match() {
        let (home, _) = catalog("model = 'gpt-base'\n", &[]);
        fs::write(
            home.path().join("broken.config.toml"),
            "model_provider = 'gateway'\nmodel = \n",
        )
        .unwrap();
        fs::write(
            home.path().join("undefined.config.toml"),
            "model_provider = 'gateway'\n",
        )
        .unwrap();
        fs::write(
            home.path().join(crate::wrappers::FILE_NAME),
            "[[harness]]\nname = 'chat'\nharness = 'Chat'\nprovider = 'gateway'\nlaunch = ['/opt/chat']\n",
        )
        .unwrap();
        let loaded = crate::profile::load(home.path()).unwrap();
        assert_eq!(loaded.profiles.len(), 4);
        assert_eq!(
            matched(&loaded, "gateway", None),
            (Vec::new(), false, false)
        );
        // A broken config.toml leaves even built-in providers unresumable.
        let (_home, broken) = catalog("model = \n", &[]);
        assert_eq!(matched(&broken, "openai", None), (Vec::new(), false, false));
    }

    #[test]
    fn times_are_brief_and_utc() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let before = |seconds: u64| ago(now - Duration::from_secs(seconds), now);
        assert_eq!(before(0), "just now");
        assert_eq!(ago(now + HOUR, now), "just now");
        assert_eq!(before(59), "just now");
        assert_eq!(before(60), "1m ago");
        assert_eq!(before(3599), "59m ago");
        assert_eq!(before(3600), "1h ago");
        assert_eq!(before(86_399), "23h ago");
        assert_eq!(before(86_400), "1d ago");
        assert_eq!(before(7 * 86_400), "1w ago");
        assert_eq!(before(28 * 86_400), "1mo ago");
        assert_eq!(before(364 * 86_400), "12mo ago");
        assert_eq!(before(365 * 86_400), "1y ago");
        assert_eq!(utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            utc_minute(UNIX_EPOCH + Duration::from_secs(1_795_788_192)),
            "2026-11-27 14:03 UTC"
        );
        assert_eq!(utc(now), "2027-01-15T08:00:00Z");
        assert_eq!(
            utc(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
        assert_eq!(
            utc(UNIX_EPOCH + Duration::from_secs(1_795_788_192)),
            "2026-11-27T14:03:12Z"
        );
    }

    #[test]
    fn clean_text_cannot_move_the_cursor() {
        assert_eq!(clean("a\u{1b}[2Jb\nc"), "a [2Jb c");
    }
}
