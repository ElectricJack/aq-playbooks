use std::{
    fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
fn cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_aq-playbooks"))
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn offline_cli_round_trip_and_invalid_input_exit() {
    let dir = std::env::temp_dir().join(format!(
        "aq-playbooks-cli-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let path = dir.to_str().unwrap();
    let output = cli(&["examples", "--output", path]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // `validate`, `compile` and `diff` took a whole CompileInput; the
    // authoring tools (`check`, `seal`, `compare`, tests/authoring_cli.rs)
    // replace them, and they are refused now.
    let proposal = dir.join("routing.proposal.json");
    for args in [
        &["validate", proposal.to_str().unwrap()][..],
        &["compile", proposal.to_str().unwrap(), "--output", "x.json"],
        &[
            "diff",
            proposal.to_str().unwrap(),
            proposal.to_str().unwrap(),
        ],
    ] {
        assert!(!cli(args).status.success(), "{args:?}");
    }
    let output = cli(&[
        "simulate",
        dir.join("report.scenario.json").to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["complete"], true);
    // The finished runs have left the engine's State; the report's
    // inspection reads them from run history.
    let runs = value["inspection"]["runs"].as_array().unwrap();
    assert!(!runs.is_empty());
    assert!(runs.iter().all(|run| {
        ["completed", "failed", "cancelled"].contains(&run["status"].as_str().unwrap())
    }));
    assert!(
        !value["inspection"]["receipts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn finite_catalog_and_required_explicit_store() {
    let out = cli(&["commands", "list"]);
    assert!(out.status.success());
    let names: Vec<String> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(names.len(), 12);
    assert!(!cli(&["inspect"]).status.success());
    let out = cli(&["simulate", "--pilot", "routing"]);
    assert!(out.status.success());
}
