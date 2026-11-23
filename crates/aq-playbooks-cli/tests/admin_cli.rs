#![cfg(feature = "postgres-tests")]
#[allow(dead_code)]
#[path = "support/pilots.rs"]
mod pilots;
use aq_playbooks_cli::configuration::read_keyring;
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::{DispatchResult, Engine, Storage};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};
struct Fixture {
    dir: PathBuf,
    namespace: String,
    database: String,
}
impl Fixture {
    fn new() -> Self {
        let name = format!(
            "admin_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(&name);
        fs::create_dir(&dir).unwrap();
        Self {
            dir,
            namespace: name,
            database: std::env::var("AQ_PLAYBOOKS_TEST_DATABASE_URL")
                .expect("disposable PostgreSQL required"),
        }
    }
    fn write(&self, name: &str, value: Value) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }
    fn call(&self, args: &[&str], keys: Option<&Path>) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_aq-playbooks"));
        c.args(args)
            .args(["--namespace", &self.namespace])
            .env("AQ_PLAYBOOKS_DATABASE_URL", &self.database);
        if let Some(keys) = keys {
            c.arg("--protection-keys").arg(keys);
        }
        c.output().unwrap()
    }
    fn ok(&self, args: &[&str], keys: Option<&Path>) -> Value {
        let output = self.call(args, keys);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}
impl Fixture {
    fn protected_store(&self, keys: &Path) -> PostgresStorage {
        PostgresStorage::connect_protected(&self.database, read_keyring(keys).unwrap()).unwrap()
    }
}
fn verify_catalog_bounds(f: &Fixture, keys: &Path) {
    // Invalid bounds/absent consent fail before even creating a namespace.
    for args in [
        vec!["inspect-catalog"],
        vec![
            "inspect-catalog",
            "--acquire-idle-namespace",
            "--limit",
            "26",
        ],
        vec![
            "inspect-catalog",
            "--acquire-idle-namespace",
            "--pages",
            "5",
        ],
    ] {
        assert!(!f.call(&args, Some(keys)).status.success());
    }
    let store = f.protected_store(keys);
    assert_eq!(store.incarnation(&f.namespace).unwrap(), None);
    drop(store);
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
#[test]
fn current_catalog_requires_explicit_idle_ownership_and_never_admits_work() {
    let f = Fixture::new();
    let scenario = pilots::scenario("watcher").unwrap();
    let artifact = scenario.artifacts.last().unwrap();
    let a = f.write(
        "catalog-artifact.json",
        serde_json::to_value(artifact).unwrap(),
    );
    let keys = f.write(
        "catalog-keys.json",
        json!({"active_key_id":"a","keys":{"a":"11".repeat(32)}}),
    );
    verify_catalog_bounds(&f, &keys);
    f.ok(&["install", a.to_str().unwrap()], Some(&keys));
    let engine = Engine::new(f.protected_store(&keys), &f.namespace, "actual-live-owner").unwrap();
    assert!(
        !f.call(
            &["inspect-catalog", "--acquire-idle-namespace"],
            Some(&keys)
        )
        .status
        .success()
    );
    let before = engine.snapshot().unwrap();
    engine.release().unwrap();
    drop(engine);
    let page = f.ok(
        &[
            "inspect-catalog",
            "--acquire-idle-namespace",
            "--exact",
            &artifact.digest,
        ],
        Some(&keys),
    );
    assert_eq!(page["api_version"], "playbooks-catalog-1");
    assert_eq!(
        page["items"][0]["record"]["metadata"]["artifact"],
        artifact.digest
    );
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let state = f.protected_store(&keys).load(&f.namespace).unwrap().state;
    assert_eq!(
        serde_json::to_value(state).unwrap(),
        serde_json::to_value(before).unwrap()
    );
}
#[test]
fn protected_admin_lifecycle_control_recovery_rotation_and_paged_inspection() {
    let f = Fixture::new();
    let keys_a = f.write(
        "keys-a.json",
        json!({"active_key_id":"a","keys":{"a":"11".repeat(32)}}),
    );
    let keys_ab = f.write(
        "keys-ab.json",
        json!({"active_key_id":"b","keys":{"a":"11".repeat(32),"b":"22".repeat(32)}}),
    );
    let keys_b = f.write(
        "keys-b.json",
        json!({"active_key_id":"b","keys":{"b":"22".repeat(32)}}),
    );
    let run = start_watcher(&f, &keys_a);
    let operation = recover_operation(&f, &keys_a, run);
    verify_receipt_pages(&f, &operation.run);
    rotate_namespace_keys(&f, &keys_a, &keys_ab, &keys_b);
    retain_unresolved_effects(&f, &keys_b);
    finish_cancelled_run(&f, &keys_b, operation);
}

fn start_watcher(f: &Fixture, keys: &Path) -> String {
    let scenario = pilots::scenario("watcher").unwrap();
    let artifact = scenario.artifacts.last().unwrap();
    let a = f.write("artifact.json", serde_json::to_value(artifact).unwrap());
    let authority = f.write(
        "authority.json",
        serde_json::to_value(&scenario.authority).unwrap(),
    );
    let input = f.write("input.json", scenario.starts[0].input.clone());
    f.ok(&["install", a.to_str().unwrap()], Some(keys));
    f.ok(
        &[
            "activate",
            "watcher",
            &artifact.digest,
            "--authority",
            authority.to_str().unwrap(),
        ],
        Some(keys),
    );
    f.ok(
        &["controls", "watcher", "--calls-enabled", "false"],
        Some(keys),
    );
    let args = [
        "run",
        "watcher",
        "--input",
        input.to_str().unwrap(),
        "--authority",
        authority.to_str().unwrap(),
    ];
    assert!(!f.call(&args, Some(keys)).status.success());
    f.ok(
        &["controls", "watcher", "--calls-enabled", "true"],
        Some(keys),
    );
    let view = f.ok(&args, Some(keys));
    view["runs"][0]["id"].as_str().unwrap().to_string()
}

struct RecoveredOperation {
    run: String,
    intent: String,
    request: PathBuf,
}
fn recover_operation(f: &Fixture, keys: &Path, run: String) -> RecoveredOperation {
    let store = f.protected_store(keys);
    let e = Engine::new(store, &f.namespace, "test-host").unwrap();
    let state = e.snapshot().unwrap();
    let intent = state
        .intents
        .values()
        .find(|i| i.run == run)
        .unwrap()
        .id
        .clone();
    e.record_result(
        &intent,
        DispatchResult::Unknown {
            reason: "private-host-detail".into(),
        },
        1,
    )
    .unwrap();
    e.release().unwrap();
    drop(e);
    let request=f.write("recovery.json",json!({"intent":intent,"actor":"operator","reason":"private-recovery-reason","evidence_refs":["private-evidence-ref"]}));
    let recovery = f.ok(
        &["reconcile", &run, "--request", request.to_str().unwrap()],
        Some(keys),
    );
    assert_eq!(recovery["status"], "recorded");
    RecoveredOperation {
        run,
        intent,
        request,
    }
}

fn verify_receipt_pages(f: &Fixture, run: &str) {
    // Safe metadata inspection requires no key and never loads raw workflow values.
    let first = f.ok(
        &["inspect", run, "--kind", "receipts", "--limit", "1"],
        None,
    );
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    let cursor = first["next"].as_str().unwrap();
    let next = f.ok(
        &[
            "inspect", run, "--kind", "receipts", "--after", cursor, "--limit", "100",
        ],
        None,
    );
    let public = serde_json::to_string(&next).unwrap();
    assert!(!public.contains("private-host-detail"));
    assert!(!public.contains("private-recovery-reason"));
    assert!(!public.contains("private-evidence-ref"));
}

fn rotate_namespace_keys(f: &Fixture, old: &Path, both: &Path, new: &Path) {
    f.ok(&["rotate-keys"], Some(both));
    assert!(f.protected_store(new).load(&f.namespace).is_ok());
    assert!(f.protected_store(old).load(&f.namespace).is_err());
}

fn retain_unresolved_effects(f: &Fixture, keys: &Path) {
    let policy = f.write(
        "retention.json",
        json!({"event_replay_ms":100,"terminal_run_ms":100,"audit_ms":100,"max_tombstones":1000}),
    );
    f.ok(&["retention", policy.to_str().unwrap()], Some(keys));
    // `gc` keeps working and says plainly that it is not needed: it is one
    // more ordinary commit of the bounded work every commit does.
    let help = f.call(&["gc", "--help"], None);
    assert!(String::from_utf8_lossy(&help.stdout).contains("Not needed"));
    let report = f.ok(&["gc", "--limit", "10"], Some(keys));
    assert_eq!(report["runs"], 0); // unresolved effects survive aggressive retention
    assert_eq!(report["admissions"], 0);
    f.ok(
        &[
            "retire-producer",
            "aq.host.bridge@1",
            "1",
            "--actor",
            "operator",
            "--reason",
            "retired delivery epoch",
        ],
        Some(keys),
    );
}

fn finish_cancelled_run(f: &Fixture, keys: &Path, operation: RecoveredOperation) {
    // Cancel waits on the unresolved operation; once the host settles it, a
    // second cancel finishes the run, which leaves the State. The view reads
    // it from run history, and a later command for it is not found.
    let view = f.ok(&["cancel", &operation.run], Some(keys));
    assert_eq!(view["runs"][0]["status"], "cancelling");
    let store = f.protected_store(keys);
    let e = Engine::new(store, &f.namespace, "test-host-settle").unwrap();
    e.record_result(
        &operation.intent,
        DispatchResult::Rejected {
            reason: "private-host-detail".into(),
        },
        2,
    )
    .unwrap();
    e.release().unwrap();
    drop(e);
    let view = f.ok(&["cancel", &operation.run], Some(keys));
    assert_eq!(view["runs"][0]["id"], operation.run.as_str());
    assert_eq!(view["runs"][0]["status"], "cancelled");
    assert!(!view["receipts"].as_array().unwrap().is_empty());
    let state = f.protected_store(keys).load(&f.namespace).unwrap().state;
    assert!(!state.runs.contains_key(&operation.run));
    assert!(
        !f.call(&["cancel", &operation.run], Some(keys))
            .status
            .success()
    );
    // Recovery for its operation, which left with it, says so.
    let refused = f.call(
        &[
            "reconcile",
            &operation.run,
            "--request",
            operation.request.to_str().unwrap(),
        ],
        Some(keys),
    );
    assert!(!refused.status.success());
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("finished with its run"), "{message}");
}
#[test]
fn key_file_permissions_and_invalid_configuration_fail_closed() {
    let f = Fixture::new();
    let bad = f.write(
        "bad.json",
        json!({"active_key_id":"a","keys":{"a":"secret-invalid-key"}}),
    );
    assert!(
        !read_keyring(&bad)
            .err()
            .unwrap()
            .to_string()
            .contains("secret-invalid-key")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let key = f.write(
            "open.json",
            json!({"active_key_id":"a","keys":{"a":"11".repeat(32)}}),
        );
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_keyring(&key).is_err());
    }
}
/// Ruling 3: the clock rebase needs explicit confirmation, offers a dry
/// run that commits nothing, refuses a target too far from the host time,
/// and lowers a pinned clock with an admin receipt in history. A floor set
/// too high can be lowered with its own confirmation.
#[test]
fn rebase_retention_clock_requires_confirmation_and_lowers_a_pinned_clock() {
    let f = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let pinned = now + 10 * 365 * 86_400_000;
    pin_clock(&f, pinned);
    let to = (now + 30_000).to_string();
    let floor = (now - 1_000).to_string();
    let run = |extra: &[&str], target: &str| rebase_command(&f, &floor, target, extra);
    assert!(!run(&[], &to).status.success());
    // The dry run reports and commits nothing.
    let output = run(&["--dry-run"], &to);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let preview: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["report"]["from_ms"], pinned);
    assert_eq!(preview["report"]["to_ms"], now + 30_000);
    assert_eq!(retention_clock(&f).clock_ms, Some(pinned));
    assert!(
        !run(&["--dry-run", "--confirm-clock-rebase"], &to)
            .status
            .success()
    );
    let past = (pinned + 1).to_string();
    let refused = run(&["--confirm-clock-rebase"], &past);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("clock rebase target"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let output = run(&["--confirm-clock-rebase"], &to);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["dry_run"], false);
    assert_eq!(report["report"]["from_ms"], pinned);
    assert_eq!(report["report"]["stale_floor_ms"], now - 1_000);
    let r = retention_clock(&f);
    assert_eq!(r.clock_ms, Some(now + 30_000));
    assert_eq!(r.stale_floor_ms, Some(now - 1_000));
    let receipts = f.ok(&["inspect", "admin", "--kind", "receipts"], None);
    assert!(
        receipts["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "retention_clock_rebased"),
        "{receipts}"
    );
    verify_floor_lowering(&f, now);
}

fn pin_clock(f: &Fixture, pinned: i64) {
    let e = Engine::new(
        PostgresStorage::connect(&f.database).unwrap(),
        &f.namespace,
        "test-host",
    )
    .unwrap();
    // One far-future host time pins the clock.
    e.advance_producer_epoch("producer", 1, "operator", "pin", pinned)
        .unwrap();
    e.release().unwrap();
    drop(e);
}

fn rebase_command(f: &Fixture, floor: &str, to: &str, extra: &[&str]) -> Output {
    let mut args = vec![
        "rebase-retention-clock",
        "--to-ms",
        to,
        "--stale-floor-ms",
        floor,
        "--actor",
        "operator",
        "--reason",
        "host clock jumped",
    ];
    args.extend_from_slice(extra);
    f.call(&args, None)
}

fn retention_clock(f: &Fixture) -> aq_playbooks_runner::retention::RetentionState {
    PostgresStorage::connect(&f.database)
        .unwrap()
        .load(&f.namespace)
        .unwrap()
        .state
        .retention
}

fn verify_floor_lowering(f: &Fixture, now: i64) {
    // Lowering the floor needs its own confirmation.
    let lower = (now - 2_000).to_string();
    let args = [
        "lower-stale-floor",
        "--to-ms",
        &lower,
        "--actor",
        "operator",
        "--reason",
        "floor too high",
    ];
    assert!(!f.call(&args, None).status.success());
    let mut confirmed = args.to_vec();
    confirmed.push("--confirm-floor-lowering");
    let lowered = f.ok(&confirmed, None);
    assert_eq!(lowered["from_ms"], now - 1_000);
    assert_eq!(retention_clock(f).stale_floor_ms, Some(now - 2_000));
}
