//! A draft workspace: an agent writes a playbook with contract names, and the
//! tool fills in everything the agent cannot compute by hand.
//!
//! A workspace is a directory of five files:
//!
//! | File | Written by | Content |
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
//! `source.md`. Any other unknown field is refused where it is written.
//!
//! [`fill`] resolves the names against the catalog, restricted to the names
//! the record allows, adds each asynchronous command's reconciliation command
//! and completion event, writes the `---aq-playbook` header, and captures the
//! facts. Identities come only from the catalog: a draft cannot carry a
//! fingerprint, a registry or a header of its own.
use crate::{
    COMPILER_VERSION, ChangeKind, CompileInput, Diagnostic, Proposal, SemanticDiff, compile,
    semantic_diff, validate,
};
use aq_playbooks_model::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

pub const SOURCE_FILE: &str = "source.md";
pub const RECORD_FILE: &str = "record.json";
pub const CATALOG_FILE: &str = "catalog.json";
pub const FACTS_FILE: &str = "facts.json";
pub const DRAFT_FILE: &str = "draft.json";
const MAX_SOURCE_BYTES: u64 = 4_194_304;
const MAX_RECORD_BYTES: u64 = 65_536;
const MAX_CATALOG_BYTES: u64 = 16_777_216;
const MAX_FACTS_BYTES: u64 = 1_048_576;
const MAX_DRAFT_BYTES: u64 = 4_194_304;

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
    op: DraftOperation,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum DraftOperation {
    Invoke {
        command: String,
        input: Expr,
        bind: String,
        retry: Option<RetryPolicy>,
    },
    Decide {
        predicate: Expr,
    },
    Wait {
        event: Option<String>,
        correlation: BTreeMap<String, Expr>,
        minimum_revision: Option<Expr>,
        deadline_ms: u64,
        bind: String,
    },
    ForEach {
        collection: Expr,
        item: String,
        body: String,
        next: String,
        max_items: usize,
        concurrency: u32,
        on_failure: IterationFailure,
    },
    Call {
        dependency: String,
        entry: String,
        input: Expr,
        mode: CallMode,
        bind: String,
    },
    Join {
        members: Vec<Expr>,
        #[serde(default)]
        member_keys: Vec<String>,
        mode: JoinMode,
        accepted_outcomes: BTreeSet<String>,
        deadline_ms: u64,
        remaining: RemainingPolicy,
        bind: String,
    },
    Return {
        outcome: String,
        output: Expr,
    },
}

fn at(file: &str, path: &str) -> String {
    if path.is_empty() {
        file.into()
    } else {
        format!("{file}:{path}")
    }
}

enum Frame {
    Object(Option<String>),
    Array(usize),
}
fn render(frames: &[Frame]) -> String {
    let mut path = String::new();
    for frame in frames {
        match frame {
            Frame::Object(Some(key)) => {
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(key);
            }
            Frame::Array(index) => path.push_str(&format!("[{index}]")),
            Frame::Object(None) => {}
        }
    }
    path
}
/// The JSON path (`steps.read.op`, `triggers[0]`) of the value being read at
/// byte `offset` of `text`. An error serde reports once a whole object was
/// read (it reads a tagged operation or expression whole before checking it,
/// and reports past the enclosing step) names the object just closed.
fn json_path_at(text: &str, offset: usize) -> String {
    let mut stack: Vec<Frame> = vec![];
    let mut closed: Option<String> = None;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && i < offset {
        match bytes[i] {
            b'{' => stack.push(Frame::Object(None)),
            b'[' => stack.push(Frame::Array(0)),
            b'}' | b']' => {
                closed = Some(render(&stack[..stack.len().saturating_sub(1)]));
                stack.pop();
            }
            b',' => match stack.last_mut() {
                Some(Frame::Object(key)) => *key = None,
                Some(Frame::Array(index)) => *index += 1,
                None => {}
            },
            b'"' => {
                let start = i + 1;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                if let Some(Frame::Object(key @ None)) = stack.last_mut() {
                    let raw = &text[start..i.min(bytes.len())];
                    *key = Some(serde_json::from_str(&format!("\"{raw}\"")).unwrap_or(raw.into()));
                }
            }
            _ => {}
        }
        if bytes
            .get(i)
            .is_some_and(|b| !matches!(b, b'}' | b']' | b',') && !b.is_ascii_whitespace())
        {
            closed = None;
        }
        i += 1;
    }
    closed.unwrap_or_else(|| render(&stack))
}

/// A contract position holding anything but a name: refused where it is
/// written, before serde, which reads an operation whole and so cannot say
/// which of its fields is wrong.
fn contract_positions(draft: &Value) -> Vec<Diagnostic> {
    let mut places = vec![];
    if let Some(steps) = draft.get("steps").and_then(Value::as_object) {
        for (name, step) in steps {
            let op = step.get("op");
            for field in ["command", "event"] {
                if let Some(value) = op.and_then(|op| op.get(field)) {
                    let nullable = field == "event";
                    if !value.is_string() && (!nullable || !value.is_null()) {
                        places.push(format!("steps.{name}.op.{field}"));
                    }
                }
            }
        }
    }
    if let Some(triggers) = draft.get("triggers").and_then(Value::as_array) {
        for (i, t) in triggers.iter().enumerate() {
            if t.get("event").is_some_and(|e| !e.is_string()) {
                places.push(format!("triggers[{i}].event"));
            }
        }
    }
    places
        .into_iter()
        .map(|path| {
            Diagnostic::error(
                "draft_format",
                &at(DRAFT_FILE, &path),
                format!(
                    "{path}: expected a contract name. Write the contract name as a string, such as \"work.read\"; the tool fills in its identity."
                ),
            )
        })
        .collect()
}

/// Strict JSON with the place of the first fault. With `positions`, line and
/// column are those of `text`, the file the agent wrote.
fn parse<T: DeserializeOwned>(file: &str, text: &str, positions: bool) -> Result<T, Diagnostic> {
    serde_json::from_str(text).map_err(|e| {
        let (line, column) = (e.line(), e.column());
        let offset: usize = text
            .split_inclusive('\n')
            .take(line.saturating_sub(1))
            .map(str::len)
            .sum::<usize>()
            + column;
        let path = json_path_at(text, offset);
        let full = e.to_string();
        let reason = full
            .rsplit_once(" at line ")
            .map_or(full.as_str(), |(reason, _)| reason);
        let place = match (positions, path.is_empty()) {
            (true, true) => format!("{file} line {line} column {column}"),
            (true, false) => format!("{file} line {line} column {column} ({path})"),
            (false, true) => file.to_string(),
            (false, false) => format!("{file} {path}"),
        };
        let code = format!("{}_format", file.trim_end_matches(".json"));
        let mut d = Diagnostic::error(&code, &at(file, &path), format!("{place}: {reason}."));
        if positions {
            d.line = u32::try_from(line).ok();
            d.column = u32::try_from(column).ok();
        }
        d
    })
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
    let mut bytes = vec![];
    std::fs::File::open(&path)
        .and_then(|f| f.take(max + 1).read_to_end(&mut bytes))
        .map_err(|e| fault(format!("{name}: cannot be read ({e}).")))?;
    if bytes.len() as u64 > max {
        return Err(too_big());
    }
    String::from_utf8(bytes).map_err(|_| fault(format!("{name} is not UTF-8 text.")))
}

fn keep<T>(result: Result<T, Diagnostic>, faults: &mut Vec<Diagnostic>) -> Option<T> {
    result.map_err(|d| faults.push(d)).ok()
}

impl Workspace {
    /// Reads `source.md`, `record.json`, `catalog.json`, `facts.json` and
    /// `draft.json` from `dir`, reporting every unreadable or malformed file.
    pub fn read(dir: &Path) -> Result<Self, Vec<Diagnostic>> {
        let mut faults = vec![];
        let f = &mut faults;
        let markdown = keep(read_file(dir, SOURCE_FILE, MAX_SOURCE_BYTES), f);
        let record = keep(read_file(dir, RECORD_FILE, MAX_RECORD_BYTES), f)
            .and_then(|t| keep(parse::<SourceRecord>(RECORD_FILE, &t, true), f));
        let catalog = keep(read_file(dir, CATALOG_FILE, MAX_CATALOG_BYTES), f)
            .and_then(|t| keep(parse::<Registry>(CATALOG_FILE, &t, true), f));
        let facts = keep(read_file(dir, FACTS_FILE, MAX_FACTS_BYTES), f)
            .and_then(|t| keep(parse::<BTreeMap<String, Value>>(FACTS_FILE, &t, true), f));
        let draft = keep(read_file(dir, DRAFT_FILE, MAX_DRAFT_BYTES), f).and_then(|t| {
            let value = keep(parse::<Value>(DRAFT_FILE, &t, true), f)?;
            let names = contract_positions(&value);
            if !names.is_empty() {
                f.extend(names);
                return None;
            }
            keep(parse::<Draft>(DRAFT_FILE, &t, true), f).map(|_| value)
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

fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let here = row[j + 1];
            row[j + 1] = (previous + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(here + 1);
            previous = here;
        }
    }
    row[b.len()]
}

/// Refuses a name the source may not use, listing the ones it may, closest first.
fn allowed_name(
    kind: &str,
    name: &str,
    allowed: &BTreeSet<&String>,
    path: &str,
    faults: &mut Vec<Diagnostic>,
) {
    if allowed.contains(&name.to_string()) {
        return;
    }
    let mut names: Vec<&&String> = allowed.iter().collect();
    names.sort_by_key(|n| (distance(name, n), (**n).clone()));
    let choice = if names.is_empty() {
        format!("This source may use no {kind}s.")
    } else {
        format!(
            "Use one of the {kind}s this source may use (closest first): {}.",
            names
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    faults.push(Diagnostic::error(
        &format!("unknown_{kind}"),
        &at(DRAFT_FILE, path),
        format!("{path}: {name:?} is not a {kind} this source may use. {choice}"),
    ));
}

fn list(names: &BTreeSet<String>) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// Builds the compile input: identities from the catalog, the header and
/// definitions from the record and facts.
pub fn fill(ws: &Workspace) -> Result<CompileInput, Vec<Diagnostic>> {
    let names = contract_positions(&ws.draft);
    if !names.is_empty() {
        return Err(names);
    }
    let pretty = serde_json::to_string_pretty(&ws.draft).unwrap_or_default();
    let draft: Draft = parse(DRAFT_FILE, &pretty, false).map_err(|d| vec![d])?;
    let record = &ws.record;
    let mut faults = vec![];
    let commands_allowed: BTreeSet<&String> = record
        .commands
        .iter()
        .filter(|n| ws.catalog.commands.contains_key(*n))
        .collect();
    let events_allowed: BTreeSet<&String> = record
        .events
        .iter()
        .filter(|n| ws.catalog.events.contains_key(*n))
        .collect();
    let mut commands = BTreeSet::new();
    let mut events = BTreeSet::new();
    for (name, step) in &draft.steps {
        match &step.op {
            DraftOperation::Invoke { command, .. } => {
                let path = format!("steps.{name}.op.command");
                allowed_name("command", command, &commands_allowed, &path, &mut faults);
                commands.insert(command.clone());
            }
            DraftOperation::Wait {
                event: Some(event), ..
            } => {
                let path = format!("steps.{name}.op.event");
                allowed_name("event", event, &events_allowed, &path, &mut faults);
                events.insert(event.clone());
            }
            _ => {}
        }
    }
    for (i, t) in draft.triggers.iter().enumerate() {
        let path = format!("triggers[{i}].event");
        allowed_name("event", &t.event, &events_allowed, &path, &mut faults);
        events.insert(t.event.clone());
    }
    if !faults.is_empty() {
        return Err(faults);
    }
    let (registry, capabilities) = retain_registry(&ws.catalog, &commands, &events)
        .map_err(|e| vec![Diagnostic::error("catalog", CATALOG_FILE, format!("{e}."))])?;
    // Each invoked command, with what its asynchronous completion adds, may
    // need only capabilities the record allows.
    for (name, step) in &draft.steps {
        if let DraftOperation::Invoke { command, .. } = &step.op {
            let c = &registry.commands[command];
            let mut needed = c.capabilities.clone();
            if let Some(a) = &c.asynchronous {
                needed.extend(
                    registry.commands[&a.reconciliation_command]
                        .capabilities
                        .clone(),
                );
            }
            let missing: BTreeSet<String> =
                needed.difference(&record.capabilities).cloned().collect();
            if !missing.is_empty() {
                let path = format!("steps.{name}.op.command");
                faults.push(Diagnostic::error(
                    "capability",
                    &at(DRAFT_FILE, &path),
                    format!(
                        "{path}: {command} needs the capabilities {}, which this source may not use (it may use: {}). Use another command.",
                        list(&missing),
                        list(&record.capabilities)
                    ),
                ));
            }
        }
    }
    let identity = |field: &str, found: String, expected: String, from: &str| {
        Diagnostic::error(
            "draft_identity",
            &at(DRAFT_FILE, field),
            format!(
                "{field}: the draft says {found}, but {from} is {expected}. Leave {field} out: the tool writes it."
            ),
        )
    };
    if let Some(id) = draft.id.as_ref().filter(|id| **id != record.binding) {
        faults.push(identity(
            "id",
            format!("{id:?}"),
            format!("{:?}", record.binding),
            "this source's playbook id (record.json binding)",
        ));
    }
    if let Some(scope) = draft.scope.as_ref().filter(|s| **s != record.scope) {
        faults.push(identity(
            "scope",
            format!("{scope:?}"),
            format!("{:?}", record.scope),
            "this source's scope",
        ));
    }
    if let Some(c) = draft
        .required_capabilities
        .as_ref()
        .filter(|c| **c != capabilities)
    {
        faults.push(identity(
            "required_capabilities",
            list(c),
            list(&capabilities),
            "what the commands it uses require",
        ));
    }
    let (source, header_lines) = source_text(
        record,
        &draft.entries.keys().cloned().collect(),
        &ws.facts.keys().cloned().collect(),
        &ws.markdown,
    );
    let markdown_lines = ws.markdown.lines().count();
    let mut source_map = BTreeMap::new();
    for (step, line) in &draft.source_map {
        let path = format!("source_map.{step}");
        if !draft.steps.contains_key(step) {
            faults.push(Diagnostic::error(
                "source_map",
                &at(DRAFT_FILE, &path),
                format!(
                    "{path}: there is no step {step:?}; map a step name (one of: {}) to a line of source.md.",
                    list(&draft.steps.keys().cloned().collect())
                ),
            ));
        } else if *line == 0 || *line as usize > markdown_lines {
            faults.push(Diagnostic::error(
                "source_map",
                &at(DRAFT_FILE, &path),
                format!(
                    "{path}: line {line} is not a line of source.md, which has lines 1 to {markdown_lines}."
                ),
            ));
        } else {
            source_map.insert(
                format!("steps.{step}"),
                SourceLocation {
                    line: line + header_lines,
                    column: 1,
                },
            );
        }
    }
    let mut definitions = BTreeMap::new();
    for (name, value) in &ws.facts {
        match digest(value) {
            Ok(digest) => {
                definitions.insert(
                    name.clone(),
                    CapturedDefinition {
                        version: record.facts_version.clone(),
                        digest,
                        value: value.clone(),
                    },
                );
            }
            Err(e) => faults.push(Diagnostic::error(
                "facts_format",
                &at(FACTS_FILE, name),
                format!("{FACTS_FILE} {name}: {e}."),
            )),
        }
    }
    let mut reference = |name: &str, fingerprint: Result<Hash, ModelError>| match fingerprint {
        Ok(fingerprint) => ContractRef {
            name: name.into(),
            fingerprint,
        },
        Err(e) => {
            faults.push(Diagnostic::error(
                "catalog",
                CATALOG_FILE,
                format!("{CATALOG_FILE}: contract {name} has no identity: {e}."),
            ));
            ContractRef {
                name: name.into(),
                fingerprint: String::new(),
            }
        }
    };
    let mut steps = BTreeMap::new();
    for (name, step) in draft.steps {
        let op = match step.op {
            DraftOperation::Invoke {
                command,
                input,
                bind,
                retry,
            } => Operation::Invoke {
                command: reference(&command, registry.commands[&command].fingerprint()),
                input,
                bind,
                retry,
            },
            DraftOperation::Decide { predicate } => Operation::Decide { predicate },
            DraftOperation::Wait {
                event,
                correlation,
                minimum_revision,
                deadline_ms,
                bind,
            } => Operation::Wait {
                event: event.map(|e| reference(&e, registry.events[&e].fingerprint())),
                correlation,
                minimum_revision,
                deadline_ms,
                bind,
            },
            DraftOperation::ForEach {
                collection,
                item,
                body,
                next,
                max_items,
                concurrency,
                on_failure,
            } => Operation::ForEach {
                collection,
                item,
                body,
                next,
                max_items,
                concurrency,
                on_failure,
            },
            DraftOperation::Call {
                dependency,
                entry,
                input,
                mode,
                bind,
            } => Operation::Call {
                dependency,
                entry,
                input,
                mode,
                bind,
            },
            DraftOperation::Join {
                members,
                member_keys,
                mode,
                accepted_outcomes,
                deadline_ms,
                remaining,
                bind,
            } => Operation::Join {
                members,
                member_keys,
                mode,
                accepted_outcomes,
                deadline_ms,
                remaining,
                bind,
            },
            DraftOperation::Return { outcome, output } => Operation::Return { outcome, output },
        };
        steps.insert(
            name,
            Step {
                op,
                edges: step.edges,
                on_error: step.on_error,
            },
        );
    }
    let triggers = draft
        .triggers
        .into_iter()
        .map(|t| Trigger {
            event: reference(&t.event, registry.events[&t.event].fingerprint()),
            id: t.id,
            entry: t.entry,
            input: t.input,
        })
        .collect();
    if !faults.is_empty() {
        return Err(faults);
    }
    Ok(CompileInput {
        source,
        proposal: Proposal {
            playbook: Playbook {
                id: record.binding.clone(),
                purpose: draft.purpose.unwrap_or_else(|| record.purpose.clone()),
                scope: record.scope.clone(),
                entries: draft.entries,
                steps,
                triggers,
                limits: draft.limits.unwrap_or_default(),
                required_capabilities: capabilities,
            },
            source_map,
        },
        registry,
        dependencies: BTreeMap::new(),
        definitions,
        compiler: COMPILER_VERSION.into(),
    })
}

/// A validation diagnostic placed in the workspace file it is about.
fn place(mut d: Diagnostic, header_lines: u32) -> Diagnostic {
    let path = d.path.clone();
    d.path = if let Some(rest) = path.strip_prefix("playbook.") {
        at(DRAFT_FILE, rest)
    } else if path == "playbook" {
        DRAFT_FILE.into()
    } else if let Some(name) = path.strip_prefix("definitions.") {
        at(FACTS_FILE, name)
    } else if path == "source" || path.starts_with("source.") {
        SOURCE_FILE.into()
    } else if ["steps.", "entries.", "triggers.", "source_map."]
        .iter()
        .any(|p| path.starts_with(p))
    {
        at(DRAFT_FILE, &path)
    } else {
        path
    };
    if let Some(line) = d.line {
        let written = line.saturating_sub(header_lines);
        d.message = format!(
            "{} (the step is mapped to source.md line {written})",
            d.message
        );
    }
    d
}

fn placed(ws: &Workspace, diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let (_, header_lines) = source_text(&ws.record, &BTreeSet::new(), &BTreeSet::new(), "");
    diagnostics
        .into_iter()
        .map(|d| place(d, header_lines))
        .collect()
}

/// Every diagnostic for the workspace's draft: filling, then validation.
pub fn check(ws: &Workspace) -> Vec<Diagnostic> {
    match fill(ws) {
        Ok(input) => placed(ws, validate(&input)),
        Err(faults) => faults,
    }
}

/// The sealed artifact, or every diagnostic that prevents it.
pub fn seal(ws: &Workspace) -> Result<Artifact, Vec<Diagnostic>> {
    let input = fill(ws)?;
    compile(&input).map_err(|d| placed(ws, d))
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
            let op = match step.op.clone() {
                Operation::Invoke {
                    command,
                    input,
                    bind,
                    retry,
                } => DraftOperation::Invoke {
                    command: command.name,
                    input,
                    bind,
                    retry,
                },
                Operation::Decide { predicate } => DraftOperation::Decide { predicate },
                Operation::Wait {
                    event,
                    correlation,
                    minimum_revision,
                    deadline_ms,
                    bind,
                } => DraftOperation::Wait {
                    event: event.map(|e| e.name),
                    correlation,
                    minimum_revision,
                    deadline_ms,
                    bind,
                },
                Operation::ForEach {
                    collection,
                    item,
                    body,
                    next,
                    max_items,
                    concurrency,
                    on_failure,
                } => DraftOperation::ForEach {
                    collection,
                    item,
                    body,
                    next,
                    max_items,
                    concurrency,
                    on_failure,
                },
                Operation::Call {
                    dependency,
                    entry,
                    input,
                    mode,
                    bind,
                } => DraftOperation::Call {
                    dependency,
                    entry,
                    input,
                    mode,
                    bind,
                },
                Operation::Join {
                    members,
                    member_keys,
                    mode,
                    accepted_outcomes,
                    deadline_ms,
                    remaining,
                    bind,
                } => DraftOperation::Join {
                    members,
                    member_keys,
                    mode,
                    accepted_outcomes,
                    deadline_ms,
                    remaining,
                    bind,
                },
                Operation::Return { outcome, output } => DraftOperation::Return { outcome, output },
            };
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

    #[test]
    fn json_paths_name_object_keys_and_array_items() {
        let text = "{\"steps\": {\"a\": {\"op\": 1}}, \"triggers\": [{\"id\": 1}, {\"event\": 2}]}";
        let offset = |needle: &str| text.find(needle).unwrap() + needle.len();
        assert_eq!(json_path_at(text, offset("\"op\"")), "steps.a.op");
        assert_eq!(json_path_at(text, offset("\"event\"")), "triggers[1].event");
        assert_eq!(json_path_at(text, 1), "");
        // Escaped quotes inside keys do not end them early.
        let text = "{\"a\\\"b\": {\"c\": 1}}";
        assert_eq!(json_path_at(text, text.find("1").unwrap()), "a\"b.c");
        // Malformed input never panics.
        for text in ["{\"a\\", "{\"é\\é", "}}]]", "{\"a\": [\"x\\\"] "] {
            json_path_at(text, text.len() + 5);
        }
        let d = parse::<Value>(DRAFT_FILE, "{\"a\": [1, \"é\\", true).unwrap_err();
        assert_eq!(d.code, "draft_format");
    }

    #[test]
    fn names_are_offered_closest_first() {
        assert_eq!(distance("work.reed", "work.read"), 1);
        assert!(distance("work.reed", "work.route") > 1);
        let allowed = ["work.route".to_string(), "work.read".to_string()];
        let mut faults = vec![];
        allowed_name(
            "command",
            "work.reed",
            &allowed.iter().collect(),
            "steps.a.op.command",
            &mut faults,
        );
        assert!(faults[0].message.ends_with("work.read, work.route."));
        let mut faults = vec![];
        allowed_name(
            "event",
            "x",
            &BTreeSet::new(),
            "triggers[0].event",
            &mut faults,
        );
        assert!(
            faults[0]
                .message
                .ends_with("This source may use no events.")
        );
    }

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
        assert_eq!(
            place(d("steps.a", Some(15)), 3).message,
            "m (the step is mapped to source.md line 12)"
        );
    }
}
