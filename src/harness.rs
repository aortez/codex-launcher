//! Standalone launch targets in codex-launcher.toml. These never inherit Codex
//! configuration, discover Codex models, or fall back to a program on PATH.

use std::collections::BTreeSet;
use std::path::Path;

use toml_edit::{DocumentMut, Item, Table};

use crate::launch::Program;
use crate::profile::{self, Profile, Provider, Setting, Source};
use crate::wrappers::{self, FILE_NAME};

#[derive(Clone, Debug)]
pub struct Harness {
    pub label: String,
    pub launch: Program,
    pub status: Option<Program>,
    pub tools: Option<String>,
}

/// The `[[harness]]` entries of a parsed launcher file, which came from `file`.
/// `existing` are the profiles already loaded, whose names an entry cannot
/// reuse. Problems become warnings.
pub(crate) fn parse(
    file: &Path,
    document: &DocumentMut,
    existing: &[Profile],
    home: Option<&Path>,
    warnings: &mut Vec<String>,
) -> Vec<Profile> {
    let Some(item) = document.get("harness") else {
        return Vec::new();
    };
    let Ok(entries) = item.clone().into_array_of_tables() else {
        warnings.push(format!(
            "{FILE_NAME}: write each harness as a [[harness]] table"
        ));
        return Vec::new();
    };
    let mut names: BTreeSet<String> = existing.iter().map(|p| p.name.clone()).collect();
    let mut profiles = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let parsed = parse_entry(entry, home).and_then(|(name, harness)| {
            if !names.insert(name.clone()) {
                return Err(format!("name {name:?} is already used"));
            }
            Ok((name, harness))
        });
        match parsed {
            Err(problem) => warnings.push(format!(
                "{FILE_NAME}: harness {}: {problem}; entry skipped",
                index + 1
            )),
            Ok((name, harness)) => profiles.push(Profile {
                name,
                path: file.to_path_buf(),
                description: string(entry, "description").map(str::to_owned),
                model: setting(entry, "model"),
                provider: Provider {
                    id: string(entry, "provider")
                        .unwrap_or(&harness.label)
                        .to_owned(),
                    source: Source::Profile,
                    name: None,
                    base_url: None,
                    defined_in: None,
                    missing: false,
                },
                effort: setting(entry, "effort"),
                context_window: entry.get("context_window").map(|item| Setting {
                    value: item.as_integer().expect("validated integer").to_string(),
                    source: Source::Profile,
                }),
                model_catalog: None,
                auto_compact_token_limit: None,
                sandbox: None,
                approval: None,
                raw: Some(format!("[[harness]]\n{}", profile::redacted_entry(entry))),
                error: None,
                wrapper: None,
                harness: Some(harness),
            }),
        }
    }
    profiles
}

/// The names that will become harness entries, so wrapper loading can say why
/// one of them takes no wrapper. Names an entry cannot use are left out and
/// reported by [`parse`]: a name a profile already holds stays the profile's,
/// because the entry reusing it is the one that gets skipped.
pub(crate) fn claimed_names(document: &DocumentMut, profiles: &[String]) -> Vec<String> {
    document
        .get("harness")
        .and_then(Item::as_array_of_tables)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| string(entry, "name"))
                .filter(|name| profile::is_valid_name(name))
                .filter(|name| !profiles.iter().any(|profile| profile == name))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_entry(entry: &Table, home: Option<&Path>) -> Result<(String, Harness), String> {
    const TEXT: &[&str] = &[
        "name",
        "harness",
        "description",
        "model",
        "provider",
        "effort",
        "tools",
    ];
    for (key, item) in entry.iter() {
        if TEXT.contains(&key) {
            if item
                .as_str()
                .is_none_or(|text| text.trim().is_empty() || text.chars().any(char::is_control))
            {
                return Err(format!(
                    "{key} must be a nonempty string without control characters"
                ));
            }
        } else if key == "context_window" {
            if item.as_integer().is_none_or(|value| value <= 0) {
                return Err("context_window must be a positive integer".into());
            }
        } else if key != "launch" && key != "status" {
            return Err(format!("unknown key {key:?}"));
        }
    }
    let name = string(entry, "name").ok_or("name is required")?;
    if !profile::is_valid_name(name) {
        return Err("name must use only letters, digits, '-', and '_'".into());
    }
    let label = string(entry, "harness").ok_or("harness label is required")?;
    let launch = wrappers::command(entry, "launch", name, home)?.ok_or("launch is required")?;
    let status = wrappers::command(entry, "status", name, home)?;
    Ok((
        name.to_owned(),
        Harness {
            label: label.to_owned(),
            launch,
            status,
            tools: string(entry, "tools").map(str::to_owned),
        },
    ))
}

fn string<'a>(entry: &'a Table, key: &str) -> Option<&'a str> {
    entry.get(key)?.as_str()
}

fn setting(entry: &Table, key: &str) -> Option<Setting> {
    string(entry, key).map(|value| Setting {
        value: value.to_owned(),
        source: Source::Profile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(text: &str) -> (tempfile::TempDir, profile::Catalog) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "model = 'codex-model'\nmodel_reasoning_effort = 'high'\nsandbox_mode = 'danger-full-access'\n").unwrap();
        std::fs::write(dir.path().join(FILE_NAME), text).unwrap();
        let catalog = profile::load(dir.path()).unwrap();
        (dir, catalog)
    }

    #[test]
    fn independent_entry_preserves_argv_without_codex_defaults_or_effort_flags() {
        let (dir, catalog) = catalog(
            "[[harness]]\nname='chat'\nharness='Playtime'\nlaunch=['/opt/chat', '{profile}', 'two words']\ncontext_window=32768\n",
        );
        assert!(catalog.warnings.is_empty(), "{:?}", catalog.warnings);
        let p = catalog.find("chat").unwrap();
        assert!(p.model.is_none() && p.effort.is_none() && p.sandbox.is_none());
        assert_eq!(p.provider.id, "Playtime");
        let command =
            crate::launch::Command::for_profile(p, &["--prompt".into(), "hello".into()], None);
        assert_eq!(
            command.argv(),
            ["/opt/chat", "chat", "two words", "--prompt", "hello"]
        );
        assert_eq!(command.with_effort("high"), command);
        assert!(profile::save_effort(p, "high").is_err());
        assert!(
            !std::fs::read_to_string(dir.path().join(FILE_NAME))
                .unwrap()
                .contains("model_reasoning_effort")
        );
        let mut choices = crate::effort::Choices::load(p, dir.path(), &[]);
        assert!(choices.levels.is_empty());
        assert!(choices.discover(
            p,
            dir.path(),
            &[],
            Some(Path::new("/must-not-run")),
            std::time::Duration::ZERO
        ));
        assert_eq!(choices.metadata_source, Some("harness preset"));
    }

    #[test]
    fn malformed_entries_cannot_fall_back_to_codex() {
        let (_, catalog) = catalog(
            "[[harness]]\nname='missing'\nharness='Chat'\n[[harness]]\nname='relative'\nharness='Chat'\nlaunch=['chat']\n[[harness]]\nname='default'\nharness='Chat'\nlaunch=['/opt/chat']\n[[harness]]\nname='bad-status'\nharness='Chat'\nlaunch=['/opt/chat']\nstatus=['status']\n",
        );
        assert_eq!(catalog.profiles.len(), 1);
        assert_eq!(catalog.warnings.len(), 4);
    }

    #[test]
    fn name_collisions_are_reported_and_entry_view_excludes_other_entries() {
        let (dir, _) = catalog("");
        std::fs::write(dir.path().join("work.config.toml"), "model='work-model'").unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "[[harness]]\nname='work'\nharness='Other'\nlaunch=['/opt/other']\n[[harness]]\nname='chat'\nharness='Chat'\nlaunch=['/opt/chat']\n[[harness]]\nname='chat'\nharness='Duplicate'\nlaunch=['/opt/duplicate']\n").unwrap();
        let catalog = profile::load(dir.path()).unwrap();
        assert_eq!(catalog.warnings.len(), 2);
        assert!(catalog.find("work").unwrap().harness.is_none());
        let raw = catalog.find("chat").unwrap().raw.as_deref().unwrap();
        assert!(raw.contains("/opt/chat"));
        assert!(!raw.contains("other") && !raw.contains("Duplicate"));
    }
}
