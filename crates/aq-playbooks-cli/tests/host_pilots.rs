//! Compiled pilot graphs driven by the domain-enforcing fake services, rather
//! than queues of unconditional scripted command successes.
#[allow(dead_code)]
#[path = "support/fake_host.rs"]
mod fake_host;
#[path = "support/fake_host_tests.rs"]
mod fake_host_tests;
#[allow(dead_code)]
#[path = "support/pilots.rs"]
mod pilots;
#[path = "support/pilots_tests.rs"]
mod pilots_tests;

use aq_playbooks_cli::catalog;
use aq_playbooks_compiler::COMPILER_VERSION;
use aq_playbooks_model::{Artifact, canonical_bytes};
use aq_playbooks_runner::{Engine, Error, Intent, IntentStatus, MemoryStorage, Run, RunStatus};
use fake_host::{Episode, FakeHost, Observation, Subject};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

#[path = "support/pilot_driver.rs"]
mod pilot_driver;
use pilot_driver::{Pilot, Replies, settled, settled_items, setup};

#[test]
fn watcher_reconciles_host_attested_observation_and_recovers_atomic_notifications() {
    let mut pilot = setup("watcher");
    let findings = json!([{"key":"stuck-work","evidence":"work-1","expected_revision":0}]);
    pilot
        .host
        .record_evaluation("evaluation-1", "snapshot-1", &findings)
        .unwrap();
    pilot.drive(0, Replies::MANUAL);
    assert_eq!(
        settled(&pilot.engine, &pilot.root).outcome.as_deref(),
        Some("success")
    );
    assert!(pilot.host.episodes["project-health/stuck-work"].active);
    assert_eq!(pilot.host.outbox.len(), 1);
    pilot.host.deliver_notifications(1, false).unwrap();
    pilot.host = pilot.host.restart();
    pilot.host.deliver_notifications(1, true).unwrap();
    assert_eq!(pilot.host.recipient_receipts.len(), 1);
    assert_eq!(pilot.host.episodes.len(), 1);
}

#[test]
fn watcher_partial_positive_scan_keeps_unseen_episode_open() {
    let mut pilot = setup("watcher");
    pilot.host.observations.clear();
    pilot.host.episodes.insert(
        "project-health/unseen".into(),
        Episode {
            revision: 3,
            active: true,
            evidence: "prior-complete-scan".into(),
            last_notification_ms: 0,
        },
    );
    pilot
        .host
        .capture_observation(Observation {
            reference: "snapshot-1".into(),
            partition: "project-health".into(),
            scope: "project".into(),
            visibility: "project".into(),
            view: "health_observations".into(),
            subject: Subject {
                id: "candidate-1".into(),
                revision: 1,
            },
            pages: vec![json!({"partial":true})],
            complete: false,
            observed_at_ms: 0,
            expires_at_ms: 60_000,
            episode_revisions: BTreeMap::new(),
        })
        .unwrap();
    pilot
        .host
        .record_evaluation(
            "evaluation-1",
            "snapshot-1",
            &json!([{"key":"stuck-work","evidence":"work-1","expected_revision":0}]),
        )
        .unwrap();
    pilot.drive(0, Replies::MANUAL);
    assert_eq!(
        settled(&pilot.engine, &pilot.root).outcome.as_deref(),
        Some("success")
    );
    assert!(pilot.host.episodes["project-health/stuck-work"].active);
    let unseen = &pilot.host.episodes["project-health/unseen"];
    assert_eq!((unseen.active, unseen.revision), (true, 3));
}

#[test]
fn coding_admission_and_completed_review_with_changes_cannot_publish() {
    let mut pilot = setup("coding");
    pilot.drive(0, Replies::CHECKS);
    assert_eq!(pilot.host.work.len(), 1);
    assert!(
        pilot.host.candidates.is_empty() && pilot.host.evidence.is_empty(),
        "no candidate or check evidence before implementation"
    );
    assert!(
        !pilot.engine.snapshot().unwrap().runs[&pilot.root]
            .status
            .terminal()
    );
    pilot.complete("implementation", "accepted", 1);
    pilot.drive(1, Replies::CHECKS);
    assert!(pilot.host.work.values().any(|work| work.kind == "review"));
    assert!(pilot.host.candidates.is_empty());
    pilot.complete("review", "changes_requested", 2);
    pilot.drive(2, Replies::CHECKS);
    assert_eq!(
        settled(&pilot.engine, &pilot.root).outcome.as_deref(),
        Some("escalated")
    );
    let (_, intents) = settled_items(&pilot.engine);
    assert!(!intents.is_empty());
    assert!(
        intents
            .iter()
            .all(|intent| intent.command.name != "candidate.publish")
    );
    assert_eq!(pilot.host.subjects["main"], 1);
}

#[test]
fn coding_requires_explicit_accepted_review_and_exact_check_before_independent_followup() {
    let mut pilot = setup("coding");
    pilot.drive(0, Replies::CHECKS);
    pilot.complete("implementation", "accepted", 1);
    pilot.drive(1, Replies::CHECKS);
    pilot.complete("review", "accepted", 2);
    pilot.drive(2, Replies::CHECKS);
    assert_eq!(
        settled(&pilot.engine, &pilot.root).outcome.as_deref(),
        Some("success")
    );
    assert!(!pilot.host.evidence.is_empty());
    assert_eq!(
        (pilot.host.subjects["main"], pilot.host.messages.len()),
        (2, 1)
    );
    let (runs, _) = settled_items(&pilot.engine);
    assert!(runs.iter().any(|run| run.origin.is_some()
        && run.parent.is_none()
        && run.status == RunStatus::Completed));
    assert!(
        pilot
            .host
            .work
            .values()
            .filter(|work| work.kind == "review")
            .all(|work| work.completion.as_ref().unwrap()["disposition"] == "accepted")
    );
}

#[test]
fn coding_real_publication_conflict_observes_new_base_repairs_and_checks_new_candidate() {
    let mut pilot = setup("coding");
    pilot.drive(0, Replies::CHECKS);
    pilot.complete("implementation", "accepted", 1);
    pilot.drive(1, Replies::CHECKS);
    pilot.complete("review", "accepted", 2);
    let mut conflicted = false;
    pilot.drive_with_hook(2, Replies::CHECKS, |engine, host| {
        if !conflicted
            && engine.snapshot().unwrap().intents.values().any(|intent| {
                intent.command.name == "candidate.publish"
                    && intent.status == IntentStatus::Prepared
            })
        {
            // A concurrent domain writer advances the destination after this
            // immutable candidate was prepared and checked against main@1.
            assert!(
                host.candidates
                    .values()
                    .any(|candidate| candidate.base.revision == 1)
            );
            host.subjects.insert("main".into(), 2);
            conflicted = true;
        }
    });
    assert!(conflicted);
    assert_eq!(
        (pilot.host.subjects["main"], pilot.host.candidates.len()),
        (2, 1)
    );
    assert!(
        pilot
            .host
            .work
            .values()
            .any(|work| work.kind == "repair" && work.completion.is_none())
    );
    pilot.complete("repair", "accepted", 3);
    pilot.drive(3, Replies::CHECKS);
    assert_eq!(
        settled(&pilot.engine, &pilot.root).outcome.as_deref(),
        Some("success")
    );
    verify_repaired_candidates(&pilot);
}

#[test]
fn report_deadline_cancels_owned_author_and_late_completion_keeps_one_message() {
    let mut pilot = setup("report");
    pilot.drive(0, Replies::MANUAL);
    assert_eq!(pilot.host.work.len(), 1);
    assert!(pilot.host.messages.is_empty());
    pilot.drive(1001, Replies::MANUAL);
    assert_eq!(pilot.host.messages.len(), 1);
    let author = pilot.host.work.values().next().unwrap().clone();
    assert!(author.cancelled);
    // The fallback finished the root; its cohort, with the cancelled author
    // operation, has left the State.
    assert_eq!(
        settled(&pilot.engine, &pilot.root).outcome.as_deref(),
        Some("success")
    );
    let (_, intents) = settled_items(&pilot.engine);
    assert!(
        intents
            .iter()
            .filter(|intent| intent.command.name == "work.ensure")
            .any(|intent| intent.status == IntentStatus::Cancelled && intent.cancel_requested)
    );
    pilot.complete("supervisor_authoring", "accepted", 2000);
    pilot.drive(2000, Replies::MANUAL);
    assert_eq!(pilot.host.messages.len(), 1);
    // The late completion finds no operation (spec 0003 ruling: a late
    // result for a cancelled operation that left is not found); nothing is
    // revived and history is unchanged.
    let state = pilot.engine.snapshot().unwrap();
    assert!(state.runs.is_empty() && state.intents.is_empty());
    assert_eq!(
        serde_json::to_value(settled_items(&pilot.engine).1).unwrap(),
        serde_json::to_value(&intents).unwrap()
    );
}

#[test]
fn every_pilot_uses_the_same_twelve_captured_host_contracts() {
    let registry = catalog::catalog();
    for name in pilots::NAMES {
        for artifact in pilots::scenario(name).unwrap().artifacts {
            for (command, contract) in &artifact.content.registry.commands {
                assert_eq!(
                    Some(contract),
                    registry.commands.get(command),
                    "pilot-specific contract mutation in {name}: {command}"
                );
            }
            for (event, contract) in &artifact.content.registry.events {
                assert_eq!(
                    Some(contract),
                    registry.events.get(event),
                    "pilot-specific event mutation in {name}: {event}"
                );
            }
        }
    }
    assert_eq!(registry.commands.len(), 12);
}

#[test]
fn pilots_pass_on_reconciled_catalog() {
    let registry = catalog::catalog();
    assert!(
        registry.commands["message.send"]
            .outcomes
            .contains_key("queued")
    );
    let decision = registry.commands["decision.request"]
        .asynchronous
        .as_ref()
        .unwrap();
    assert_eq!(decision.completion_event, "aq.decision.completed");
    assert_eq!(decision.reconciliation_command, "operation.get");
    assert!(
        registry.events["aq.operation.updated"]
            .trusted_producers
            .contains("aq.host.bridge@1")
    );
    every_pilot_uses_the_same_twelve_captured_host_contracts();
    watcher_reconciles_host_attested_observation_and_recovers_atomic_notifications();
    coding_requires_explicit_accepted_review_and_exact_check_before_independent_followup();
    report_deadline_cancels_owned_author_and_late_completion_keeps_one_message();
}

/// A scripted cancel of a start that already finished (and left the
/// engine's State as history) is a no-op, not a failed simulation. (Moved
/// from the CLI's `simulation` module when the simulator became the
/// `aq-playbooks-simulation` crate, which has no pilots.)
#[test]
fn cancelling_a_finished_start_is_a_no_op() {
    use aq_playbooks_cli::simulation::{Action, simulate};
    let mut scenario = pilots::scenario("watcher").unwrap();
    let clock = scenario
        .actions
        .iter()
        .map(|action| match action {
            Action::Tick { at_ms }
            | Action::Event { at_ms, .. }
            | Action::Cancel { at_ms, .. }
            | Action::Recover { at_ms } => *at_ms,
        })
        .max()
        .unwrap_or(0);
    scenario.actions.push(Action::Cancel {
        at_ms: clock + 1,
        start: 0,
    });
    let report = simulate(&scenario).unwrap();
    assert!(report.complete);
    let root = &report.roots[0];
    let runs = report.inspection["runs"].as_array().unwrap();
    assert!(
        runs.iter()
            .any(|run| &run["id"] == root && run["outcome"] == "success")
    );
    scenario.actions.push(Action::Cancel {
        at_ms: clock + 2,
        start: 7,
    });
    assert!(simulate(&scenario).is_err(), "an unknown start still fails");
}

/// The pilots are test support now, and `aq-playbooks examples` writes the
/// worked authoring workspaces instead of them. Their bytes stay pinned: the
/// coding pilot and its repair child seal to the exact artifacts the model's
/// format test keeps (`aq-playbooks-model/tests/fixtures`), once stamped
/// with the compiler version that wrote those (the stamp is part of the
/// content, so a compiler release changes every digest and nothing else).
#[test]
fn the_coding_pilot_keeps_its_pinned_bytes() {
    let artifacts = pilots::scenario("coding").unwrap().artifacts;
    let pinned: [(&str, &[u8]); 2] = [
        (
            "coding",
            include_bytes!("../../aq-playbooks-model/tests/fixtures/coding.artifact.json"),
        ),
        (
            "repair",
            include_bytes!("../../aq-playbooks-model/tests/fixtures/repair.artifact.json"),
        ),
    ];
    let stamp = Artifact::from_canonical_bytes(pinned[0].1)
        .unwrap()
        .content
        .compiler;
    assert!(
        artifacts
            .iter()
            .all(|a| a.content.compiler == COMPILER_VERSION)
    );
    // Seal every artifact again under the pinned stamp. An artifact names
    // its dependencies by digest, so one pass per artifact carries the new
    // digests through the deepest chain.
    let mut digests: BTreeMap<String, String> = BTreeMap::new();
    let mut stamped: Vec<Artifact> = vec![];
    for _ in 0..artifacts.len() {
        stamped = artifacts
            .iter()
            .map(|a| {
                let mut content = a.content.clone();
                content.compiler = stamp.clone();
                let mut text = serde_json::to_string(&content).unwrap();
                for (old, new) in &digests {
                    text = text.replace(old, new);
                }
                Artifact::seal(serde_json::from_str(&text).unwrap()).unwrap()
            })
            .collect();
        digests = artifacts
            .iter()
            .zip(&stamped)
            .map(|(a, s)| (a.digest.clone(), s.digest.clone()))
            .collect();
    }
    for (id, bytes) in pinned {
        let artifact = stamped
            .iter()
            .find(|a| a.content.playbook.id == id)
            .unwrap_or_else(|| panic!("the coding pilot has no {id} artifact"));
        assert_eq!(canonical_bytes(artifact).unwrap(), bytes, "{id}");
    }
}

fn verify_repaired_candidates(pilot: &Pilot) {
    let (_, intents) = settled_items(&pilot.engine);
    assert_eq!(pilot.host.subjects["main"], 3);
    assert_eq!(pilot.host.candidates.len(), 2);
    assert_eq!(
        pilot
            .host
            .observations
            .values()
            .filter(|observation| observation.subject.id == "main")
            .map(|observation| observation.subject.revision)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([1, 2])
    );
    let candidates: BTreeSet<_> = pilot.host.candidates.keys().cloned().collect();
    assert_eq!(
        pilot
            .host
            .evidence
            .values()
            .filter(|evidence| candidates.contains(&evidence.subject.id) && evidence.passed)
            .count(),
        2
    );
    assert_eq!(
        intents
            .iter()
            .filter(|intent| intent.command.name == "candidate.publish")
            .count(),
        2
    );
    assert_eq!(
        pilot
            .host
            .work
            .values()
            .filter(|work| work.kind == "repair")
            .count(),
        1
    );
}
