use super::LANGUAGE;
use std::{collections::BTreeSet, fs, path::Path};

/// Codes built with `format!`, which the scan cannot read.
const FORMATTED_CODES: [&str; 7] = [
    "unknown_command",
    "unknown_event",
    "draft_format",
    "record_format",
    "catalog_format",
    "extensions_format",
    "facts_format",
];
fn source_codes(source: &str, codes: &mut BTreeSet<String>) {
    for args in source.split("issue(").skip(1) {
        let Some((first, rest)) = args.split_once(',') else {
            continue;
        };
        if !first.contains(['(', ')', '"']) {
            insert_code(rest, codes);
        }
    }
    for marker in ["Diagnostic::error(", ".require(", ".report("] {
        for args in source.split(marker).skip(1) {
            insert_code(args, codes);
        }
    }
}

fn insert_code(args: &str, codes: &mut BTreeSet<String>) {
    let Some(text) = args.trim_start().strip_prefix('"') else {
        return;
    };
    let code = text.split('"').next().unwrap_or_default();
    let valid = code.bytes().all(|c| c.is_ascii_lowercase() || c == b'_');
    if code.len() > 1 && valid {
        codes.insert(code.to_owned());
    }
}

/// Read both validators so moving a diagnostic to a module preserves its coverage.
fn collect_diagnostic_codes(dir: &Path, codes: &mut BTreeSet<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_diagnostic_codes(&path, codes);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            source_codes(&fs::read_to_string(path).unwrap(), codes);
        }
    }
}

#[test]
fn every_validation_code_has_an_entry() {
    let mut codes = std::collections::BTreeSet::new();
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for src in ["src", "../aq-playbooks-model/src"] {
        collect_diagnostic_codes(&crate_dir.join(src), &mut codes);
    }
    codes.extend(FORMATTED_CODES.map(String::from));
    // The scan finds the codes it must: a code from each source.
    for code in [
        "unhandled_outcome",
        "draft_limit",
        "workspace_file",
        "source_header",
        "call",
    ] {
        assert!(codes.contains(code), "the scan missed {code}");
    }
    let headings: std::collections::BTreeSet<String> = LANGUAGE
        .split("\n## Diagnostic codes\n")
        .nth(1)
        .expect("LANGUAGE.md has a Diagnostic codes section")
        .lines()
        .filter_map(|l| l.strip_prefix("### "))
        .map(String::from)
        .collect();
    let missing: Vec<&String> = codes.difference(&headings).collect();
    assert!(
        missing.is_empty(),
        "codes without an entry in LANGUAGE.md: {missing:?}"
    );
    let unknown: Vec<&String> = headings.difference(&codes).collect();
    assert!(
        unknown.is_empty(),
        "entries for codes no tool reports: {unknown:?}"
    );
}
