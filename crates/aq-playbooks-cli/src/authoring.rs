//! The authoring agent's tools: `check`, `seal`, `try`, `compare`, `facts`,
//! `save`, `ask` and `unable`. They work offline, on the workspace the host
//! wrote, and print JSON for an agent that has no one to ask.
//!
//! Paths. The workspace is the current directory, and only the workspace
//! directory itself: a command run anywhere else, a subdirectory of the
//! workspace included, refuses to run. Every path a command takes is
//! relative to the workspace and stays inside it: absolute paths, `..`,
//! symbolic links (at any depth) and files with other hard links are
//! refused, and a file is checked again after it is opened.
//!
//! Outcome. `save`, `ask` and `unable` each write only `out/outcome.json`
//! (at most `MAX_OUTCOME_BYTES`, 4 MiB), whole, and nothing else. A run
//! ends once: each of them refuses when `out/outcome.json` already exists. The outcome is data the host checks again,
//! never an instruction.
use anyhow::{Context, Result, anyhow, bail};
use aq_playbooks_compiler::Diagnostic;
use aq_playbooks_compiler::draft::{
    self, CATALOG_FILE, DRAFT_FILE, EXTENSIONS_FILE, FACTS_FILE, RECORD_FILE, SOURCE_FILE,
    SourceRecord, Workspace,
};
use aq_playbooks_model::{Artifact, CatalogExtension, Registry, canonical_bytes};
use aq_playbooks_simulation::{
    CASES_TRANSITION_BUDGET, Case, CaseReport, HOST_FILES, MAX_CASES, MAX_OUTCOME_BYTES,
    MAX_QUESTION_BYTES, MAX_REASON_BYTES, MAX_SENTENCES, MIN_CASES, OUTCOME_FILE, Outcome,
    Sentence, facts_digest, quote, run_case, run_cases,
};
use clap::Subcommand;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

const CASES_DIR: &str = "cases";
/// What `seal --output` never writes over: the host's files and the outcome.
const HOST_INPUTS: [&str; 10] = [
    SOURCE_FILE,
    RECORD_FILE,
    CATALOG_FILE,
    EXTENSIONS_FILE,
    FACTS_FILE,
    DRAFT_FILE,
    PREVIOUS_FILE,
    "previous",
    CASES_DIR,
    "out",
];
const PREVIOUS_FILE: &str = "previous.artifact.json";
const MAX_CASE_BYTES: u64 = 65_536;
const MAX_SOURCE_BYTES: u64 = 4_194_304;
const MAX_RECORD_BYTES: u64 = 65_536;
const MAX_FACTS_BYTES: u64 = 1_048_576;
const MAX_CATALOG_BYTES: u64 = 16_777_216;
const MAX_ARTIFACT_BYTES: u64 = 16_777_216;
const ORDER: &str = "Faults come in two stages. First, the faults that keep the tool from filling in the draft (unknown command or event names, identities, capabilities, calls, source_map lines); only those are listed while there are any. Once the draft fills, every validation fault is listed at once.";
const FINISHED: &str = "The run is finished: end your work now. The host checks the outcome again.";

#[derive(Subcommand)]
pub enum Authoring {
    /// Check draft.json against the workspace. Lists the faults that keep the
    /// draft from being filled in first; once it fills, every validation
    /// fault. Exits 1 when there is any.
    Check,
    /// Seal the draft into an artifact and print its digest; with --output,
    /// also write it (a path inside the workspace).
    Seal {
        #[arg(long)]
        output: Option<String>,
    },
    /// Run one case file (such as cases/bugfix.json) against the draft with
    /// a fake host, and print what happened. Exits 1 when the case fails.
    Try { case: String },
    /// Compare the draft with previous.artifact.json, the playbook active
    /// now, in the owner's terms; {"previous": null} when there is none.
    Compare,
    /// Print the fixed facts the host placed in facts.json, with their digest.
    Facts,
    /// Check, seal, run every cases/*.json (5 to 40) and, when all pass,
    /// write out/outcome.json. A refused save writes nothing.
    Save,
    /// Ask the owner a question about the document and end. Name each
    /// sentence it is about by its source.md line (`--line N`) or, when it
    /// spans lines, by its first and last line (`--lines A-B`, at most 8
    /// lines): 1 to 16 sentences, recorded in document order.
    Ask {
        #[arg(long = "line")]
        line: Vec<u32>,
        #[arg(long = "lines")]
        lines: Vec<String>,
        question: String,
    },
    /// Give up: the document cannot be turned into a playbook because of the
    /// sentence on this source.md line (`--line N`) or these lines
    /// (`--lines A-B`), for this reason. For a fault in one of the host's
    /// own files, name that file instead (`--file catalog.json`).
    Unable {
        #[arg(long = "line")]
        line: Option<u32>,
        #[arg(long = "lines")]
        lines: Option<String>,
        #[arg(long = "file")]
        file: Option<String>,
        reason: String,
    },
}

/// Runs an authoring command, prints its JSON, and exits 1 when it refused.
pub fn run(command: Authoring) -> Result<()> {
    let (ok, value) = match execute(command) {
        Ok(result) => result,
        Err(error) => (false, json!({"error": format!("{error:#}")})),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

mod commands;
mod feedback;
mod files;
mod save;
use files::{exists, inside, is_workspace, read_inside, workspace_root, write_inside};

fn execute(command: Authoring) -> Result<(bool, Value)> {
    let root = workspace_root()?;
    match command {
        Authoring::Check => Ok(check(&root)),
        Authoring::Seal { output } => commands::seal(&root, output),
        Authoring::Try { case } => commands::try_case(&root, case),
        Authoring::Compare => commands::compare(&root),
        Authoring::Facts => commands::facts(&root),
        Authoring::Save => save::save(&root),
        Authoring::Ask {
            line,
            lines,
            question,
        } => feedback::ask(&root, line, lines, question),
        Authoring::Unable {
            line,
            lines,
            file,
            reason,
        } => feedback::Citation { line, lines, file }.unable(&root, reason),
    }
}

fn check(root: &Path) -> (bool, Value) {
    match read_workspace(root) {
        Ok(ws) => {
            let (stage, diagnostics) = match draft::fill(&ws) {
                Err(faults) => ("filling", faults),
                Ok(_) => ("validation", draft::check(&ws)),
            };
            checked(stage, diagnostics)
        }
        Err(refusal) => refusal,
    }
}

/// Refuses a second ending: `save`, `ask` and `unable` each end the run, and
/// only the first one counts.
fn not_ended(root: &Path) -> Result<()> {
    if exists(root, OUTCOME_FILE) {
        let kind = read_inside(root, OUTCOME_FILE, MAX_OUTCOME_BYTES as u64)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|v| v["kind"].as_str().map(String::from))
            .unwrap_or_else(|| "an outcome".into());
        bail!(
            "{OUTCOME_FILE} already holds this run's ending ({kind}); a run ends once. {FINISHED}"
        );
    }
    Ok(())
}

/// Writes the outcome and says so.
fn finish(root: &Path, outcome: Outcome) -> Result<(bool, Value)> {
    write_inside(root, OUTCOME_FILE, &outcome.to_bytes()?)?;
    Ok((
        true,
        json!({"written": OUTCOME_FILE, "outcome": outcome, "next": FINISHED}),
    ))
}

fn checked(stage: &str, diagnostics: Vec<Diagnostic>) -> (bool, Value) {
    let ok = diagnostics.is_empty();
    let next = if ok {
        "The draft checks. Try it against each case with `aq-playbooks try cases/<name>.json`, then `aq-playbooks save`."
    } else {
        "Fix each fault where its `path` points (file:JSON path; `line` and `column` are positions in that file) and run `aq-playbooks check` again."
    };
    (
        ok,
        json!({"ok": ok, "stage": stage, "order": ORDER, "diagnostics": diagnostics, "next": next}),
    )
}
fn read_workspace(root: &Path) -> std::result::Result<Workspace, (bool, Value)> {
    Workspace::read(root).map_err(|faults| checked("reading", faults))
}
/// The workspace and its sealed draft, or the check output that refuses it.
fn sealed(root: &Path) -> std::result::Result<(Workspace, Artifact), (bool, Value)> {
    let ws = read_workspace(root)?;
    match draft::fill(&ws) {
        Err(faults) => Err(checked("filling", faults)),
        Ok(_) => match draft::seal(&ws) {
            Ok(artifact) => Ok((ws, artifact)),
            Err(faults) => Err(checked("validation", faults)),
        },
    }
}

/// The current directory, when it is an authoring workspace.
fn parse_json<T: serde::de::DeserializeOwned>(file: &str, text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|e| {
        let full = e.to_string();
        let reason = full
            .rsplit_once(" at line ")
            .map_or(full.as_str(), |(reason, _)| reason);
        anyhow!(
            "{file} line {} column {}: {reason}. Fix it there and run the command again.",
            e.line(),
            e.column()
        )
    })
}
fn read_case(root: &Path, given: &str) -> Result<Case> {
    parse_json::<Case>(given, &read_inside(root, given, MAX_CASE_BYTES)?).map_err(|e| {
        anyhow!(
            "{e} A case is {{\"name\", \"situation\", \"start\", \"returns\", \"expect\"}}; see the case format in GUIDE.md."
        )
    })
}
/// The catalog `commands` and `events` describe: in a workspace, its
/// `catalog.json` (or `--registry`, under the workspace's path rule); else
/// `./catalog.json` when present. Also returns the extensions for their notes.
/// `None` leaves the default to the caller.
pub fn workspace_registry(
    registry: Option<&Path>,
) -> Result<Option<(Registry, Vec<CatalogExtension>)>> {
    let cwd = std::env::current_dir().context("the current directory cannot be read")?;
    if !is_workspace(&cwd) && cwd.ancestors().skip(1).any(is_workspace) {
        // In a subdirectory of a workspace: the same refusal as the tools.
        workspace_root()?;
    }
    let file = match registry {
        Some(path) if is_workspace(&cwd) => path
            .to_str()
            .ok_or_else(|| anyhow!("--registry: the path is not UTF-8"))?
            .to_string(),
        Some(_) => return Ok(None),
        None if is_workspace(&cwd) || exists(&cwd, CATALOG_FILE) => CATALOG_FILE.to_string(),
        None => return Ok(None),
    };
    let text = read_inside(&cwd, &file, MAX_CATALOG_BYTES)?;
    let base: Registry = parse_json(&file, &text)?;
    let extensions =
        draft::read_extensions(&cwd).map_err(|e| anyhow!("{}: {}", e.path, e.message))?;
    let catalog = if extensions.is_empty() {
        base
    } else {
        base.with_extensions(&extensions)?
    };
    Ok(Some((catalog, extensions)))
}
