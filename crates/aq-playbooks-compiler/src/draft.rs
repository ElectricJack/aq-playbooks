//! A draft workspace: an agent writes a playbook with contract names, and the
//! tool fills in everything the agent cannot compute by hand.
//!
//! A workspace is a directory of five files:
//!
//! | File | Provided by | Content |
//! | --- | --- | --- |
//! | `source.md` | the owner | the document, without any header |
//! | `record.json` | the host | [`SourceRecord`]: id, binding, scope and what the source may use |
//! | `catalog.json` | the host | the [`Registry`] identities come from |
//! | `facts.json` | the host | named fixed facts, captured as definitions |
//! | `draft.json` | the agent | the playbook, with contract names |
//!
//! `draft.json` is a [`Playbook`] with these differences: an invoke's
//! `command`, a wait's `event` and a trigger's `event` are contract names
//! (strings); `id`, `scope` and `required_capabilities` may be left out (the
//! tool writes them from `record.json` and the commands used, and refuses a
//! value that differs); `purpose` defaults to the record's; `limits` defaults
//! to [`Limits::default`]; and `source_map` maps a step name to a line of
//! `source.md`. Any other unknown field is refused where it is written. A
//! draft has at most [`MAX_DRAFT_STEPS`] steps and [`MAX_DRAFT_JSON_BYTES`]
//! bytes.
//!
//! [`fill`] resolves the names against the catalog, restricted to the names
//! the record allows, adds each asynchronous command's reconciliation command
//! and completion event, writes the `---aq-playbook` header, and captures the
//! facts. Identities come only from the catalog: a draft cannot carry a
//! fingerprint, a registry or a header of its own.
//!
//! A sealed artifact is only as trustworthy as the record, catalog, facts and
//! Markdown it was filled from. In an agent's workspace those files are
//! writable by the agent, so a host re-fills from its own copies and takes
//! only the draft from an agent.
mod contracts;
mod operation;
use operation::DraftOperation;
mod json;
use json::{contract_positions, parse};
mod duration;
use duration::duration_faults;
mod fill;
use crate::{
    COMPILER_VERSION, ChangeKind, CompileInput, Diagnostic, Proposal, SemanticDiff, compile,
    semantic_diff, validate,
};
use aq_playbooks_model::*;
pub use fill::fill;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

pub const SOURCE_FILE: &str = "source.md";
pub const RECORD_FILE: &str = "record.json";
pub const CATALOG_FILE: &str = "catalog.json";
pub const EXTENSIONS_FILE: &str = "extensions.json";
pub const FACTS_FILE: &str = "facts.json";
pub const DRAFT_FILE: &str = "draft.json";
const MAX_SOURCE_BYTES: u64 = 4_194_304;
const MAX_RECORD_BYTES: u64 = 65_536;
const MAX_CATALOG_BYTES: u64 = 16_777_216;
const MAX_FACTS_BYTES: u64 = 1_048_576;
const MAX_DRAFT_BYTES: u64 = 4_194_304;
/// A draft may have at most this many steps. Checking a draft costs time and
/// memory that grow faster than its size, and it runs on the host at save.
pub const MAX_DRAFT_STEPS: usize = 256;
/// A draft (the `draft.json` file, or the draft serialized) may be at most
/// this many bytes: room for pretty-printed JSON, and the most at which the
/// worst draft found (a literal of one-field rows, about 2 KB a row to
/// verify) stays inside the 256 MB budget.
pub const MAX_DRAFT_JSON_BYTES: usize = 786_432;

/// What the host keeps about a source; the owner writes only the Markdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRecord {
    /// The source's own id, such as `routing`.
    pub id: String,
    /// The playbook id the artifact is activated as, such as `aq.routing`.
    pub binding: String,
    pub scope: String,
    pub purpose: String,
    /// Command and event names the playbook may use.
    pub commands: BTreeSet<String>,
    pub events: BTreeSet<String>,
    /// Capabilities the commands it uses may require.
    pub capabilities: BTreeSet<String>,
    /// The version every captured fact is recorded with.
    pub facts_version: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub markdown: String,
    pub record: SourceRecord,
    pub catalog: Registry,
    pub facts: BTreeMap<String, Value>,
    pub draft: Value,
}

// `draft.json`: the model's playbook shape with names in the three contract
// positions. Every nested type is the model's own, so it stays as strict.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Draft {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    required_capabilities: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    purpose: Option<String>,
    entries: BTreeMap<String, EntryPoint>,
    steps: BTreeMap<String, DraftStep>,
    triggers: Vec<DraftTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limits: Option<Limits>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    source_map: BTreeMap<String, u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftStep {
    #[serde(with = "DraftOperation")]
    op: Operation,
    edges: BTreeMap<String, String>,
    on_error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftTrigger {
    id: String,
    event: String,
    entry: String,
    input: Expr,
}

fn at(file: &str, path: &str) -> String {
    if path.is_empty() {
        file.into()
    } else {
        format!("{file}:{path}")
    }
}

/// A regular file directly inside `dir`, at most `max` bytes, as UTF-8.
fn read_file(dir: &Path, name: &str, max: u64) -> Result<String, Diagnostic> {
    let fault = |message: String| Diagnostic::error("workspace_file", name, message);
    let path = dir.join(name);
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|e| fault(format!("{name}: cannot be read from the workspace ({e}).")))?;
    if meta.file_type().is_symlink() {
        return Err(fault(format!(
            "{name} is a symbolic link; the workspace accepts only regular files inside it."
        )));
    }
    if !meta.is_file() {
        return Err(fault(format!("{name} is not a regular file.")));
    }
    let inside = std::fs::canonicalize(&path)
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        == std::fs::canonicalize(dir).ok();
    if !inside {
        return Err(fault(format!("{name} is not inside the workspace.")));
    }
    let too_big = || {
        fault(format!(
            "{name} is larger than {max} bytes; keep it under that."
        ))
    };
    if meta.len() > max {
        return Err(too_big());
    }
    let unreadable = |e: std::io::Error| fault(format!("{name}: cannot be read ({e})."));
    let file = std::fs::File::open(&path).map_err(unreadable)?;
    // What was opened must be the file that was checked: a file swapped in
    // between the check and the open is refused before it is read.
    let opened = file.metadata().map_err(unreadable)?;
    check_open_file(name, &meta, &opened)?;
    if opened.len() > max {
        return Err(too_big());
    }
    let mut bytes = vec![];
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > max {
        return Err(too_big());
    }
    String::from_utf8(bytes).map_err(|_| fault(format!("{name} is not UTF-8 text.")))
}

fn check_open_file(
    name: &str,
    _meta: &std::fs::Metadata,
    opened: &std::fs::Metadata,
) -> Result<(), Diagnostic> {
    let fault = |message: String| Diagnostic::error("workspace_file", name, message);
    if !opened.is_file() {
        return Err(fault(format!("{name} is not a regular file.")));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (opened.dev(), opened.ino()) != (_meta.dev(), _meta.ino()) {
            return Err(fault(format!(
                "{name} was replaced while it was being read; write it once and run again."
            )));
        }
        if opened.nlink() != 1 {
            return Err(fault(format!(
                "{name} has other hard links; the workspace accepts only files of its own. Write it as a new file."
            )));
        }
    }
    Ok(())
}

fn keep<T>(result: Result<T, Diagnostic>, faults: &mut Vec<Diagnostic>) -> Option<T> {
    result.map_err(|d| faults.push(d)).ok()
}

/// Read optional extension registrations for a local authoring workspace.
pub fn read_extensions(dir: &Path) -> Result<Vec<CatalogExtension>, Diagnostic> {
    if std::fs::symlink_metadata(dir.join(EXTENSIONS_FILE))
        .is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(vec![]);
    }
    let text = read_file(dir, EXTENSIONS_FILE, MAX_CATALOG_BYTES)?;
    parse(EXTENSIONS_FILE, &text, true, None)
}

impl Workspace {
    /// Reads `source.md`, `record.json`, `catalog.json`, `facts.json` and
    /// `draft.json` from `dir`, reporting every unreadable or malformed file.
    /// An optional `extensions.json` overlays plugin contracts on the catalog.
    ///
    /// This is for the agent's own tools, in the agent's own directory. A
    /// host never calls it on a directory an agent can write: it builds the
    /// [`Workspace`] from its own record, catalog, facts and document, and
    /// takes only `draft.json` from the agent.
    pub fn read(dir: &Path) -> Result<Self, Vec<Diagnostic>> {
        let mut faults = vec![];
        let f = &mut faults;
        let markdown = keep(read_file(dir, SOURCE_FILE, MAX_SOURCE_BYTES), f);
        let record = keep(read_file(dir, RECORD_FILE, MAX_RECORD_BYTES), f)
            .and_then(|t| keep(parse::<SourceRecord>(RECORD_FILE, &t, true, None), f));
        let catalog = keep(read_file(dir, CATALOG_FILE, MAX_CATALOG_BYTES), f)
            .and_then(|t| keep(parse::<Registry>(CATALOG_FILE, &t, true, None), f));
        let extensions = keep(read_extensions(dir), f);
        let catalog = match (catalog, extensions) {
            (Some(catalog), Some(extensions)) if extensions.is_empty() => Some(catalog),
            (Some(catalog), Some(extensions)) => keep(
                catalog.with_extensions(&extensions).map_err(|e| {
                    Diagnostic::error("catalog_extension", EXTENSIONS_FILE, e.to_string())
                }),
                f,
            ),
            _ => None,
        };
        let facts = keep(read_file(dir, FACTS_FILE, MAX_FACTS_BYTES), f).and_then(|t| {
            keep(
                parse::<BTreeMap<String, Value>>(FACTS_FILE, &t, true, None),
                f,
            )
        });
        let draft = keep(read_file(dir, DRAFT_FILE, MAX_DRAFT_BYTES), f).and_then(|t| {
            let limits = draft_limits(0, t.len());
            if !limits.is_empty() {
                f.extend(limits);
                return None;
            }
            let value = keep(parse::<Value>(DRAFT_FILE, &t, true, None), f)?;
            let names = contract_positions(&value);
            if !names.is_empty() {
                f.extend(names);
                return None;
            }
            keep(parse::<Draft>(DRAFT_FILE, &t, true, Some(&value)), f).map(|_| value)
        });
        match (markdown, record, catalog, facts, draft) {
            (Some(markdown), Some(record), Some(catalog), Some(facts), Some(draft)) => Ok(Self {
                markdown,
                record,
                catalog,
                facts,
                draft,
            }),
            _ => Err(faults),
        }
    }
}

/// The complete source: the `---aq-playbook` header the tool writes, then the
/// Markdown unchanged. Returns it with the number of header lines, by which
/// every `source.md` line moves.
pub fn source_text(
    record: &SourceRecord,
    entries: &BTreeSet<String>,
    facts: &BTreeSet<String>,
    markdown: &str,
) -> (String, u32) {
    let header = SourceHeader {
        format_version: FORMAT_VERSION,
        id: record.binding.clone(),
        scope: record.scope.clone(),
        interfaces: entries.clone(),
        definitions: facts.clone(),
    };
    // A header of strings and sets always serializes.
    let json = serde_json::to_string(&header).unwrap_or_default();
    let head = format!("---aq-playbook\n{json}\n---\n");
    let lines = head.matches('\n').count() as u32;
    (head + markdown, lines)
}

/// The catalog restricted to `commands` and `events`, plus each asynchronous
/// command's reconciliation command and completion event, with the
/// capabilities the retained commands require. Refuses a name absent from
/// `catalog`.
pub fn retain_registry(
    catalog: &Registry,
    commands: &BTreeSet<String>,
    events: &BTreeSet<String>,
) -> Result<(Registry, BTreeSet<String>), String> {
    let mut commands = commands.clone();
    let mut events = events.clone();
    for name in commands.clone() {
        let c = catalog
            .commands
            .get(&name)
            .ok_or_else(|| format!("the catalog has no command {name}"))?;
        if let Some(a) = &c.asynchronous {
            commands.insert(a.reconciliation_command.clone());
            events.insert(a.completion_event.clone());
        }
    }
    let mut registry = Registry::default();
    for name in commands {
        let c = catalog
            .commands
            .get(&name)
            .ok_or_else(|| format!("the catalog has no command {name}"))?;
        registry.commands.insert(name, c.clone());
    }
    for name in events {
        let e = catalog
            .events
            .get(&name)
            .ok_or_else(|| format!("the catalog has no event {name}"))?;
        registry.events.insert(name, e.clone());
    }
    let capabilities = registry
        .commands
        .values()
        .flat_map(|c| c.capabilities.iter().cloned())
        .collect();
    Ok((registry, capabilities))
}

fn list(names: &BTreeSet<String>) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// A draft past the step or byte bound, refused before any other work.
fn draft_limits(steps: usize, bytes: usize) -> Vec<Diagnostic> {
    let advice = "Shorten it: one step per decision, with `lookup` and `let` for tables instead of chains of steps. A draft cannot call other playbooks, so if the document needs more than that, ask the owner to split the document.";
    let mut faults = vec![];
    if steps > MAX_DRAFT_STEPS {
        faults.push(Diagnostic::error(
            "draft_limit",
            &at(DRAFT_FILE, "steps"),
            format!(
                "steps: the draft has {steps} steps; a draft may have at most {MAX_DRAFT_STEPS}. {advice}"
            ),
        ));
    }
    if bytes > MAX_DRAFT_JSON_BYTES {
        faults.push(Diagnostic::error(
            "draft_limit",
            DRAFT_FILE,
            format!(
                "{DRAFT_FILE} is {bytes} bytes; a draft may be at most {MAX_DRAFT_JSON_BYTES} bytes. {advice}"
            ),
        ));
    }
    faults
}

/// A validation diagnostic placed in the workspace file it is about.
fn place(mut d: Diagnostic, header_lines: u32) -> Diagnostic {
    d.path = workspace_path(d.path);
    // A line and column are always positions in draft.json. A step mapped
    // to the document names its source.md line in the message instead.
    if let Some(line) = d.line.take() {
        let written = line.saturating_sub(header_lines);
        d.message = format!(
            "{} (the step is mapped to source.md line {written})",
            d.message
        );
        d.column = None;
    }
    d
}

fn workspace_path(path: String) -> String {
    if let Some(rest) = path.strip_prefix("playbook.") {
        at(DRAFT_FILE, rest)
    } else if path == "playbook" {
        DRAFT_FILE.into()
    } else if let Some(name) = path.strip_prefix("definitions.") {
        at(FACTS_FILE, name)
    } else if path.split('.').next() == Some("source") {
        SOURCE_FILE.into()
    } else if ["steps.", "entries.", "triggers.", "source_map."]
        .iter()
        .any(|p| path.starts_with(p))
    {
        at(DRAFT_FILE, &path)
    } else {
        path
    }
}
fn placed(ws: &Workspace, diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let (_, header_lines) = source_text(&ws.record, &BTreeSet::new(), &BTreeSet::new(), "");
    diagnostics
        .into_iter()
        .map(|d| place(d, header_lines))
        .collect()
}

/// Every diagnostic for the workspace's draft: filling, then validation.
/// While the draft does not fill (a name, identity, capability, call or
/// source mapping is wrong), only those faults are reported; validation runs
/// once it fills.
pub fn check(ws: &Workspace) -> Vec<Diagnostic> {
    match fill(ws) {
        Ok(input) => placed(ws, validated(&input)),
        Err(faults) => faults,
    }
}

/// The sealed artifact, or every diagnostic that prevents it.
pub fn seal(ws: &Workspace) -> Result<Artifact, Vec<Diagnostic>> {
    let input = fill(ws)?;
    let faults = validated(&input);
    if !faults.is_empty() {
        return Err(placed(ws, faults));
    }
    compile(&input).map_err(|d| placed(ws, d))
}

/// Validation, plus what only a new draft is held to here: no way through
/// the run may wait as long as the run itself may last.
fn validated(input: &CompileInput) -> Vec<Diagnostic> {
    let mut faults = validate(input);
    if !faults.iter().any(|d| d.code == "deadline") {
        faults.extend(duration_faults(&input.proposal.playbook));
    }
    faults
}

/// The draft an artifact was filled from: names in place of identities, no
/// id, scope or capabilities, default limits left out, and `source.md` lines.
/// Used for worked examples and for the previous playbook.
pub fn unfill(artifact: &Artifact) -> Value {
    let c = &artifact.content;
    let p = &c.playbook;
    let header = &c.source.header;
    let record = SourceRecord {
        id: header.id.clone(),
        binding: header.id.clone(),
        scope: header.scope.clone(),
        purpose: String::new(),
        commands: BTreeSet::new(),
        events: BTreeSet::new(),
        capabilities: BTreeSet::new(),
        facts_version: String::new(),
    };
    let (_, header_lines) = source_text(&record, &header.interfaces, &header.definitions, "");
    let steps = p
        .steps
        .iter()
        .map(|(name, step)| {
            let op = step.op.clone();
            (
                name.clone(),
                DraftStep {
                    op,
                    edges: step.edges.clone(),
                    on_error: step.on_error.clone(),
                },
            )
        })
        .collect();
    let draft = Draft {
        id: None,
        scope: None,
        required_capabilities: None,
        purpose: Some(p.purpose.clone()),
        entries: p.entries.clone(),
        steps,
        triggers: p
            .triggers
            .iter()
            .map(|t| DraftTrigger {
                id: t.id.clone(),
                event: t.event.name.clone(),
                entry: t.entry.clone(),
                input: t.input.clone(),
            })
            .collect(),
        limits: Some(p.limits.clone()).filter(|l| *l != Limits::default()),
        source_map: c
            .source_map
            .iter()
            .filter_map(|(key, loc)| {
                let step = key.strip_prefix("steps.")?;
                let line = loc.line.checked_sub(header_lines).filter(|l| *l > 0)?;
                Some((step.to_string(), line))
            })
            .collect(),
    };
    // The draft is built from model values, which always serialize.
    serde_json::to_value(draft).unwrap_or(Value::Null)
}

/// How a new playbook differs from the previous one, in the owner's terms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    /// Identical, wording only (`Presentation`), behaviour (`Executable`) or interface.
    pub change: ChangeKind,
    /// A capability was added, or a command or event contract changed.
    pub abilities_changed: bool,
    pub diff: SemanticDiff,
}

pub fn compare(previous: &Artifact, next: &Artifact) -> Result<Comparison, ModelError> {
    let diff = semantic_diff(previous, next)?;
    Ok(Comparison {
        change: diff.kind.clone(),
        abilities_changed: !diff.capabilities_added.is_empty()
            || !diff.changed_commands.is_empty()
            || !diff.changed_events.is_empty(),
        diff,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aq-draft-unit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn workspace_files_must_be_regular_bounded_and_inside() {
        let dir = scratch("files");
        let missing = Workspace::read(&dir).unwrap_err();
        assert_eq!(missing.len(), 5, "{missing:#?}");
        assert!(missing.iter().all(|d| d.code == "workspace_file"));
        std::fs::write(dir.join(SOURCE_FILE), "# Doc\n").unwrap();
        std::fs::write(dir.join(RECORD_FILE), "{\"id\": \"x\"}").unwrap();
        std::fs::write(dir.join(CATALOG_FILE), "{\"commands\": {}, \"events\": {}}").unwrap();
        std::fs::write(
            dir.join(FACTS_FILE),
            vec![b' '; MAX_FACTS_BYTES as usize + 1],
        )
        .unwrap();
        std::fs::create_dir(dir.join(DRAFT_FILE)).unwrap();
        let faults = Workspace::read(&dir).unwrap_err();
        let by = |path: &str| faults.iter().find(|d| d.path.starts_with(path)).unwrap();
        assert_eq!(by(RECORD_FILE).code, "record_format");
        assert!(
            by(RECORD_FILE).message.contains("missing field"),
            "{faults:#?}"
        );
        assert!(
            by(FACTS_FILE).message.contains("larger than"),
            "{faults:#?}"
        );
        assert!(
            by(DRAFT_FILE).message.contains("not a regular file"),
            "{faults:#?}"
        );
        #[cfg(unix)]
        {
            let outside = scratch("outside");
            std::fs::write(
                outside.join("catalog.json"),
                "{\"commands\": {}, \"events\": {}}",
            )
            .unwrap();
            std::fs::remove_file(dir.join(CATALOG_FILE)).unwrap();
            std::os::unix::fs::symlink(outside.join("catalog.json"), dir.join(CATALOG_FILE))
                .unwrap();
            let faults = Workspace::read(&dir).unwrap_err();
            let link = faults.iter().find(|d| d.path == CATALOG_FILE).unwrap();
            assert!(link.message.contains("symbolic link"), "{faults:#?}");
            // A hard link to a file elsewhere is refused too.
            std::fs::remove_file(dir.join(CATALOG_FILE)).unwrap();
            std::fs::hard_link(outside.join("catalog.json"), dir.join(CATALOG_FILE)).unwrap();
            let faults = Workspace::read(&dir).unwrap_err();
            let link = faults.iter().find(|d| d.path == CATALOG_FILE).unwrap();
            assert!(link.message.contains("hard links"), "{faults:#?}");
            std::fs::remove_dir_all(outside).unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn validation_paths_are_placed_in_workspace_files() {
        let d = |path: &str, line: Option<u32>| Diagnostic {
            line,
            ..Diagnostic::error("x", path, "m")
        };
        assert_eq!(place(d("steps.a", None), 3).path, "draft.json:steps.a");
        assert_eq!(
            place(d("playbook.limits", None), 3).path,
            "draft.json:limits"
        );
        assert_eq!(place(d("playbook", None), 3).path, "draft.json");
        assert_eq!(
            place(d("definitions.levels", None), 3).path,
            "facts.json:levels"
        );
        assert_eq!(
            place(d("source.header.interfaces", None), 3).path,
            "source.md"
        );
        assert_eq!(place(d("registry", None), 3).path, "registry");
        let mapped = place(
            Diagnostic {
                column: Some(1),
                ..d("steps.a", Some(15))
            },
            3,
        );
        assert_eq!(
            mapped.message,
            "m (the step is mapped to source.md line 12)"
        );
        assert_eq!((mapped.line, mapped.column), (None, None));
    }
}
