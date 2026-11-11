//! Codex profile discovery, effective settings, and secret redaction.
//!
//! Codex treats `$CODEX_HOME/<name>.config.toml` as a profile layer that is
//! merged table by table on top of `$CODEX_HOME/config.toml`.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

use crate::wrappers::{self, Wrapper};

/// The menu entry that runs Codex with only the base configuration.
pub const DEFAULT_PROFILE: &str = "default";
pub const DEFAULT_DESCRIPTION: &str = "Runs Codex with config.toml only, without a profile.";

const PROFILE_SUFFIX: &str = ".config.toml";
const BASE_CONFIG: &str = "config.toml";
const MAX_CONFIG_BYTES: u64 = 1 << 20;
pub const REDACTED: &str = "<redacted>";

/// Providers that Codex knows without a `[model_providers.<id>]` table.
pub const BUILTIN_PROVIDERS: &[&str] = &[
    "amazon-bedrock",
    "amazon-bedrock-runtime",
    "gcp",
    "lmstudio",
    "ollama",
    "openai",
];

/// Key name fragments whose string values are treated as secrets.
const SECRET_KEY_PARTS: &[&str] = &[
    "api_key",
    "apikey",
    "authorization",
    "bearer",
    "cookie",
    "credential",
    "passwd",
    "password",
    "private_key",
    "secret",
    "token",
];

/// Tables whose string values are all treated as secrets.
const OPAQUE_TABLES: &[&str] = &["env", "headers", "http_headers", "query_params", "set"];

/// Whether `name` is a profile name that Codex accepts for `--profile`.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Where an effective setting comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The profile file.
    Profile,
    /// config.toml.
    Base,
    /// Codex's built-in default.
    Builtin,
    /// A temporary choice in the launcher.
    Override,
    /// An explicit Codex command-line setting.
    Arguments,
}

/// One effective setting, with the layer that supplied it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Setting {
    pub value: String,
    pub source: Source,
}

/// The model provider that a profile resolves to.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Provider {
    pub id: String,
    pub source: Source,
    pub name: Option<String>,
    pub base_url: Option<String>,
    /// The layer whose `[model_providers]` table defines it, or `None` when built in.
    pub defined_in: Option<Source>,
    /// Set when the provider id matches neither a table nor a built-in provider.
    pub missing: bool,
}

/// One menu entry: either the base configuration or a profile layer.
#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    /// The profile file, or config.toml for the default entry.
    pub path: PathBuf,
    /// The leading comment block of the file, without `#` markers.
    pub description: Option<String>,
    pub model: Option<Setting>,
    pub provider: Provider,
    pub effort: Option<Setting>,
    pub model_catalog: Option<Setting>,
    pub context_window: Option<Setting>,
    pub auto_compact_token_limit: Option<Setting>,
    pub sandbox: Option<Setting>,
    pub approval: Option<Setting>,
    /// The redacted file text, or `None` when the file does not exist.
    pub raw: Option<String>,
    /// A read or parse problem that makes the settings incomplete.
    pub error: Option<String>,
    /// The commands that codex-launcher.toml gives this profile. A profile's
    /// launch [`Command`](crate::launch::Command) is built from this once, so
    /// read that command for what runs.
    pub wrapper: Option<Wrapper>,
}

impl Profile {
    pub fn is_default(&self) -> bool {
        self.name == DEFAULT_PROFILE
    }

    /// The effective model name, if any layer sets one.
    pub fn model_name(&self) -> Option<&str> {
        self.model.as_ref().map(|setting| setting.value.as_str())
    }
}

/// The profiles of one Codex home.
#[derive(Clone, Debug)]
pub struct Catalog {
    pub codex_home: PathBuf,
    /// The default entry first, then profiles sorted by name.
    pub profiles: Vec<Profile>,
    /// Problems that did not stop loading, such as skipped file names.
    pub warnings: Vec<String>,
}

impl Catalog {
    pub fn find(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.name == name)
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.profiles
            .iter()
            .position(|profile| profile.name == name)
    }
}

/// The home directory, when `HOME` names an absolute path. A relative one
/// would make `~/` paths depend on the current directory.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
}

/// Resolve the Codex home directory the same way Codex does.
pub fn codex_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .context("HOME is not set; set CODEX_HOME to the Codex configuration directory")?;
    Ok(PathBuf::from(home).join(".codex"))
}

/// Load the default entry and every profile layer in `codex_home`.
pub fn load(codex_home: &Path) -> Result<Catalog> {
    let mut warnings = Vec::new();
    let base_path = codex_home.join(BASE_CONFIG);
    let base_file = read_config(&base_path);
    if let Some(error) = &base_file.error {
        warnings.push(format!("{BASE_CONFIG}: {error}"));
    }
    let empty = Table::new();
    let base = base_file
        .document
        .as_ref()
        .map_or(&empty, |document| document.as_table());

    let mut names = Vec::new();
    let entries = fs::read_dir(codex_home)
        .with_context(|| format!("cannot read Codex home {}", codex_home.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot list {}", codex_home.display()))?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        // "config.toml" has no name before the suffix, so it never matches.
        let Some(name) = file_name.strip_suffix(PROFILE_SUFFIX) else {
            continue;
        };
        if !entry.path().is_file() {
            continue;
        }
        if !is_valid_name(name) {
            warnings.push(format!(
                "skipped {file_name}: profile names use only letters, digits, '-', and '_'"
            ));
            continue;
        }
        if name == DEFAULT_PROFILE {
            warnings.push(format!(
                "skipped {file_name}: \"{DEFAULT_PROFILE}\" is the launcher's name for config.toml"
            ));
            continue;
        }
        names.push(name.to_owned());
    }
    names.sort();
    let mut wrappers = wrappers::load(codex_home, &names, home_dir().as_deref(), &mut warnings);

    let mut profiles = Vec::with_capacity(names.len() + 1);
    profiles.push(build_profile(
        DEFAULT_PROFILE.to_owned(),
        base_path,
        &base_file,
        None,
        base,
    ));
    for name in names {
        let path = codex_home.join(format!("{name}{PROFILE_SUFFIX}"));
        let file = read_config(&path);
        let layer = file.document.as_ref().map(DocumentMut::as_table);
        let mut profile = build_profile(name, path, &file, layer, base);
        profile.wrapper = wrappers.remove(&profile.name);
        profiles.push(profile);
    }

    Ok(Catalog {
        codex_home: codex_home.to_path_buf(),
        profiles,
        warnings,
    })
}

/// Save only the effort key using the original file, never the redacted view.
/// Re-read before editing to preserve unrelated changes made while the menu was
/// open. An atomic replacement keeps a failed write from truncating the config.
pub fn save_effort(profile: &Profile, effort: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let path = &profile.path;
    let metadata = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            bail!(
                "cannot save effort: {} is not a regular file",
                path.display()
            );
        }
        Ok(meta) => Some(meta),
        Err(error) if error.kind() == ErrorKind::NotFound && profile.is_default() => None,
        Err(error) => return Err(error).context("cannot inspect profile file"),
    };
    let original = if metadata.is_some() {
        read_limited(path).context("cannot read profile file")?
    } else {
        String::new()
    };
    let mut document = original
        .parse::<DocumentMut>()
        .map_err(|error| anyhow::anyhow!("cannot save invalid TOML: {}", error.message()))?;
    if document
        .get("model_reasoning_effort")
        .is_some_and(|item| !item.is_str())
    {
        bail!("cannot save: model_reasoning_effort must be a string");
    }
    let mut value = Value::from(effort);
    if let Some(old) = document
        .get("model_reasoning_effort")
        .and_then(Item::as_value)
    {
        *value.decor_mut() = old.decor().clone();
    }
    document["model_reasoning_effort"] = Item::Value(value);
    let parent = path
        .parent()
        .context("profile path has no parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let permissions = metadata.map_or_else(
        || fs::Permissions::from_mode(0o600),
        |meta| meta.permissions(),
    );
    temporary.as_file().set_permissions(permissions)?;
    temporary.write_all(document.to_string().as_bytes())?;
    temporary.as_file().sync_all()?;
    // Detect edits during serialization rather than overwriting them.
    if path.exists() && read_limited(path)? != original {
        bail!("profile changed while saving; try again");
    }
    temporary
        .persist(path)
        .context("cannot replace profile file")?;
    Ok(())
}

struct ConfigFile {
    text: Option<String>,
    document: Option<DocumentMut>,
    error: Option<String>,
}

fn read_config(path: &Path) -> ConfigFile {
    let text = match read_limited(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return ConfigFile {
                text: None,
                document: None,
                error: None,
            };
        }
        Err(error) => {
            return ConfigFile {
                text: None,
                document: None,
                error: Some(format!("cannot read file: {error}")),
            };
        }
    };
    match text.parse::<DocumentMut>() {
        Ok(document) => ConfigFile {
            text: Some(text),
            document: Some(document),
            error: None,
        },
        Err(error) => ConfigFile {
            text: Some(text),
            document: None,
            error: Some(format!("invalid TOML: {}", error.message())),
        },
    }
}

pub(crate) fn read_limited(path: &Path) -> std::io::Result<String> {
    let size = fs::metadata(path)?.len();
    if size > MAX_CONFIG_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("file is larger than {MAX_CONFIG_BYTES} bytes"),
        ));
    }
    fs::read_to_string(path)
}

/// The same base/profile table merge Codex performs for runtime commands.
/// Used for a private config snapshot when a diagnostic lacks --profile.
pub fn merged_config(codex_home: &Path, profile: &Profile) -> Result<DocumentMut> {
    fn merge(base: &mut dyn TableLike, layer: &dyn TableLike) {
        for (key, value) in layer.iter() {
            if let Some(target) = base.get_mut(key).and_then(Item::as_table_like_mut)
                && let Some(source) = value.as_table_like()
            {
                merge(target, source);
            } else {
                base.insert(key, value.clone());
            }
        }
    }

    let base = read_config(&codex_home.join(BASE_CONFIG));
    if let Some(error) = base.error {
        bail!("{error}");
    }
    let mut document = base.document.unwrap_or_default();
    if !profile.is_default() {
        let layer = read_config(&profile.path);
        if let Some(error) = layer.error {
            bail!("{error}");
        }
        let layer = layer.document.context("profile file no longer exists")?;
        merge(document.as_table_mut(), layer.as_table());
    }
    Ok(document)
}

/// Build one entry. `layer` is the profile table, or `None` for the default entry.
fn build_profile(
    name: String,
    path: PathBuf,
    file: &ConfigFile,
    layer: Option<&Table>,
    base: &Table,
) -> Profile {
    let raw = redacted_text(file);
    let description = if layer.is_none() && name == DEFAULT_PROFILE {
        Some(DEFAULT_DESCRIPTION.to_owned())
    } else if file.document.is_some() {
        // Read the redacted text, so a commented-out secret cannot leak.
        raw.as_deref().and_then(leading_comment)
    } else {
        None
    };
    Profile {
        name,
        path,
        description,
        model: setting(layer, base, "model"),
        provider: resolve_provider(layer, base),
        effort: setting(layer, base, "model_reasoning_effort"),
        model_catalog: setting(layer, base, "model_catalog_json"),
        context_window: setting(layer, base, "model_context_window"),
        auto_compact_token_limit: setting(layer, base, "model_auto_compact_token_limit"),
        sandbox: setting(layer, base, "sandbox_mode"),
        approval: setting(layer, base, "approval_policy"),
        raw,
        error: file.error.clone(),
        wrapper: None,
    }
}

/// Look up a top-level scalar, preferring the profile layer over the base.
fn setting(layer: Option<&Table>, base: &Table, key: &str) -> Option<Setting> {
    if let Some(value) = layer.and_then(|table| scalar(table, key)) {
        return Some(Setting {
            value,
            source: Source::Profile,
        });
    }
    scalar(base, key).map(|value| Setting {
        value,
        source: Source::Base,
    })
}

fn scalar(table: &Table, key: &str) -> Option<String> {
    match table.get(key)?.as_value()? {
        Value::String(text) => Some(text.value().clone()),
        Value::Integer(number) => Some(number.value().to_string()),
        Value::Float(number) => Some(number.value().to_string()),
        Value::Boolean(flag) => Some(flag.value().to_string()),
        Value::Datetime(date) => Some(date.value().to_string()),
        Value::Array(_) | Value::InlineTable(_) => None,
    }
}

fn resolve_provider(layer: Option<&Table>, base: &Table) -> Provider {
    let (id, source) = match setting(layer, base, "model_provider") {
        Some(setting) => (setting.value, setting.source),
        None => ("openai".to_owned(), Source::Builtin),
    };
    // Codex merges a profile's provider table into the base one field by
    // field, so each field comes from the profile when set there.
    let tables: Vec<(&dyn TableLike, Source)> =
        [(layer, Source::Profile), (Some(base), Source::Base)]
            .into_iter()
            .filter_map(|(table, source)| Some((provider_table(table?, &id)?, source)))
            .collect();
    let field = |key: &str| {
        tables
            .iter()
            .find_map(|(table, source)| Some((table.get(key)?.as_str()?, *source)))
    };
    let name = field("name");
    Provider {
        missing: tables.is_empty() && !BUILTIN_PROVIDERS.contains(&id.as_str()),
        // The table that names the provider defines it.
        defined_in: name
            .map(|(_, source)| source)
            .or_else(|| tables.first().map(|(_, source)| *source)),
        name: name.map(|(value, _)| value.to_owned()),
        base_url: field("base_url").map(|(value, _)| redact_url(value)),
        id,
        source,
    }
}

fn provider_table<'a>(table: &'a Table, id: &str) -> Option<&'a dyn TableLike> {
    table
        .get("model_providers")?
        .as_table_like()?
        .get(id)?
        .as_table_like()
}

/// The first comment block before any TOML content, joined into one paragraph.
fn leading_comment(text: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if lines.is_empty() {
                continue;
            }
            break;
        }
        let Some(comment) = trimmed.strip_prefix('#') else {
            break;
        };
        let comment = comment.trim();
        if !comment.is_empty() {
            lines.push(comment.to_owned());
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join(" "))
    }
}

fn redacted_text(file: &ConfigFile) -> Option<String> {
    match (&file.document, &file.text) {
        (Some(document), _) => {
            let mut document = document.clone();
            redact_table(document.as_table_mut(), false);
            Some(redact_comment_lines(&document.to_string()))
        }
        (None, Some(_)) => Some(format!(
            "# Not shown: the file does not parse, so secrets cannot be found reliably.\n# {}\n",
            file.error.as_deref().unwrap_or("unknown error")
        )),
        (None, None) => None,
    }
}

/// Whether a key name looks like it holds a credential.
///
/// Keys such as `env_key` and `bearer_token_env_var` name an environment
/// variable instead of holding its value, so they stay visible.
fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace('-', "_");
    if key.ends_with("env_key") || key.ends_with("_env") || key.ends_with("_env_var") {
        return false;
    }
    SECRET_KEY_PARTS.iter().any(|part| key.contains(part))
}

fn is_opaque_table(key: &str) -> bool {
    OPAQUE_TABLES.contains(&key.to_ascii_lowercase().as_str())
}

fn redact_table(table: &mut Table, opaque: bool) {
    for (key, item) in table.iter_mut() {
        let key = key.get().to_owned();
        match item {
            Item::Value(value) => redact_value(&key, value, opaque),
            Item::Table(inner) => redact_table(inner, opaque || is_opaque_table(&key)),
            Item::ArrayOfTables(tables) => {
                for inner in tables.iter_mut() {
                    redact_table(inner, opaque || is_opaque_table(&key));
                }
            }
            Item::None => {}
        }
    }
}

/// Redact string values only: numbers and booleans are never credentials.
fn redact_value(key: &str, value: &mut Value, opaque: bool) {
    match value {
        Value::InlineTable(table) => {
            let opaque = opaque || is_opaque_table(key);
            for (inner_key, inner) in table.iter_mut() {
                let inner_key = inner_key.get().to_owned();
                redact_value(&inner_key, inner, opaque);
            }
        }
        Value::Array(array) => {
            for element in array.iter_mut() {
                redact_value(key, element, opaque);
            }
        }
        Value::String(_) if opaque || is_secret_key(key) => replace_string(value, REDACTED),
        Value::String(text) => {
            let cleaned = redact_url(text.value());
            if cleaned != *text.value() {
                replace_string(value, &cleaned);
            }
        }
        _ => {}
    }
}

/// Replace a value with a string, keeping its surrounding whitespace and comments.
fn replace_string(value: &mut Value, text: &str) {
    let decor = value.decor().clone();
    let mut replacement = Value::from(text);
    *replacement.decor_mut() = decor;
    *value = replacement;
}

/// Hide the values of commented-out secret assignments, such as `# api_key = "..."`.
fn redact_comment_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        match commented_secret(line) {
            Some(assignment) => {
                let newline = if line.ends_with('\n') { "\n" } else { "" };
                out.push_str(assignment);
                out.push_str(" \"");
                out.push_str(REDACTED);
                out.push('"');
                out.push_str(newline);
            }
            None => out.push_str(line),
        }
    }
    out
}

/// For a line such as `# api_key = "x"`, the text up to and including `=`.
fn commented_secret(line: &str) -> Option<&str> {
    let body = line.trim_start().strip_prefix('#')?;
    let (key, _) = body.split_once('=')?;
    let key = key.trim();
    let is_key = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-.\"'".contains(c));
    let last = key
        .rsplit('.')
        .next()
        .unwrap_or(key)
        .trim_matches(['"', '\'']);
    if !is_key || !is_secret_key(last) {
        return None;
    }
    let equals = line.find('=')?;
    Some(&line[..=equals])
}

/// Hide `user:password@` credentials and query-string values in a URL.
///
/// Query parameters such as `?key=...` often carry API keys, so every value is
/// hidden and only the parameter names stay visible.
pub fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_owned();
    };
    let prefix = &url[..scheme_end + 3];
    let rest = &url[scheme_end + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let host = match authority.rfind('@') {
        Some(at) => format!("{REDACTED}@{}", &authority[at + 1..]),
        None => authority.to_owned(),
    };
    let (before_fragment, fragment) = match tail.find('#') {
        Some(hash) => tail.split_at(hash),
        None => (tail, ""),
    };
    let tail = match before_fragment.split_once('?') {
        Some((path, query)) => {
            let query: Vec<String> = query
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((name, value)) if !value.is_empty() => format!("{name}={REDACTED}"),
                    _ => pair.to_owned(),
                })
                .collect();
            format!("{path}?{}{fragment}", query.join("&"))
        }
        None => tail.to_owned(),
    };
    format!("{prefix}{host}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_preserves_comments_credentials_and_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("work.config.toml");
        fs::write(&path, "# Profile description\nmodel_reasoning_effort = 'low' # Keep this\napi_key = 'secret-value'\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let catalog = load(home.path()).unwrap();
        // An unrelated edit after the menu loaded must survive the save.
        let original = fs::read_to_string(&path).unwrap() + "model = 'new-model'\n";
        fs::write(&path, &original).unwrap();
        save_effort(catalog.find("work").unwrap(), "high").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original.replace("'low'", "\"high\"")
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let updated = load(home.path()).unwrap();
        let profile = updated.find("work").unwrap();
        assert_eq!(profile.effort.as_ref().unwrap().value, "high");
        assert!(!profile.raw.as_ref().unwrap().contains("secret-value"));
    }

    #[test]
    fn save_refuses_invalid_toml_and_symlinks() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("work.config.toml");
        fs::write(&path, "model = 'test'\n").unwrap();
        let catalog = load(home.path()).unwrap();
        let profile = catalog.find("work").unwrap();
        fs::write(&path, "not valid [").unwrap();
        assert!(save_effort(profile, "high").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "not valid [");
        fs::remove_file(&path).unwrap();
        let target = home.path().join("target.toml");
        fs::write(&target, "model = 'test'\n").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(save_effort(profile, "high").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "model = 'test'\n");
    }

    #[test]
    fn saving_default_creates_private_config() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let catalog = load(home.path()).unwrap();
        save_effort(catalog.find("default").unwrap(), "high").unwrap();
        let path = home.path().join("config.toml");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "model_reasoning_effort = \"high\"\n"
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    fn home_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            fs::write(dir.path().join(name), text).unwrap();
        }
        dir
    }

    const BASE: &str = r#"
model = "base-model"
model_reasoning_effort = "high"
sandbox_mode = "workspace-write"
approval_policy = "on-request"

[model_providers.lab]
name = "Lab server"
base_url = "http://lab.example:11434/v1"
"#;

    #[test]
    fn valid_names_match_codex() {
        for name in ["work", "Up-per_1", "_x", "9x", "x-", "-x"] {
            assert!(is_valid_name(name), "{name}");
        }
        for name in ["", "a.b", "a b", "../x", "é", "a/b"] {
            assert!(!is_valid_name(name), "{name}");
        }
    }

    #[test]
    fn lists_default_first_then_sorted_profiles() {
        let home = home_with(&[
            ("config.toml", BASE),
            ("zeta.config.toml", "model = \"z\"\n"),
            ("alpha.config.toml", "model = \"a\"\n"),
            ("notes.toml", "model = \"ignored\"\n"),
            ("bad.name.config.toml", "model = \"x\"\n"),
            ("default.config.toml", "model = \"x\"\n"),
        ]);
        fs::create_dir(home.path().join("dir.config.toml")).unwrap();
        let catalog = load(home.path()).unwrap();
        let names: Vec<_> = catalog.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["default", "alpha", "zeta"]);
        assert_eq!(catalog.warnings.len(), 2, "{:?}", catalog.warnings);
    }

    #[test]
    fn profile_values_override_base_and_report_source() {
        let home = home_with(&[
            ("config.toml", BASE),
            (
                "local.config.toml",
                "# Local lab model.\n# Second line.\n\nmodel = \"lab-model\"\n\
                 model_provider = \"lab\"\nmodel_context_window = 65536\n",
            ),
        ]);
        let catalog = load(home.path()).unwrap();
        let local = catalog.find("local").unwrap();
        assert_eq!(
            local.description.as_deref(),
            Some("Local lab model. Second line.")
        );
        assert_eq!(
            local.model,
            Some(Setting {
                value: "lab-model".into(),
                source: Source::Profile
            })
        );
        assert_eq!(local.effort.as_ref().unwrap().source, Source::Base);
        assert_eq!(local.context_window.as_ref().unwrap().value, "65536");
        assert_eq!(local.sandbox.as_ref().unwrap().value, "workspace-write");
        assert_eq!(local.provider.id, "lab");
        assert_eq!(local.provider.defined_in, Some(Source::Base));
        assert_eq!(local.provider.name.as_deref(), Some("Lab server"));
        assert!(!local.provider.missing);

        let default = catalog.find("default").unwrap();
        assert_eq!(default.description.as_deref(), Some(DEFAULT_DESCRIPTION));
        assert_eq!(default.model.as_ref().unwrap().source, Source::Base);
        assert_eq!(default.provider.id, "openai");
        assert_eq!(default.provider.source, Source::Builtin);
        assert!(!default.provider.missing);
    }

    #[test]
    fn unknown_provider_is_flagged() {
        let home = home_with(&[("x.config.toml", "model_provider = \"nowhere\"\n")]);
        let catalog = load(home.path()).unwrap();
        assert!(catalog.find("x").unwrap().provider.missing);
    }

    #[test]
    fn every_builtin_provider_is_known() {
        for id in BUILTIN_PROVIDERS {
            let text = format!("model_provider = \"{id}\"\n");
            let home = home_with(&[("x.config.toml", &text)]);
            let catalog = load(home.path()).unwrap();
            let provider = &catalog.find("x").unwrap().provider;
            assert!(!provider.missing, "{id}");
            assert_eq!(provider.defined_in, None, "{id}");
        }
    }

    #[test]
    fn provider_fields_merge_across_layers() {
        let base = "[model_providers.lab]\nname = \"Lab\"\nbase_url = \"http://lab.example/v1\"\n";
        let home = home_with(&[
            ("config.toml", base),
            (
                "keyed.config.toml",
                "model_provider = \"lab\"\n\n[model_providers.lab]\nenv_key = \"LAB_KEY\"\n",
            ),
            (
                "moved.config.toml",
                "model_provider = \"lab\"\n\n[model_providers.lab]\n\
                 base_url = \"http://other.example/v1\"\n",
            ),
            (
                "renamed.config.toml",
                "model_provider = \"lab\"\n\n[model_providers.lab]\nname = \"Mine\"\n",
            ),
        ]);
        let catalog = load(home.path()).unwrap();

        let keyed = &catalog.find("keyed").unwrap().provider;
        assert_eq!(keyed.name.as_deref(), Some("Lab"));
        assert_eq!(keyed.base_url.as_deref(), Some("http://lab.example/v1"));
        assert_eq!(keyed.defined_in, Some(Source::Base));
        assert!(!keyed.missing);

        let moved = &catalog.find("moved").unwrap().provider;
        assert_eq!(moved.name.as_deref(), Some("Lab"));
        assert_eq!(moved.base_url.as_deref(), Some("http://other.example/v1"));

        let renamed = &catalog.find("renamed").unwrap().provider;
        assert_eq!(renamed.name.as_deref(), Some("Mine"));
        assert_eq!(renamed.base_url.as_deref(), Some("http://lab.example/v1"));
        assert_eq!(renamed.defined_in, Some(Source::Profile));
    }

    #[test]
    fn descriptions_never_show_secrets() {
        let home = home_with(&[
            (
                "commented.config.toml",
                "# Lab profile.\n# api_key = \"sk-in-comment\"\nmodel = \"m\"\n",
            ),
            (
                "broken.config.toml",
                "# token = \"sk-in-broken-file\"\nmodel = \n",
            ),
        ]);
        let catalog = load(home.path()).unwrap();
        let commented = catalog.find("commented").unwrap();
        let description = commented.description.as_deref().unwrap();
        assert!(!description.contains("sk-in-comment"), "{description}");
        assert!(description.starts_with("Lab profile."), "{description}");
        assert!(description.contains(REDACTED), "{description}");
        assert_eq!(catalog.find("broken").unwrap().description, None);
    }

    #[test]
    fn missing_base_config_is_fine() {
        let home = home_with(&[("x.config.toml", "model = \"m\"\n")]);
        let catalog = load(home.path()).unwrap();
        let default = catalog.find("default").unwrap();
        assert!(default.raw.is_none());
        assert!(default.model.is_none());
        assert!(catalog.warnings.is_empty());
    }

    #[test]
    fn invalid_toml_is_reported_and_not_shown() {
        let home = home_with(&[("broken.config.toml", "api_key = \"sk-live\nmodel = \n")]);
        let catalog = load(home.path()).unwrap();
        let broken = catalog.find("broken").unwrap();
        assert!(broken.error.as_deref().unwrap().starts_with("invalid TOML"));
        let raw = broken.raw.as_deref().unwrap();
        assert!(!raw.contains("sk-live"), "{raw}");
    }

    #[test]
    fn redacts_secrets_but_keeps_env_var_names() {
        let text = r#"# Keep this comment.
model = "m"
model_provider = "gw"
model_auto_compact_token_limit = 90000
api_key = "sk-top-secret" # trailing comment
# api_key = "sk-old-secret"
#model_instructions_file = "/tmp/instructions.txt"

[model_providers.gw]
base_url = "https://user:hunter2@gw.example/v1"
env_key = "GW_API_KEY"
http_headers = { "X-Custom" = "header-secret" }
query_params = { "api-version" = "2025-01-01", token = "query-secret" }

[mcp_servers.tool]
command = "tool"
args = ["--verbose"]
bearer_token_env_var = "TOOL_TOKEN"

[mcp_servers.tool.env]
SOMETHING = "env-secret"
"#;
        let home = home_with(&[("x.config.toml", text)]);
        let catalog = load(home.path()).unwrap();
        let profile = catalog.find("x").unwrap();
        let raw = profile.raw.as_deref().unwrap();
        for secret in [
            "sk-top-secret",
            "sk-old-secret",
            "hunter2",
            "header-secret",
            "query-secret",
            "env-secret",
        ] {
            assert!(!raw.contains(secret), "{secret} leaked:\n{raw}");
        }
        for kept in [
            "# Keep this comment.",
            "# trailing comment",
            "model_auto_compact_token_limit = 90000",
            "#model_instructions_file = \"/tmp/instructions.txt\"",
            "GW_API_KEY",
            "TOOL_TOKEN",
            "--verbose",
            "https://<redacted>@gw.example/v1",
        ] {
            assert!(raw.contains(kept), "{kept} missing:\n{raw}");
        }
        assert!(raw.contains("# api_key = \"<redacted>\"\n"), "{raw}");
        assert_eq!(
            profile.provider.base_url.as_deref(),
            Some("https://<redacted>@gw.example/v1")
        );
    }

    #[test]
    fn url_redaction_hides_userinfo_and_query_values() {
        assert_eq!(redact_url("http://h:1/v1"), "http://h:1/v1");
        assert_eq!(redact_url("http://u@h/a@b"), "http://<redacted>@h/a@b");
        assert_eq!(
            redact_url("https://h/v1?key=abc&flag&x=#frag"),
            "https://h/v1?key=<redacted>&flag&x=#frag"
        );
        assert_eq!(redact_url("https://h/v1#a?b=c"), "https://h/v1#a?b=c");
        assert_eq!(redact_url("not a url"), "not a url");
    }

    #[test]
    fn leading_comment_stops_at_content_or_blank_line() {
        assert_eq!(leading_comment("model = \"x\"\n# later\n"), None);
        assert_eq!(
            leading_comment("\n# One.\n#\n# Two.\n\n# Not this.\n"),
            Some("One. Two.".into())
        );
    }
}
