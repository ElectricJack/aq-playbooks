# Reconciled AQ Host Catalog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Publish one pinned, versioned twelve-command host schema and exercise it through all four pilots and the stateful fake host.

**Architecture:** Publish the typed Registry schema as JSON with a semantic version, canonical fingerprint, and valid/invalid/replay/compatibility fixtures under `contracts/aq-host/`. The CLI loads that exact artifact. Existing model/runner types remain the serialization and execution implementation; consumers vendor the data into their pinned build, without any dependency on a sibling repository or the CLI fake implementation.

**Tech Stack:** Rust 2024, serde_json, existing model canonical SHA-256, bounded Cargo builds.

**Spec:** D4 task crisp-meadow-36.5; aq-server program plan `docs/superpowers/plans/2026-11-02-v2-full-application-plan.md` Global Constraints; aq-playbooks spec 0001 §§6,11.

## Global Constraints

- Each repository builds and tests alone. No sibling path dependencies, no submodules, no floating branches in release inputs. Rust uses the committed lockfile with `--locked`.
- Wire changes publish schemas and fixtures from the contract owner; production adoption and endpoint validation belong to D5.
- Exactly twelve commands, at most sixteen events, registry at most 262,144 bytes; preserve five context views, nine work kinds and six change variants.
- Do not edit aq-server or docs/specs; document decisions in host integration and D4 evidence.
- Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, and the named workspace test. Record exact results and limits.
- Record evidence in `docs/implementation/D4.md`; squash unpublished implementation history, merge origin/main (supervisor-approved substitute for absent origin/dev), publish only the assigned branch with guarded AQ push.
- No adversarial review is marked on D4. File the out-of-repo pin follow-up with explicit provenance.

### Task 1: Freeze the schema contract and pilot acceptance

**Files:** Modify `crates/aq-playbooks-cli/src/catalog.rs`, `crates/aq-playbooks-cli/tests/host_pilots.rs`, `crates/aq-playbooks-cli/src/pilots.rs`; create `contracts/aq-host/registry.json`, `manifest.json`, `fixtures.json`, `README.md`.

**Interfaces:** `catalog() -> Registry`, `catalog::describe_surface() -> serde_json::Value`; JSON Registry decoded with `serde_json::from_str::<Registry>`, fingerprint `digest(&(version, registry))`.

- [x] Add `pilots_pass_on_reconciled_catalog`: assert queued message admission, async decision, exact event and producer, exact catalog capture in all four pilots; run stateful coding/routing/watcher/report to success with explicit completions.
- [x] Run `CARGO_BUILD_JOBS=2 cargo test --locked --workspace pilots_pass_on_reconciled_catalog`; require a failing assertion against the old catalog.
- [x] Set catalog version `1.0.0`; retain two Observation and nine KeyedIdempotent commands; publication Reconcilable. Use `queued` for message enqueue. Use async accepted handle then decision approved/rejected/expired/cancelled/refused results. Operation readback returns truthful running status or typed original completion.
- [x] Rename synthetic events to namespaced AQ names; use `aq.operation.updated` for work completion, `aq.decision.completed` for decisions, trusted producer `aq.host.bridge@1`. Keep typed subject/revision and operation correlation.
- [x] Export the registry once as authoritative data; load it with `include_str!("../../../contracts/aq-host/registry.json")`. Preserve schema builders used by pilots. Manifest records version, versioned canonical digest and finite vocabularies.
- [x] Add fixture validation that decodes schemas, checks bounds and fingerprint, rejects invalid inputs and old event/message contracts, and validates emitted completions.

```rust
let registry = catalog::catalog();
assert!(registry.commands["message.send"].outcomes.contains_key("queued"));
assert!(registry.commands["decision.request"].asynchronous.is_some());
assert!(registry.events["aq.operation.updated"].trusted_producers.contains("aq.host.bridge@1"));
```

### Task 2: Stateful asynchronous decision semantics

**Files:** Modify `crates/aq-playbooks-cli/src/fake_host.rs` and its tests; update event literals in `crates/aq-playbooks-cli/tests/admin_cli.rs`.

**Interfaces:** `FakeHost::complete_decision(handle: &str, responder: &str, outcome: &str) -> Result<EventEnvelope, String>`; durable decision record keyed by exact operation handle, carrying subject, eligible responders, expiry, actor/scope and immutable completion.

- [x] Add tests for accepted pending decision, restart/replay, unauthorized responder, stale subject, expiry, immutable completion, lost response and operation.get readback.
- [x] Store admitted decisions and receipts atomically. Authenticate responders separately from the payload, require eligible responder and current subject, enforce expiry, emit the exact decision completion schema and correlation. Never fabricate approval from admission.
- [x] Share the catalog event identity across fake work completions, simulation and tests. Make message mocks and pilot edges consume queued.
- [x] Run `CARGO_BUILD_JOBS=2 cargo test --locked -p aq-playbooks-cli`; fix changed-area regressions without weakening existing checks.

```rust
assert!(matches!(host.dispatch(&decision), DispatchResult::Accepted { .. }));
assert!(host.complete_decision("decision", "mallory", "approved").is_err());
let event = host.complete_decision("decision", "alice", "approved").unwrap();
assert_eq!(event.contract.name, "aq.decision.completed");
```

### Task 3: Documentation, validation and publication

**Files:** Modify `docs/aq-host-integration.md`; create `docs/implementation/D4.md`.

- [x] Document every command outcome and recovery rationale; each final event name, producer, payload, subject/revision and correlation; exact revision vendoring instructions and D5 migration requirements.
- [x] File emergent out-of-repo D5 pin/validator adoption follow-up under the program parent; record its ID and exact artifact fingerprint. No production implementation in this task.
- [x] Run named test and full Rust definition-of-done commands above, with heartbeat before quiet runs. Save logs outside tracked source and summarize counts in evidence.
- [ ] Reconcile acceptance/subtasks, record source base and final commit through task comments. Squash local feature history, merge origin/main (supervisor-approved substitute for absent origin/dev), run focused tests after any merge changes, push using `aq git push`, and close with changes/verification/test evidence, then drain-ack.
