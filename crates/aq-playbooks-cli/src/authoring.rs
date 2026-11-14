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
//! (at most 1 MiB), whole, and nothing else. Running one again with the same
//! inputs writes the same bytes. The outcome is data the host checks again,
//! never an instruction.
use anyhow::{Context, Result, anyhow, bail};
use aq_playbooks_compiler::Diagnostic;
use aq_playbooks_compiler::draft::{
    self, CATALOG_FILE, DRAFT_FILE, FACTS_FILE, RECORD_FILE, SOURCE_FILE, SourceRecord, Workspace,
};
use aq_playbooks_model::{Artifact, Registry, canonical_bytes};
use aq_playbooks_simulation::{
    Case, CaseReport, MAX_CASES, MAX_QUESTION_BYTES, MAX_REASON_BYTES, MAX_SENTENCE_BYTES,
    MAX_SENTENCES, MIN_CASES, OUTCOME_FILE, Outcome, Sentence, facts_digest, run_case, run_cases,
};
use clap::Subcommand;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

const CASES_DIR: &str = "cases";
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
    /// Ask the owner a question about the document and end: name each
    /// sentence it is about by its source.md line (--line, 1 to 16 times).
    Ask {
        #[arg(long = "line", required = true)]
        lines: Vec<u32>,
        question: String,
    },
    /// Give up: the document cannot be turned into a playbook because of the
    /// sentence on this source.md line, for this reason.
    Unable {
        #[arg(long)]
        line: u32,
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

fn execute(command: Authoring) -> Result<(bool, Value)> {
    let root = workspace_root()?;
    match command {
        Authoring::Check => Ok(match read_workspace(&root) {
            Ok(ws) => {
                let (stage, diagnostics) = match draft::fill(&ws) {
                    Err(faults) => ("filling", faults),
                    Ok(_) => ("validation", draft::check(&ws)),
                };
                checked(stage, diagnostics)
            }
            Err(refusal) => refusal,
        }),
        Authoring::Seal { output } => {
            let (_, artifact) = match sealed(&root) {
                Ok(sealed) => sealed,
                Err(refusal) => return Ok(refusal),
            };
            if let Some(output) = &output {
                anyhow::ensure!(
                    inside(&root, output)? != root.join(OUTCOME_FILE),
                    "{output}: only save, ask and unable write {OUTCOME_FILE}; write the artifact elsewhere."
                );
                write_inside(&root, output, &canonical_bytes(&artifact)?)?;
            }
            let registry = &artifact.content.registry;
            Ok((
                true,
                json!({
                    "artifact": artifact.digest,
                    "executable_fingerprint": artifact.executable_fingerprint,
                    "required_capabilities": artifact.content.playbook.required_capabilities,
                    "commands": registry.commands.keys().collect::<Vec<_>>(),
                    "events": registry.events.keys().collect::<Vec<_>>(),
                    "written": output,
                }),
            ))
        }
        Authoring::Try { case } => {
            let parsed = read_case(&root, &case)?;
            let (_, artifact) = match sealed(&root) {
                Ok(sealed) => sealed,
                Err(refusal) => return Ok(refusal),
            };
            let report = run_case(&artifact, &parsed)?;
            Ok((report.passed, serde_json::to_value(report)?))
        }
        Authoring::Compare => {
            let (_, next) = match sealed(&root) {
                Ok(sealed) => sealed,
                Err(refusal) => return Ok(refusal),
            };
            if !exists(&root, PREVIOUS_FILE) {
                return Ok((true, json!({"previous": null})));
            }
            let previous: Artifact = parse_json(
                PREVIOUS_FILE,
                &read_inside(&root, PREVIOUS_FILE, MAX_ARTIFACT_BYTES)?,
            )?;
            previous
                .verify()
                .map_err(|e| anyhow!("{PREVIOUS_FILE}: is not a sound artifact: {e}"))?;
            Ok((
                true,
                serde_json::to_value(draft::compare(&previous, &next)?)?,
            ))
        }
        Authoring::Facts => {
            let record: SourceRecord = parse_json(
                RECORD_FILE,
                &read_inside(&root, RECORD_FILE, MAX_RECORD_BYTES)?,
            )?;
            let facts: BTreeMap<String, Value> = parse_json(
                FACTS_FILE,
                &read_inside(&root, FACTS_FILE, MAX_FACTS_BYTES)?,
            )?;
            Ok((
                true,
                json!({
                    "facts_version": record.facts_version,
                    "digest": facts_digest(&facts)?,
                    "facts": facts,
                    "use": "Read a fact in the draft as {\"op\": \"ref\", \"root\": \"definitions\", \"path\": [\"<name>\", …]}.",
                }),
            ))
        }
        Authoring::Save => save(&root),
        Authoring::Ask { lines, question } => {
            anyhow::ensure!(
                (1..=MAX_SENTENCES).contains(&lines.len()),
                "--line is given {} times; a question names 1 to {MAX_SENTENCES} sentences of source.md. Name only the sentences the question is about.",
                lines.len()
            );
            bounded("the question", &question, MAX_QUESTION_BYTES)?;
            let source = read_inside(&root, SOURCE_FILE, MAX_SOURCE_BYTES)?;
            let mut seen = BTreeSet::new();
            let mut sentences = vec![];
            for line in lines {
                anyhow::ensure!(
                    seen.insert(line),
                    "--line {line} is given twice; name each sentence once."
                );
                sentences.push(sentence(&source, line)?);
            }
            finish(
                &root,
                Outcome::Question {
                    question,
                    sentences,
                },
            )
        }
        Authoring::Unable { line, reason } => {
            bounded("the reason", &reason, MAX_REASON_BYTES)?;
            let source = read_inside(&root, SOURCE_FILE, MAX_SOURCE_BYTES)?;
            let Sentence { line, text } = sentence(&source, line)?;
            finish(
                &root,
                Outcome::Unable {
                    reason,
                    line,
                    sentence: text,
                },
            )
        }
    }
}

fn save(root: &Path) -> Result<(bool, Value)> {
    let refused = |reasons: Vec<String>, extra: Value| {
        let mut value = json!({
            "saved": false,
            "reasons": reasons,
            "next": "Nothing was written. Fix each reason, check and try again, then run `aq-playbooks save` again.",
        });
        if let (Some(value), Value::Object(extra)) = (value.as_object_mut(), extra) {
            value.extend(extra);
        }
        Ok((false, value))
    };
    let (ws, artifact) = match sealed(root) {
        Ok(sealed) => sealed,
        Err((_, check)) => {
            return refused(
                vec!["the draft does not check; the faults are under `diagnostics`.".into()],
                check,
            );
        }
    };
    let mut reasons = vec![];
    let needed = &artifact.content.playbook.required_capabilities;
    let missing: Vec<&String> = needed.difference(&ws.record.capabilities).collect();
    if !missing.is_empty() {
        reasons.push(format!(
            "the playbook needs the capabilities {missing:?}, which record.json does not give this source ({:?}). Use only commands whose capabilities it gives.",
            ws.record.capabilities
        ));
    }
    let (files, cases) = match read_cases(root) {
        Ok(read) => read,
        Err(faults) => {
            reasons.extend(faults);
            (vec![], vec![])
        }
    };
    let mut reports: Vec<CaseReport> = vec![];
    if reasons.is_empty() {
        match run_cases(&artifact, &cases) {
            Ok(done) => reports = done,
            Err(e) => reasons.push(format!("{e:#}")),
        }
    }
    for (file, report) in files.iter().zip(&reports) {
        if !report.passed {
            reasons.push(format!(
                "case {:?} ({file}) does not run as it says: {}",
                report.name,
                report.problems.join(" ")
            ));
        }
    }
    if !reasons.is_empty() {
        return refused(reasons, json!({"cases": reports}));
    }
    let outcome = Outcome::Saved {
        draft: ws.draft.clone(),
        cases,
        artifact: artifact.digest.clone(),
        facts_digest: facts_digest(&ws.facts)?,
    };
    let bytes = match outcome.to_bytes() {
        Ok(bytes) => bytes,
        Err(e) => return refused(vec![format!("{e:#}")], json!({})),
    };
    write_inside(root, OUTCOME_FILE, &bytes)?;
    let table: Vec<Value> = reports
        .iter()
        .map(|r| json!({"name": r.name, "situation": r.situation, "outcome": r.outcome}))
        .collect();
    Ok((
        true,
        json!({
            "saved": true,
            "written": OUTCOME_FILE,
            "artifact": artifact.digest,
            "facts_digest": facts_digest(&ws.facts)?,
            "cases": table,
            "next": FINISHED,
        }),
    ))
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
fn workspace_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("the current directory cannot be read")?;
    if is_workspace(&cwd) {
        Ok(cwd)
    } else {
        bail!(
            "{}: this is not an authoring workspace: it has no {RECORD_FILE}. The authoring commands run only in the workspace directory itself (the one holding {SOURCE_FILE}, {RECORD_FILE}, {CATALOG_FILE}, {FACTS_FILE} and {DRAFT_FILE}), not in a subdirectory of it. Change to that directory and run the command again.",
            cwd.display()
        )
    }
}
fn is_workspace(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(RECORD_FILE)).is_ok_and(|m| m.is_file())
}
fn exists(root: &Path, given: &str) -> bool {
    fs::symlink_metadata(root.join(given)).is_ok()
}

/// `given` inside `root`: relative, without `..`, and through no symbolic
/// link at any depth.
fn inside(root: &Path, given: &str) -> Result<PathBuf> {
    let refuse = || {
        anyhow!(
            "{given}: only a path inside the workspace, relative to it, is accepted (such as cases/bugfix.json); absolute paths and `..` are refused."
        )
    };
    let path = Path::new(given);
    if path.is_absolute() {
        return Err(refuse());
    }
    let mut at = root.to_path_buf();
    let mut named = false;
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                at.push(name);
                named = true;
                if fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
                    bail!(
                        "{given}: {} is a symbolic link; the workspace accepts only its own files and directories, not links. Write a regular file instead.",
                        at.strip_prefix(root).unwrap_or(&at).display()
                    );
                }
            }
            Component::CurDir => {}
            _ => return Err(refuse()),
        }
    }
    if named { Ok(at) } else { Err(refuse()) }
}

/// A regular file of the workspace, at most `max` bytes, as UTF-8: checked
/// before it is opened, and the opened file checked again.
fn read_inside(root: &Path, given: &str, max: u64) -> Result<String> {
    let path = inside(root, given)?;
    let checked = fs::symlink_metadata(&path)
        .map_err(|e| anyhow!("{given}: cannot be read from the workspace ({e})."))?;
    anyhow::ensure!(checked.is_file(), "{given} is not a regular file.");
    let file = fs::File::open(&path).map_err(|e| anyhow!("{given}: cannot be read ({e})."))?;
    let opened = file
        .metadata()
        .map_err(|e| anyhow!("{given}: cannot be read ({e})."))?;
    anyhow::ensure!(opened.is_file(), "{given} is not a regular file.");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            (opened.dev(), opened.ino()) == (checked.dev(), checked.ino()),
            "{given} was replaced while it was being read; write it once and run again."
        );
        anyhow::ensure!(
            opened.nlink() == 1,
            "{given} has other hard links; the workspace accepts only files of its own. Write it as a new file."
        );
    }
    let too_big = || anyhow!("{given} is larger than {max} bytes; keep it under that.");
    if opened.len() > max {
        return Err(too_big());
    }
    let mut bytes = vec![];
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(too_big());
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("{given} is not UTF-8 text."))
}

/// Writes `bytes` to `given` inside `root` whole: to a new file beside it,
/// then renamed over it, so a reader never sees half a file and a link at
/// the target is replaced, never followed.
fn write_inside(root: &Path, given: &str, bytes: &[u8]) -> Result<()> {
    let path = inside(root, given)?;
    let parent = path.parent().unwrap_or(root).to_path_buf();
    let mut at = root.to_path_buf();
    for component in parent
        .strip_prefix(root)
        .unwrap_or(Path::new(""))
        .components()
    {
        at.push(component);
        match fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("{given}: {} is not a directory.", at.display()),
            Err(_) => fs::create_dir(&at)
                .map_err(|e| anyhow!("{given}: cannot create {} ({e}).", at.display()))?,
        }
    }
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{given}: names no file."))?
        .to_string_lossy()
        .to_string();
    let temporary = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| anyhow!("{given}: cannot be written ({e})."))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, &path).map_err(|e| {
        let _ = fs::remove_file(&temporary);
        anyhow!("{given}: cannot be written ({e}).")
    })
}

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
/// Every `cases/*.json`, by file name, or every fault found reading them.
fn read_cases(root: &Path) -> std::result::Result<(Vec<String>, Vec<Case>), Vec<String>> {
    let dir = inside(root, CASES_DIR).map_err(|e| vec![format!("{e:#}")])?;
    let mut names = vec![];
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".json") {
                names.push(format!("{CASES_DIR}/{name}"));
            }
        }
    }
    names.sort();
    if !(MIN_CASES..=MAX_CASES).contains(&names.len()) {
        return Err(vec![format!(
            "{CASES_DIR}/ holds {} case files (*.json); a playbook is saved with at least {MIN_CASES} cases and at most {MAX_CASES}. Write a case for each situation the document distinguishes.",
            names.len()
        )]);
    }
    let mut cases = vec![];
    let mut faults = vec![];
    for name in &names {
        match read_case(root, name) {
            Ok(case) => cases.push(case),
            Err(e) => faults.push(format!("{e:#}")),
        }
    }
    if faults.is_empty() {
        Ok((names, cases))
    } else {
        Err(faults)
    }
}

fn bounded(what: &str, text: &str, max: usize) -> Result<()> {
    anyhow::ensure!(
        !text.trim().is_empty(),
        "{what} is empty; write it in the owner's words."
    );
    anyhow::ensure!(
        text.len() <= max,
        "{what} is {} bytes; keep it to {max}.",
        text.len()
    );
    Ok(())
}
/// Line `line` of `source.md`, which must hold a sentence.
fn sentence(source: &str, line: u32) -> Result<Sentence> {
    let lines: Vec<&str> = source.lines().collect();
    let Some(text) = (line as usize).checked_sub(1).and_then(|i| lines.get(i)) else {
        bail!(
            "--line {line}: {SOURCE_FILE} has lines 1 to {}; name the line that holds the sentence.",
            lines.len()
        );
    };
    anyhow::ensure!(
        !text.trim().is_empty(),
        "--line {line}: line {line} of {SOURCE_FILE} is blank; name the line that holds the sentence."
    );
    anyhow::ensure!(
        text.len() <= MAX_SENTENCE_BYTES,
        "--line {line}: line {line} of {SOURCE_FILE} is longer than {MAX_SENTENCE_BYTES} bytes."
    );
    Ok(Sentence {
        line,
        text: (*text).to_string(),
    })
}

/// The catalog `commands` and `events` describe: in a workspace, its
/// `catalog.json` (or `--registry`, under the workspace's path rule); else
/// `./catalog.json` when present. `None` leaves the default to the caller.
pub fn workspace_registry(registry: Option<&Path>) -> Result<Option<Registry>> {
    let cwd = std::env::current_dir().context("the current directory cannot be read")?;
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
    Ok(Some(parse_json(&file, &text)?))
}
