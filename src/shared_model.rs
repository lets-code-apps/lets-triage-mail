//! The model both Let's Code apps share: `~/.config/lets-code/model.toml` (see `docs/MODEL.md`).
//!
//! Reading never fails: a missing, unreadable or invalid file is simply an empty one (the last
//! two also log a warning). Writing validates first, then goes through a temporary file in the
//! same folder and a rename, so the other app sees the old file or the new one, never half.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// The file both apps read, relative to the home directory. Each app expands the `~` with its
/// own `expand_home`.
pub const DEFAULT_PATH: &str = "~/.config/lets-code/model.toml";

/// Allowed `runtime` values.
pub const RUNTIMES: [&str; 3] = ["ollama", "lmstudio", "custom"];

/// Allowed `set_by` values: which app wrote the file last.
pub const WRITERS: [&str; 2] = ["rag docs", "triage mail"];

/// All keys optional. No `deny_unknown_fields` (unlike the app configs): the other app may
/// grow keys, and that must not break this one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedModel {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
}

impl SharedModel {
    /// The model name, or `None` if missing or blank. Borrows from `self`: `as_deref` turns
    /// `&Option<String>` into `Option<&str>` without copying.
    pub fn chat_model(&self) -> Option<&str> {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
    }

    /// `url` plus `/v1` (the OpenAI-compatible API path), unless it already ends in `/v1`.
    /// A blank value counts as missing. Builds and returns an owned `String`, because it
    /// extends the borrowed text.
    pub fn chat_base_url(&self) -> Option<String> {
        let url = self.url.as_deref()?.trim().trim_end_matches('/');
        if url.is_empty() {
            None
        } else if url.ends_with("/v1") {
            Some(url.to_string())
        } else {
            Some(format!("{url}/v1"))
        }
    }
}

/// Reads the shared setting. A missing, unreadable or invalid file gives the empty value (the
/// last two log a warning), so callers never branch on a read failure.
pub fn read(path: &Path) -> SharedModel {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return SharedModel::default(),
        Err(e) => {
            tracing::warn!("cannot read {}: {e}; ignoring it", path.display());
            return SharedModel::default();
        }
    };
    toml::from_str(&text).unwrap_or_else(|e| {
        tracing::warn!("invalid {}: {e}; ignoring it", path.display());
        SharedModel::default()
    })
}

/// Replaces the whole file. Validates first, so nothing is written on refusal. The new text
/// goes to a temporary file in the same folder (`rename` is atomic only within one filesystem),
/// is flushed to disk, then renamed over the target. `updated` is the caller's RFC 3339 string.
pub fn write(path: &Path, model: &SharedModel) -> anyhow::Result<()> {
    validate(model)?;
    let text = toml::to_string(model).context("cannot serialise the model setting")?;
    let dir = path.parent().context("the model file path has no folder")?;
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!(".model.toml.{}.tmp", std::process::id()));
    let written = write_synced(&tmp, &text).and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(tmp);
    }
    written.with_context(|| format!("cannot write {}", path.display()))
}

/// Create, write, then `sync_all` before the rename, so a crash leaves the old file or the
/// new one, never an empty one.
fn write_synced(path: &Path, text: &str) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// The rules `write` enforces; the same ones `docs/MODEL.md` states.
fn validate(model: &SharedModel) -> anyhow::Result<()> {
    if !RUNTIMES.contains(&model.runtime.as_deref().unwrap_or_default()) {
        bail!("runtime must be one of {RUNTIMES:?}");
    }
    if !WRITERS.contains(&model.set_by.as_deref().unwrap_or_default()) {
        bail!("set_by must be one of {WRITERS:?}");
    }
    let Some(url) = model.chat_base_url() else {
        bail!("url must not be empty");
    };
    if url.contains('@') {
        bail!("url must not contain credentials");
    }
    if model.chat_model().is_none() {
        bail!("model must not be empty");
    }
    if model.updated.as_deref().is_none_or(|u| u.trim().is_empty()) {
        bail!("updated must be an RFC 3339 timestamp");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// The exact temp file `write` would have used in this folder, in this process.
    fn temp_path(dir: &Path) -> PathBuf {
        dir.join(format!(".model.toml.{}.tmp", std::process::id()))
    }

    /// `valid_model` with one key replaced; an unknown key names a test bug.
    fn changed(key: &str, value: Option<String>) -> SharedModel {
        let mut model = valid_model();
        match key {
            "runtime" => model.runtime = value,
            "set_by" => model.set_by = value,
            "url" => model.url = value,
            "model" => model.model = value,
            "updated" => model.updated = value,
            _ => panic!("unknown key {key}"),
        }
        model
    }

    /// A complete, valid setting, as onboarding or a settings screen would write it.
    fn valid_model() -> SharedModel {
        SharedModel {
            runtime: Some(String::from("ollama")),
            url: Some(String::from("http://127.0.0.1:11434")),
            model: Some(String::from("qwen3:8b")),
            set_by: Some(String::from("triage mail")),
            updated: Some(String::from("2026-11-24T09:00:00+01:00")),
        }
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.toml");
        assert_eq!(read(&path), SharedModel::default());
    }

    #[test]
    fn missing_key_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.toml");
        fs::write(&path, "model = \"qwen3:8b\"\n").unwrap();
        let model = read(&path);
        assert_eq!(model.chat_model(), Some("qwen3:8b"));
        assert_eq!(model.chat_base_url(), None, "a file without url keeps None");
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.toml");
        fs::write(
            &path,
            r#"
            model = "qwen3:8b"
            newer_key = "written by a future app version"

            [extra]
            nested = 1
            "#,
        )
        .unwrap();
        assert_eq!(read(&path).chat_model(), Some("qwen3:8b"));
    }

    #[test]
    fn invalid_toml_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.toml");
        fs::write(&path, "this is not = = toml").unwrap();
        assert_eq!(read(&path), SharedModel::default());
    }

    #[test]
    fn blank_values_count_as_missing() {
        let model = SharedModel {
            runtime: None,
            url: Some(String::from("   ")),
            model: Some(String::from("")),
            set_by: None,
            updated: None,
        };
        assert_eq!(model.chat_base_url(), None);
        assert_eq!(model.chat_model(), None);
    }

    #[test]
    fn base_url_gets_v1_once() {
        let base_url = |url: &str| {
            let empty = SharedModel::default();
            SharedModel {
                url: Some(url.to_string()),
                ..empty
            }
            .chat_base_url()
        };
        assert_eq!(
            base_url("http://127.0.0.1:11434"),
            Some(String::from("http://127.0.0.1:11434/v1"))
        );
        assert_eq!(
            base_url("http://127.0.0.1:11434/"),
            Some(String::from("http://127.0.0.1:11434/v1"))
        );
        assert_eq!(
            base_url("http://127.0.0.1:11434/v1"),
            Some(String::from("http://127.0.0.1:11434/v1"))
        );
    }

    #[test]
    fn write_round_trips_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        // The folder does not exist yet: write creates it.
        let path = dir.path().join("lets-code").join("model.toml");
        write(&path, &valid_model()).unwrap();
        assert_eq!(read(&path), valid_model());

        // Overwriting an existing file keeps the same single file.
        let moved = SharedModel {
            runtime: Some(String::from("lmstudio")),
            url: Some(String::from("http://127.0.0.1:1234/v1")),
            model: Some(String::from("qwen3:4b")),
            set_by: Some(String::from("rag docs")),
            updated: Some(String::from("2026-11-24T10:00:00+01:00")),
        };
        write(&path, &moved).unwrap();
        assert_eq!(read(&path), moved);

        assert!(path.exists(), "model.toml is there after a round trip");
        let folder = path.parent().unwrap();
        assert!(
            !temp_path(folder).exists(),
            "the temp file is renamed, never left behind"
        );
    }

    #[test]
    fn write_refuses_bad_values_and_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.toml");
        write(&path, &valid_model()).unwrap();

        let bad = [
            changed("runtime", Some(String::from("cloud"))),
            changed("set_by", Some(String::from("someone"))),
            changed("url", Some(String::from("http://user:pw@127.0.0.1:11434"))),
            changed("model", Some(String::from("  "))),
            changed("updated", None),
        ];
        for model in bad {
            assert!(write(&path, &model).is_err(), "{model:?} should be refused");
            assert_eq!(
                read(&path),
                valid_model(),
                "old file unchanged after {model:?}"
            );
        }
        assert!(path.exists(), "the good file is still there");
        assert!(
            !temp_path(dir.path()).exists(),
            "a refused write leaves no temp file"
        );
    }
}
