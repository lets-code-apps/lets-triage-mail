//! The `model` command: the first-run setup commands `detect`, `pull` and `save` (LET-137).
//!
//! These are the three engine calls the desktop onboarding makes; the desktop itself never
//! makes a network call (see `docs/DESKTOP.md`). Every endpoint is local-only, so model text
//! (and, indirectly, mail text) never leaves the user's own network (see `docs/MODEL.md`
//! and `is_local_or_private_host`). The work lives in
//! [`lets_triage_mail::model_setup`]; this file only does the IO and locks the JSON shapes
//! the app reads.

use std::io::Write;
use std::path::Path;

use clap::Subcommand;
use serde_json::{Value, json};

use lets_triage_mail::model_setup;
use lets_triage_mail::shared_model;

/// What to do with the model servers.
#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    /// Probe the running model servers and list their installed models
    Detect {
        /// Probe only this server instead of the two defaults (must be local)
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// Print machine-readable JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Download a model from Ollama, streaming progress lines
    Pull {
        /// Download from this server instead of Ollama's default (must be local)
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// The model to download
        model: String,
        /// Progress is always line-delimited JSON; the flag is accepted for symmetry
        #[arg(long)]
        json: bool,
    },
    /// Record the choice in the shared model.toml
    Save {
        /// One of ollama, lmstudio or custom
        #[arg(long)]
        runtime: String,
        /// A local model server url
        #[arg(long, value_name = "URL")]
        url: String,
        /// The model name, as the server lists it
        #[arg(long)]
        model: String,
        /// Print machine-readable JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

/// What a command prints: the JSON for the app and the short text for people.
struct Outcome {
    json: Value,
    text: String,
}

/// Runs one `model` subcommand.
pub async fn run(shared_path: &Path, command: ModelCommand) -> anyhow::Result<()> {
    // `pull` streams its own line-delimited output and exits on an error line, so it does
    // not go through the JSON/text outcome handler below. Matching `&command` borrows the
    // fields instead of moving `command`, which the second `match` still needs.
    if let ModelCommand::Pull { url, model, .. } = &command {
        return pull(url.as_deref(), model.clone()).await;
    }
    let as_json = wants_json(&command);
    let outcome = match command {
        ModelCommand::Detect { url, .. } => detect(shared_path, url.as_deref()).await,
        ModelCommand::Save {
            runtime,
            url,
            model,
            ..
        } => save(shared_path, runtime, url, model),
        ModelCommand::Pull { .. } => unreachable!("handled above"),
    };
    match outcome {
        Ok(good) if as_json => {
            println!("{}", serde_json::to_string_pretty(&good.json)?);
            Ok(())
        }
        Ok(good) => {
            println!("{}", good.text);
            Ok(())
        }
        Err(error) if as_json => {
            println!("{}", failure_json(&error));
            std::process::exit(1);
        }
        Err(error) => Err(error),
    }
}

/// The `--json` flag of a subcommand. Takes a reference so it does not move the command.
fn wants_json(command: &ModelCommand) -> bool {
    match command {
        ModelCommand::Detect { json, .. } => *json,
        ModelCommand::Pull { json, .. } => *json,
        ModelCommand::Save { json, .. } => *json,
    }
}

/// `model detect`: probe the endpoints and report what answers, plus the shared file.
async fn detect(shared_path: &Path, url: Option<&str>) -> anyhow::Result<Outcome> {
    // A `--url` replaces the two defaults and, like `model save`, must be local: a remote
    // address is refused so a typo cannot point the probe at a public host.
    let endpoints: Vec<model_setup::Endpoint> = match url {
        None => model_setup::default_endpoints(),
        Some(addr) => {
            let local = match model_setup::ensure_local(addr.into()) {
                Err(reason) => anyhow::bail!("{}", reason),
                Ok(value) => value,
            };
            vec![model_setup::Endpoint {
                runtime: "custom".into(),
                url: local,
            }]
        }
    };
    let shared = shared_model::read(shared_path);
    let detection = model_setup::detect(endpoints, shared).await;
    Ok(Outcome {
        json: detect_json(&detection),
        text: detect_text(&detection),
    })
}

/// `model save`: write the choice to the shared file. The local-only rule and the shared-file
/// validation (unknown runtime, blank model, credentials in the url) all run before anything
/// is written, so a refusal leaves the file untouched.
fn save(
    shared_path: &Path,
    runtime: String,
    url: String,
    model: String,
) -> anyhow::Result<Outcome> {
    // Build the report first (it only borrows), then hand the owned strings to `save`.
    let record = save_json(&runtime, &url, &model);
    let text = format!("saved {} ({}) at {}", model, runtime, url);
    match model_setup::save(shared_path, runtime, url, model) {
        Err(reason) => anyhow::bail!("{}", reason),
        Ok(()) => Ok(Outcome { json: record, text }),
    }
}

/// The shape of `model detect --json`, locked by a test because the desktop parses it.
fn detect_json(detection: &model_setup::Detection) -> Value {
    let runtimes: Vec<Value> = detection
        .runtimes
        .iter()
        .map(|found| {
            json!({
                "runtime": found.runtime,
                "url": found.url,
                "reachable": found.reachable,
                "models": found.models,
                "usable": found.usable,
                "error": found.error,
            })
        })
        .collect();
    let shared: Value = match detection.shared.as_ref() {
        None => json!(null),
        Some(block) => json!({
            "runtime": block.runtime,
            "url": block.url,
            "model": block.model,
            "set_by": block.set_by,
            "reachable": block.reachable,
        }),
    };
    json!({"ok": true, "runtimes": runtimes, "shared": shared})
}

/// A short human-readable report for `model detect` without `--json`.
fn detect_text(detection: &model_setup::Detection) -> String {
    let mut lines: Vec<String> = vec![];
    for found in detection.runtimes.iter() {
        if found.reachable {
            let usable = if found.usable.is_empty() {
                "no usable model installed".to_string()
            } else {
                format!("usable: {}", found.usable.join(", "))
            };
            lines.push(format!(
                "{} at {} reachable, {}",
                found.runtime, found.url, usable
            ));
        } else {
            let why = found.error.clone().unwrap_or("unreachable".into());
            lines.push(format!(
                "{} at {} not reachable ({})",
                found.runtime, found.url, why
            ));
        }
    }
    if let Some(block) = detection.shared.as_ref() {
        let state = if block.reachable {
            "answers"
        } else {
            "server off now"
        };
        lines.push(format!(
            "shared model from {}: {} at {} ({})",
            block.set_by, block.model, block.url, state
        ));
    }
    lines.join("\n")
}

/// The success shape of `model save --json`.
fn save_json(runtime: &str, url: &str, model: &str) -> Value {
    json!({"ok": true, "runtime": runtime, "url": url, "model": model})
}

/// `model pull`: Ollama's `POST /api/pull` answer is NDJSON, streamed through the same
/// line buffer the library tests cover. Each line becomes one JSON object on stdout
/// (`{"stage": ...}`); an error line is still just a line, and the command exits 1 once
/// the stream ends if one was seen. A server that refuses the request outright (a 404
/// from LM Studio) is a one-sentence failure, printed as the usual failure object.
async fn pull(url: Option<&str>, model: String) -> anyhow::Result<()> {
    let base: String = match url {
        None => model_setup::OLLAMA_URL.into(),
        Some(addr) => match model_setup::ensure_local(addr.into()) {
            Err(reason) => {
                println!("{}", json!({"ok": false, "error": reason}));
                std::process::exit(1);
            }
            Ok(value) => value,
        },
    };
    // A download uses `download_client` (no total timeout), not the 2 s probe client: a real
    // model pull streams its body for far longer than two seconds (finding 5).
    let client = model_setup::download_client();
    let response = match model_setup::start_pull(client, base, model).await {
        Err(reason) => {
            println!("{}", json!({"ok": false, "error": reason}));
            std::process::exit(1);
        }
        Ok(body) => body,
    };

    // Read the NDJSON body to the end, then decide success from what the stream actually said
    // (a `success` stage, an `error` stage, or a body that failed to read / stopped before
    // finishing), not from the loop merely ending (finding 5).
    let stream = model_setup::consume_pull_stream(response).await;

    let mut out = std::io::stdout().lock();
    for line in &stream.lines {
        let _ = writeln!(out, "{}", line);
        let _ = out.flush();
    }

    // If the stream never reached `success` and never printed its own error line, add one so
    // the desktop sees why: a dropped connection (`read_failed`) or a body that stopped early.
    if !stream.saw_success && !stream.saw_error {
        let reason = if stream.read_failed {
            "the model server closed the download before it finished"
        } else {
            "the download ended before the model finished"
        };
        let _ = writeln!(out, "{}", json!({"stage": "error", "error": reason}));
        let _ = out.flush();
    }

    // Non-zero unless the server confirmed `success`. A read failure, an error line, or a stream
    // that stopped before the `success` stage are all failures (see `pull_stream_failed`).
    if model_setup::pull_stream_failed(stream) {
        std::process::exit(1);
    }
    Ok(())
}

/// The failure shape the app reads when a command could not run (the same as `config`).
fn failure_json(error: &anyhow::Error) -> Value {
    json!({"ok": false, "error": format!("{error:#}")})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cli;
    use crate::cli::Command;
    use clap::Parser;

    /// Parses `args` as if they were typed after the program name.
    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("lets-triage-mail").chain(args.iter().copied()))
    }

    #[test]
    fn detects_json_shape_is_locked() {
        let detection = model_setup::Detection {
            runtimes: vec![model_setup::Found {
                runtime: "ollama".into(),
                url: "http://127.0.0.1:11434".into(),
                reachable: true,
                models: ["qwen3:4b".into(), "nomic-embed-text".into()].to_vec(),
                usable: ["qwen3:4b".into()].to_vec(),
                error: None,
            }],
            shared: None,
        };
        assert_eq!(
            detect_json(&detection),
            json!({
                "ok": true,
                "runtimes": [{
                    "runtime": "ollama",
                    "url": "http://127.0.0.1:11434",
                    "reachable": true,
                    "models": ["qwen3:4b", "nomic-embed-text"],
                    "usable": ["qwen3:4b"],
                    "error": null,
                }],
                "shared": null,
            })
        );
    }

    #[test]
    fn save_json_shape_is_locked() {
        assert_eq!(
            save_json("ollama", "http://127.0.0.1:11434", "qwen3:4b"),
            json!({
                "ok": true,
                "runtime": "ollama",
                "url": "http://127.0.0.1:11434",
                "model": "qwen3:4b",
            })
        );
    }

    #[test]
    fn pull_line_shape_is_locked() {
        let line = model_setup::normalise_pull_line(
            r#"{"status":"downloading x: 1%","total":5,"completed":1}"#.to_string(),
        );
        assert_eq!(
            line,
            json!({"stage": "downloading", "total": 5, "completed": 1})
        );
    }

    #[test]
    fn parses_model_subcommands() {
        let cli = parse(&["model", "detect", "--json"]).unwrap();
        if let Command::Model(ModelCommand::Detect { json, url }) = cli.command {
            assert!(json);
            assert_eq!(url, None);
        } else {
            panic!("expected model detect");
        }
        let cli = parse(&["model", "pull", "qwen3:4b"]).unwrap();
        if let Command::Model(ModelCommand::Pull { model, .. }) = cli.command {
            assert_eq!(model, "qwen3:4b");
        } else {
            panic!("expected model pull");
        }
        let cli = parse(&[
            "model",
            "save",
            "--runtime",
            "ollama",
            "--url",
            "http://127.0.0.1:11434",
            "--model",
            "qwen3:4b",
        ])
        .unwrap();
        if let Command::Model(ModelCommand::Save {
            runtime,
            url,
            model,
            ..
        }) = cli.command
        {
            assert_eq!(runtime, "ollama");
            assert_eq!(url, "http://127.0.0.1:11434");
            assert_eq!(model, "qwen3:4b");
        } else {
            panic!("expected model save");
        }
    }
}
