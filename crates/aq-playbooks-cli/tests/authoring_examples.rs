//! The worked authoring examples in `docs/authoring/examples`, which an
//! authoring agent reads and imitates. Each one is written out by
//! `aq-playbooks examples` and run with the real tools from its root, as an
//! agent runs them: `check` is clean, every case passes under `try`, `seal`
//! writes an artifact that verifies, `compare` reports the kind of change the
//! example shows, and `save` writes the outcome. Each document variant under
//! `stuck/` writes exactly the outcome it shows. So the examples cannot rot.
use aq_playbooks_compiler::docs::example_files;
use aq_playbooks_model::Artifact;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

/// Each example, the change `compare` reports against its
/// `previous.artifact.json`, and the most steps its draft has.
const EXAMPLES: [(&str, &str, usize); 2] = [
    ("escalate-on-judgment", "presentation", 12),
    ("notify-when-ready", "executable", 9),
];
const ON_DISK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/authoring/examples");

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn temp(name: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!(
        "aq-playbooks-examples-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}
fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aq-playbooks"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}
/// Runs the command in `dir`, asserts that it succeeded and returns what it
/// printed.
fn ok(dir: &Path, args: &[&str]) -> Value {
    let out = run(dir, args);
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{} {args:?} failed:\n{printed}\n{}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{args:?} printed no JSON ({e}): {printed}"))
}
/// Writes every example with `aq-playbooks examples` and returns the directory.
fn written(name: &str) -> Dir {
    let dir = temp(name);
    let printed = ok(&dir.0, &["examples", "--output", "."]);
    let names: Vec<&str> = EXAMPLES.iter().map(|(name, _, _)| *name).collect();
    assert_eq!(printed["examples"], serde_json::json!(names));
    dir
}
fn json_file(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap())
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn every_worked_example_checks_saves_and_its_cases_pass() {
    let dir = written("tools");
    for (name, change, max_steps) in EXAMPLES {
        let root = dir.0.join(name);
        let checked = ok(&root, &["check"]);
        assert_eq!(checked["ok"], true, "{name}: {checked:#}");
        assert_eq!(checked["diagnostics"], serde_json::json!([]), "{name}");
        let steps = json_file(&root.join("draft.json"))["steps"]
            .as_object()
            .unwrap()
            .len();
        assert!(steps <= max_steps, "{name} has {steps} steps");

        // Every case passes, and is named after its file.
        let mut cases: Vec<String> = fs::read_dir(root.join("cases"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        cases.sort();
        assert!(cases.len() >= 5, "{name} has {} cases", cases.len());
        for file in &cases {
            let report = ok(&root, &["try", &format!("cases/{file}")]);
            assert_eq!(report["passed"], true, "{name} {file}: {report:#}");
            // A case lists every call the run makes, in order, each with
            // its whole input, and gives no return the run does not use.
            let case = json_file(&root.join("cases").join(file));
            let made: Vec<Value> = report["calls"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| serde_json::json!({"command": c["command"], "input": c["input"]}))
                .collect();
            assert_eq!(
                Value::Array(made.clone()),
                case["expect"]["calls"],
                "{name} {file}"
            );
            for (command, returns) in case["returns"].as_object().unwrap() {
                let used = made.iter().filter(|c| c["command"] == *command).count();
                assert_eq!(
                    used,
                    returns.as_array().unwrap().len(),
                    "{name} {file}: {command}"
                );
            }
            assert_eq!(
                format!("{}.json", report["name"].as_str().unwrap()),
                *file,
                "{name}: a case is named after its file"
            );
        }

        // The sealed artifact verifies, and is the one save records.
        let sealed = ok(&root, &["seal", "--output", "sealed.artifact.json"]);
        let bytes = fs::read(root.join("sealed.artifact.json")).unwrap();
        let artifact = Artifact::from_canonical_bytes(&bytes).unwrap();
        artifact.verify().unwrap();
        assert_eq!(sealed["artifact"], artifact.digest.as_str(), "{name}");

        let compared = ok(&root, &["compare"]);
        assert_eq!(compared["change"], change, "{name}: {compared:#}");
        assert_eq!(compared["abilities_changed"], false, "{name}");
        assert_eq!(compared["diff"]["source_changed"], true, "{name}");

        let saved = ok(&root, &["save"]);
        assert_eq!(saved["saved"], true, "{name}: {saved:#}");
        assert_eq!(saved["artifact"], artifact.digest.as_str(), "{name}");
        let outcome = json_file(&root.join("out/outcome.json"));
        assert_eq!(outcome["kind"], "saved", "{name}");
        assert_eq!(outcome["artifact"], artifact.digest.as_str(), "{name}");
        assert_eq!(outcome["cases"].as_array().unwrap().len(), cases.len());
    }
}

/// `stuck/<name>.json` shows a document the agent cannot turn into steps
/// (`stuck/<name>.md`), the exact command the agent runs, and the outcome it
/// writes. Run on a workspace holding that document, it writes exactly that.
#[test]
fn each_stuck_document_writes_the_outcome_it_shows() {
    let mut shown = BTreeSet::new();
    for (name, _, _) in EXAMPLES {
        let stuck: Vec<&str> = example_files()
            .into_iter()
            .map(|(path, _)| path)
            .filter(|path| path.starts_with(&format!("{name}/stuck/")) && path.ends_with(".json"))
            .collect();
        assert!(!stuck.is_empty(), "{name} shows no stuck document");
        for path in stuck {
            let dir = written("stuck");
            let root = dir.0.join(name);
            let example = json_file(&dir.0.join(path));
            let document = example["document"].as_str().unwrap();
            fs::copy(root.join(document), root.join("source.md")).unwrap();
            let args: Vec<&str> = example["run"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            shown.insert(args[0].to_string());
            let printed = ok(&root, &args);
            assert_eq!(printed["outcome"], example["writes"], "{path}");
            assert_eq!(
                json_file(&root.join("out/outcome.json")),
                example["writes"],
                "{path}"
            );
            assert!(!example["why"].as_str().unwrap().is_empty(), "{path}");
        }
    }
    assert_eq!(shown, BTreeSet::from(["ask".into(), "unable".into()]));
}

/// `example_files()` is every file under `docs/authoring/examples`, byte for
/// byte, and the documents are written as a person writes them: no
/// identifiers, no code.
#[test]
fn example_files_are_the_examples_on_disk() {
    fn walk(root: &Path, dir: &Path, found: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, found);
            } else {
                let relative = path.strip_prefix(root).unwrap().to_str().unwrap();
                found.insert(relative.into(), fs::read_to_string(&path).unwrap());
            }
        }
    }
    let mut on_disk = BTreeMap::new();
    walk(Path::new(ON_DISK), Path::new(ON_DISK), &mut on_disk);
    let listed: BTreeMap<String, String> = example_files()
        .into_iter()
        .map(|(path, text)| (path.to_string(), text.to_string()))
        .collect();
    assert_eq!(
        listed.keys().collect::<Vec<_>>(),
        on_disk.keys().collect::<Vec<_>>()
    );
    assert_eq!(listed, on_disk);
    for (path, text) in &listed {
        if path.ends_with(".md") {
            for code in ["`", "work.", "judgment.", "message.", "aq.", "_"] {
                assert!(!text.contains(code), "{path} contains {code:?}");
            }
        }
    }
}
