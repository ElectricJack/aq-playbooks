//! Documents shipped to the authoring agent with its workspace: the guide
//! ([`GUIDE`], written into the workspace as `GUIDE.md`), the language
//! reference ([`LANGUAGE`], `LANGUAGE.md`), the notes on the commands and
//! events the source may use ([`catalog`], `CATALOG.md`) and the worked
//! examples ([`example_files`], under `examples/`). `FORMAT.md` is the
//! engine's format specification, for engine developers, and is not shipped.
//! Every command, draft and case the documents show is run by the tests
//! (`tests/docs.rs` here, and `authoring_examples.rs` in the CLI), so they
//! cannot drift from the tools.
//!
//! [`example_files`] holds the worked example workspaces in this crate's
//! `authoring-examples` directory: complete workspaces an agent can read, and
//! run the authoring tools in, to see how a document becomes a playbook.
//! They live inside the crate, so that a copy of `crates/` builds it.
//! `aq-playbooks examples --output DIR` writes them out.
use crate::draft::retain_registry;
use aq_playbooks_model::{
    CatalogExtension, CommandContract, EventContract, Recovery, Registry, Schema,
};
use serde_json::Value;
use std::collections::BTreeSet;

/// How to turn a document into a playbook: the workflow, when to ask or give
/// up, the rules for a draft, the case format and the limits.
pub const GUIDE: &str = include_str!("../../../skills/aq-playbook-author/SKILL.md");
/// Every field, operation, expression, rule, limit and diagnostic code of a
/// draft.
pub const LANGUAGE: &str = include_str!("../LANGUAGE.md");
/// The pinned host catalog the notes describe (catalog 2.2.0).
pub const CATALOG_REGISTRY: &str = include_str!("../../../contracts/aq-host-2.2.0/registry.json");
/// What the host says about each contract of that catalog, beside its
/// registry so that no fingerprint changes.
pub const CATALOG_NOTES: &str = include_str!("../../../contracts/aq-host-2.2.0/notes.json");

/// The pinned catalog.
pub fn catalog_registry() -> Registry {
    serde_json::from_str(CATALOG_REGISTRY).expect("the pinned catalog parses")
}

/// The notes on one contract: `kind` is `"commands"` or `"events"`.
pub fn notes(kind: &str, name: &str) -> Option<Value> {
    notes_in(CATALOG_NOTES, [kind, name])
}
fn notes_in(all: &str, path: [&str; 2]) -> Option<Value> {
    let all: Value = serde_json::from_str(all).ok()?;
    contract_notes(&all, path).cloned()
}

fn contract_notes<'a>(all: &'a Value, [kind, name]: [&str; 2]) -> Option<&'a Value> {
    all.get(kind)?.get(name)
}

/// Catalog 2.3.0 (Epic 0, delivery by playbook).
pub const CATALOG_2_3_REGISTRY: &str =
    include_str!("../../../contracts/aq-host-2.3.0/registry.json");
/// What the host says about each contract of catalog 2.3.0.
pub const CATALOG_2_3_NOTES: &str = include_str!("../../../contracts/aq-host-2.3.0/notes.json");

/// Catalog 2.4.0, the host profile with decision notes and subtree activation.
pub const CATALOG_2_4_REGISTRY: &str =
    include_str!("../../../contracts/aq-host-2.4.0/registry.json");
pub const CATALOG_2_4_NOTES: &str = include_str!("../../../contracts/aq-host-2.4.0/notes.json");

/// Catalog 2.5.0, the delivery lineage with the 2.4.0 changes and the sandbox
/// environment commands.
pub const CATALOG_2_5_REGISTRY: &str =
    include_str!("../../../contracts/aq-host-2.5.0/registry.json");
pub const CATALOG_2_5_NOTES: &str = include_str!("../../../contracts/aq-host-2.5.0/notes.json");

/// Catalog 2.6.0, the delivery lineage of 2.3.0 whose `work.read` adds the
/// task's acceptance criteria and its result's conclusion.
pub const CATALOG_2_6_REGISTRY: &str =
    include_str!("../../../contracts/aq-host-2.6.0/registry.json");
pub const CATALOG_2_6_NOTES: &str = include_str!("../../../contracts/aq-host-2.6.0/notes.json");

/// Catalog 2.7.0: 2.6.0 whose `work.read` conclusion holds up to 4096 bytes.
pub const CATALOG_2_7_REGISTRY: &str =
    include_str!("../../../contracts/aq-host-2.7.0/registry.json");
pub const CATALOG_2_7_NOTES: &str = include_str!("../../../contracts/aq-host-2.7.0/notes.json");

/// Authoring notes for a registration that matches a published host contract.
pub fn published_notes(kind: &str, name: &str, contract: &Value) -> Option<Value> {
    for (registry, notes) in [
        (CATALOG_REGISTRY, CATALOG_NOTES),
        (CATALOG_2_4_REGISTRY, CATALOG_2_4_NOTES),
        (CATALOG_2_5_REGISTRY, CATALOG_2_5_NOTES),
        (CATALOG_2_6_REGISTRY, CATALOG_2_6_NOTES),
        (CATALOG_2_7_REGISTRY, CATALOG_2_7_NOTES),
    ] {
        let registry: Registry =
            serde_json::from_str(registry).expect("the published catalog parses");
        let published = match kind {
            "commands" => registry.commands.get(name).map(serde_json::to_value),
            "events" => registry.events.get(name).map(serde_json::to_value),
            _ => None,
        };
        if published.and_then(Result::ok).as_ref() == Some(contract) {
            return notes_in(notes, [kind, name]);
        }
    }
    None
}

/// [`catalog`] for catalog 2.3.0 and its notes.
pub fn catalog_2_3(
    commands: &BTreeSet<String>,
    events: &BTreeSet<String>,
) -> Result<String, String> {
    let registry = serde_json::from_str(CATALOG_2_3_REGISTRY)
        .map_err(|e| format!("catalog 2.3.0 does not parse: {e}"))?;
    catalog_with_extensions(&registry, CATALOG_2_3_NOTES, commands, events, &[])
}

/// `CATALOG.md` for a source that may use `commands` and `events`: every
/// contract its `catalog.json` holds (the named ones, and the reconciliation
/// command and completion event of each asynchronous command), with what the
/// host says about each input field, outcome and event. Refuses a name the
/// pinned catalog lacks, and notes that do not cover a contract exactly.
pub fn catalog(commands: &BTreeSet<String>, events: &BTreeSet<String>) -> Result<String, String> {
    catalog_with_extensions(&catalog_registry(), CATALOG_NOTES, commands, events, &[])
}

/// Render the selected base contracts and an authoring section for each
/// extension that contributes a selected contract.
pub fn catalog_with_extensions(
    base: &Registry,
    base_notes: &str,
    commands: &BTreeSet<String>,
    events: &BTreeSet<String>,
    extensions: &[CatalogExtension],
) -> Result<String, String> {
    let registry = base
        .with_extensions(extensions)
        .map_err(|e| e.to_string())?;
    let (kept, _) = retain_registry(&registry, commands, events)?;
    let base_notes =
        serde_json::from_str(base_notes).map_err(|e| format!("catalog notes do not parse: {e}"))?;
    let mut out = String::from(
        "# Catalog\n\nThe commands and events this playbook may use, and what the host says about \
         each. `aq-playbooks commands describe <name>` and `aq-playbooks events describe <name>` \
         show the same contracts as JSON, with these notes. Where a meaning says \"Not stated\", \
         the catalog does not say: do not rely on it, and ask the owner when the document \
         depends on it.\n",
    );
    let (commands, events) = render_contracts(&kept, base, &base_notes)?;
    append_section(&mut out, "Commands", commands);
    append_section(&mut out, "Events", events);
    let mut extensions: Vec<_> = extensions.iter().collect();
    extensions.sort_by_key(|extension| extension.name());
    for extension in extensions {
        let (mut section, events) =
            render_contracts(&kept, extension.registry(), extension.notes())?;
        section.push_str(&events);
        append_section(
            &mut out,
            &format!("Extension `{}`", extension.name()),
            section,
        );
    }
    Ok(out)
}

fn render_contracts(
    kept: &Registry,
    source: &Registry,
    notes: &Value,
) -> Result<(String, String), String> {
    let commands = source
        .commands
        .keys()
        .filter_map(|name| kept.commands.get(name))
        .map(|contract| command(contract, notes))
        .collect::<Result<String, _>>()?;
    let events = source
        .events
        .keys()
        .filter_map(|name| kept.events.get(name))
        .map(|contract| event(contract, notes))
        .collect::<Result<String, _>>()?;
    Ok((commands, events))
}

fn append_section(target: &mut String, title: &str, contents: String) {
    if !contents.is_empty() {
        target.push_str(&format!("\n## {title}\n"));
        target.push_str(&contents);
    }
}

fn stated(text: &str) -> &str {
    if text.trim().is_empty() {
        "Not stated."
    } else {
        text
    }
}
fn text_of<'v>(notes: &'v Value, key: &str) -> &'v str {
    notes.get(key).and_then(Value::as_str).unwrap_or("")
}
/// The names a notes map gives, which must be exactly `expected`.
fn covered(
    what: &str,
    notes: &Value,
    key: &str,
    expected: &BTreeSet<String>,
) -> Result<(), String> {
    let given: BTreeSet<String> = notes
        .get(key)
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    if &given == expected {
        Ok(())
    } else {
        Err(format!(
            "the notes on {what} give {key} {given:?}, but the contract has {expected:?}"
        ))
    }
}
fn properties(schema: &Schema) -> BTreeSet<String> {
    match schema {
        Schema::Object { properties, .. } => properties.keys().cloned().collect(),
        _ => BTreeSet::new(),
    }
}
/// A short type: `string ≤ 64`, `one of a, b`, `integer 0..1000`,
/// `{id, revision}`, `array ≤ 8 of …`, `… or null`.
pub fn short_type(schema: &Schema) -> String {
    match schema {
        Schema::Null => "null".into(),
        Schema::Boolean => "boolean".into(),
        Schema::Integer { minimum, maximum } => format!("integer {minimum}..{maximum}"),
        Schema::String { max_length, values } if values.is_empty() => {
            format!("string ≤ {max_length} B")
        }
        Schema::String { values, .. } => format!(
            "one of {}",
            values
                .iter()
                .map(|v| format!("`{v}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Schema::Array { items, max_items } => {
            format!("array ≤ {max_items} of {}", short_type(items))
        }
        Schema::Object {
            properties,
            required,
        } => format!(
            "{{{}}}",
            properties
                .keys()
                .map(|k| if required.contains(k) {
                    k.clone()
                } else {
                    format!("{k}?")
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Schema::Nullable { inner } => format!("{} or null", short_type(inner)),
        Schema::Union { variants } => variants
            .iter()
            .map(short_type)
            .collect::<Vec<_>>()
            .join(" or "),
    }
}
fn paragraphs(notes: &Value) -> String {
    notes
        .get("notes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|p| format!("\n{p}\n"))
        .collect()
}
fn command(c: &CommandContract, all_notes: &Value) -> Result<String, String> {
    let notes = contract_notes(all_notes, ["commands", &c.name])
        .ok_or(format!("no notes on command {}", c.name))?;
    let what = format!("command {}", c.name);
    covered(&what, &notes, "input", &properties(&c.input))?;
    covered(
        &what,
        &notes,
        "outcomes",
        &c.outcomes.keys().cloned().collect(),
    )?;
    let mut out = format!(
        "\n### `{}` {}\n\n{}\n\n",
        c.name,
        c.version,
        stated(text_of(&notes, "purpose"))
    );
    let kind = match (&c.recovery, &c.asynchronous) {
        (Recovery::Observation, _) => "A read: safe to repeat.".to_string(),
        (_, Some(a)) => format!(
            "A change that answers later: its `accepted` edge binds `{{handle}}`, and the answer arrives as `{}`. Read it with a join.",
            a.completion_event
        ),
        (Recovery::Reconcilable, _) => "A change the host reconciles from its own receipt.".into(),
        _ => "A change, made once per call: a repeat of the same call returns the first result."
            .into(),
    };
    out.push_str(&format!(
        "{kind} The host answers within {} ms. Needs: {}.\n\n| Input field | Type | Meaning |\n| --- | --- | --- |\n",
        c.deadline_ms,
        c.capabilities.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ")
    ));
    if let Schema::Object { properties, .. } = &c.input {
        for (field, schema) in properties {
            out.push_str(&format!(
                "| `{field}` | {} | {} |\n",
                short_type(schema),
                stated(notes["input"][field].as_str().unwrap_or(""))
            ));
        }
    }
    out.push_str("\n| Outcome | Returns | When, and what next |\n| --- | --- | --- |\n");
    for (outcome, schema) in &c.outcomes {
        out.push_str(&format!(
            "| `{outcome}` | {} | {} |\n",
            short_type(schema),
            stated(notes["outcomes"][outcome].as_str().unwrap_or(""))
        ));
    }
    out.push_str(&paragraphs(&notes));
    Ok(out)
}
fn event(e: &EventContract, all_notes: &Value) -> Result<String, String> {
    let notes = contract_notes(all_notes, ["events", &e.name])
        .ok_or(format!("no notes on event {}", e.name))?;
    covered(
        &format!("event {}", e.name),
        &notes,
        "payload",
        &properties(&e.payload),
    )?;
    let correlation = if e.correlation_fields.is_empty() {
        "none".to_string()
    } else {
        e.correlation_fields
            .iter()
            .map(|f| format!("`{f}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = format!(
        "\n### `{}` {}\n\n{}\n\n**Sent:** {}\n\nCorrelation fields: {correlation}. Kept {} ms.\n\n| Payload field | Type | Meaning |\n| --- | --- | --- |\n",
        e.name,
        e.version,
        stated(text_of(&notes, "purpose")),
        stated(text_of(&notes, "sent")),
        e.retention_ms
    );
    if let Schema::Object { properties, .. } = &e.payload {
        for (field, schema) in properties {
            out.push_str(&format!(
                "| `{field}` | {} | {} |\n",
                short_type(schema),
                stated(notes["payload"][field].as_str().unwrap_or(""))
            ));
        }
    }
    out.push_str(&paragraphs(&notes));
    Ok(out)
}

macro_rules! example_files {
    ($($path:literal),* $(,)?) => {
        &[$(($path, include_str!(concat!("../authoring-examples/", $path)))),*]
    };
}

const EXAMPLE_FILES: &[(&str, &str)] = example_files![
    "escalate-on-judgment/CATALOG.md",
    "escalate-on-judgment/cases/assumed-route-refused.json",
    "escalate-on-judgment/cases/assumed-route-stale.json",
    "escalate-on-judgment/cases/failed-three-times.json",
    "escalate-on-judgment/cases/failed-twice.json",
    "escalate-on-judgment/cases/high-risk.json",
    "escalate-on-judgment/cases/judgment-fails.json",
    "escalate-on-judgment/cases/judgment-never-answers.json",
    "escalate-on-judgment/cases/judgment-refused-at-once.json",
    "escalate-on-judgment/cases/low-risk.json",
    "escalate-on-judgment/cases/medium-risk-small-and-tested.json",
    "escalate-on-judgment/cases/medium-risk.json",
    "escalate-on-judgment/cases/nothing-offers-the-level.json",
    "escalate-on-judgment/cases/owner-chose.json",
    "escalate-on-judgment/cases/risk-unknown.json",
    "escalate-on-judgment/cases/task-changed.json",
    "escalate-on-judgment/cases/task-gone.json",
    "escalate-on-judgment/cases/unroutable-refused.json",
    "escalate-on-judgment/cases/unroutable-stale.json",
    "escalate-on-judgment/catalog.json",
    "escalate-on-judgment/draft.json",
    "escalate-on-judgment/facts.json",
    "escalate-on-judgment/previous.artifact.json",
    "escalate-on-judgment/record.json",
    "escalate-on-judgment/source.md",
    "escalate-on-judgment/stuck/deep-level.json",
    "escalate-on-judgment/stuck/deep-level.md",
    "notify-when-ready/CATALOG.md",
    "notify-when-ready/cases/changed-and-still-bare.json",
    "notify-when-ready/cases/described-meanwhile.json",
    "notify-when-ready/cases/finished-during-the-wait.json",
    "notify-when-ready/cases/message-refused.json",
    "notify-when-ready/cases/still-no-description.json",
    "notify-when-ready/cases/task-cancelled.json",
    "notify-when-ready/cases/task-finished.json",
    "notify-when-ready/cases/task-gone.json",
    "notify-when-ready/cases/task-not-readable.json",
    "notify-when-ready/catalog.json",
    "notify-when-ready/draft.json",
    "notify-when-ready/facts.json",
    "notify-when-ready/previous.artifact.json",
    "notify-when-ready/record.json",
    "notify-when-ready/source.md",
    "notify-when-ready/stuck/how-long.json",
    "notify-when-ready/stuck/how-long.md",
    "notify-when-ready/stuck/phone-call.json",
    "notify-when-ready/stuck/phone-call.md",
    "notify-when-ready/stuck/urgent.json",
    "notify-when-ready/stuck/urgent.md",
    "route-to-environment/CATALOG.md",
    "route-to-environment/cases/continuation_keeps_environment.json",
    "route-to-environment/cases/default_fits.json",
    "route-to-environment/cases/fewest_extras.json",
    "route-to-environment/cases/no_default.json",
    "route-to-environment/cases/nothing_fits_holds.json",
    "route-to-environment/cases/pinned_profile_wins.json",
    "route-to-environment/cases/route_is_stale.json",
    "route-to-environment/cases/task_gone.json",
    "route-to-environment/catalog.json",
    "route-to-environment/draft.json",
    "route-to-environment/facts.json",
    "route-to-environment/record.json",
    "route-to-environment/source.md",
    "route-to-environment/stuck/fastest.json",
    "route-to-environment/stuck/fastest.md",
];

/// Every file of the worked examples, as `(path, content)`. A path is
/// relative to `authoring-examples`, such as `"notify-when-ready/draft.json"`;
/// its first component names the example.
pub fn example_files() -> Vec<(&'static str, &'static str)> {
    EXAMPLE_FILES.to_vec()
}
