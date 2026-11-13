# Live-State-Only Engine Record Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A namespace's state record holds only work in progress plus a time-limited repeat-protection window. A finished root cohort leaves the record in the commit that settles it, and storage receives it as insert-once history rows.

**Architecture:** The shared transition code (`Engine::transaction_choice_at`, both the full and the `transition_v1` paths) gains one post-plan step, `retention::finish_commit`. It does four things. (1) It removes cohorts settled by this commit and hands them to storage through a transient, never-serialized outbox `State::settled`. (2) It drops expired window entries, at most 64 per commit, from an expiry-ordered index. (3) It drains pre-existing namespaces in bounded steps. (4) It advances a monotonic retention clock. Every backend persists the outbox's public projections atomically with the commit. Every backend replaces "removal requires a tombstone" fences with "removal requires a history record or an expiry ≤ clock". Inspection pages merge live rows and history rows.

**Tech Stack:** Rust 2024 workspace, `--locked`; synchronous `postgres` 0.19; PostgreSQL 17 (`scripts/test-postgres.sh`, benchmark image `postgres@sha256:266d3c257594e195b309ae9d1805e54c788f1d7b375f56bf6c367aac6c917e7b`); XChaCha20-Poly1305 protected snapshots, unchanged.

**Spec:** `docs/specs/0003-live-state-only.md` (binding). Evidence: `/home/jkern/dev/aq-platform/wt/status-sync-server/.superpowers/sdd/2026-11-12-routing-policy/engine-growth-report.md`. Background: `docs/specs/0001-aq-playbooks.md` §14, `docs/implementation/D10.md`.

## Spec check

This builder check tests the spec against the code at `a6a8d32`. Each item gives a resolution. Items marked **NEEDS RULING** change something the owner approved. The plan implements the recommended option.

**NR-1 (NEEDS RULING): dropping correlation fences changes wait semantics.**
- Today a `Wait` whose correlation key had an event GC'd is refused at registration (`lib.rs:1852-1860`, reading `retention.expired_correlations`). This implements 0001 §14 ("admission refuses a wait whose required history cannot be retained or established").
- With no fences, that wait is accepted and later takes its `timeout` edge. This touches the Non-goal "changing playbook semantics".
- A wait matches any retained event with `accepted_at_ms <= deadline` and has no lower bound (`lib.rs:2298-2309`). So no time-limited fence can refuse every late wait. Refusing every late wait needs memory that never expires.
- Options:
  - (a) Accept: the wait times out. Operation completions stay protected (NR-1 does not touch 0001 §14's "operation completion" clause; see SC-9).
  - (b) Keep fences as window entries for `contract.retention_ms` after their event drops. This refuses only inside that period and costs one more entry per event.
- **Recommend (a).** Task 6 implements (a). Option (b) would add a `"c:"` window member in Task 6.

**NR-2 (NEEDS RULING): Acceptance 2's last clause cannot hold as written.**
- The 20 runs that complete after the clock passes the window leave their own window entries: one keyed admission receipt plus one event, about 1.9 KB each. That adds about 38 KB. The protected record at run 1 is about 9.4 KB (report, config e, run 1). So the result is about 5×, not 1.2×.
- Proposed reading, which keeps the intent "a burst leaves nothing behind":
  1. Run the 1,000-run burst at a frozen clock.
  2. Move the clock past the window and complete 20 runs at that frozen clock. They drain the burst at 64 entries per commit.
  3. Move the clock past the window once more and complete 1 run.
  4. Compare that record with run 1. Both then hold one run's window entries.
- **Recommend this reading.** Task 12 implements it and also reports the literal figure.

**SC-1. Producer time.**
- Events carry `occurred_at_ms`/`observed_at_ms` (`aq-playbooks-model/src/lib.rs:436-437`). This is business time and part of the event digest. aq-server's calendar trigger sets it to the due date, which can be days old (`aq-server/src/playbooks/triggers.rs:179`). It cannot serve as request time.
- `KeyedAdmissionRequest` has no time field and is `deny_unknown_fields` (`admission.rs:5-15`).
- Smallest additive change: an `Option<i64>` request-time argument on two new methods, `Engine::admit_event_at` and `Engine::admit_once_at`. It is not part of any digest. The old methods pass `None`.
- This needs no model-crate change, no digest change and no struct-literal breakage.
- Carriers are `aq-playbook-host` `Request::Event` and `Request::AdmitOnce` (follow-on). The CLI has no event or keyed-admission command.
- Boundary: a request is stale iff `requested_at_ms < max(now, clock) - window`.

**SC-2. Readers of a settled run from outside its cohort**, and how each fence changes:
- **Independent starts** are admitted with `root = r.root` (`lib.rs:2024-2027`), so they are inside the cohort.
- **Cross-root joins** do not exist. Join members must be owned children or owned operation handles (`lib.rs:2074-2091`).
- **Event `Destination.run`** names the run, but every reader only tests `is_none()` (`lib.rs:812`, `retention.rs:288`, aq-playbook-host `EventStatus`).
- **Keyed admission receipts** hold `run_id`, and only `admission_receipt` reads them.
- **Late results** (`lib.rs:2441-2462`): see SC-3.
- **`verify_and_record`** (`aq-playbooks-postgres/src/retention.rs:35-187`):
  - Run and intent removal is allowed iff the run appears in `next.settled` and the existing obligation checks hold.
  - Event removal is allowed iff the event's expiry ≤ `next.retention.clock_ms`.
  - Event-tombstone removal no longer needs epoch retirement.
  - No `disposal_tombstones` rows are written for these removals.
  - The full per-commit tombstone scan (`:81-88`) becomes a lookup of the identities created in this commit.
  - The `max_tombstones` count check (`:174-185`) is removed.
  - Until Task 10, removals that carry a legacy `disposals`/tombstone keep today's path.
- **`verify_receipt_retention`** (`admission.rs:185-197`): a changed receipt is still refused. A removed receipt is allowed iff its admission expiry ≤ `next.retention.clock_ms`.
- **`verify_artifact_removals`**: unchanged. `artifact_referenced` now scans live runs only, which is what spec §3 asks for.
- All of these move into `retention::verify_removals` (Task 4), which every backend calls.

**SC-3. Cancelled operations.**
- The spec's example of "a cancelled operation whose late result has not arrived" being unsettled is wrong about the code. `IntentStatus::Cancelled` is settled in `root_effects` (`indexes.rs:732-745`) and in `gc` (`retention.rs:173-177`).
- A terminal run never has unsettled own intents, because `finish` runs only after `settled()` (`lib.rs:2202-2213`).
- Resolution: keep the code's definition. A late result for a removed intent returns `Error::NotFound`. This is today's behaviour for a collected run, now immediate. `late_result` receipts are therefore not recorded after removal.

**SC-4. `runtime_index` / `inspection_rows` and history.**
- `inspection_rows` holds exactly the public projection rows (kind `runs`/`effects`/`receipts`, `item_key`, `run_id`, `sequence`, `public_value`; `migrations/0002:46-54`). It is upserted and deleted on every commit (`indexes.rs:207-213`). `runtime_index` is scheduling-only and has no history role.
- History reuses the same projection functions (`inspection::public_run`/`public_intent`/`public_receipt`) and the same column shape. It lives in a separate insert-only table, `aq_playbooks.run_history`, because:
  - `inspection_rows` rows are rewritten;
  - format 3 has no `inspection_rows`;
  - receipt pages need per-receipt keys.
- History stores one row per inspection item (run, effect or receipt), not one row per run. This deviates from spec §2's wording, but the table shape is the builder's choice. One row per run would make global receipt paging O(history).
- Format 2 inspection runs as a SQL `UNION ALL` ordered by `item_key`.

**SC-5. Expiry order inside the State.** This works without breaking decoding.
- New fields, all `#[serde(default)]`:
  - `RetentionState.expiry: LazyMap<i64, BTreeSet<String>>` (members `"e:<event key>"`, `"k:<admission key>"`);
  - `RetentionState.admission_expires_at_ms: LazyMap<String, i64>`;
  - `RetentionState.clock_ms: Option<i64>`;
  - `RetentionState.drain: DrainState`.
- `RetentionPolicy.admission_replay_ms` gets `#[serde(default = ...)]`.
- Format 3: two new `lazy_fields` entries. The schema is derived from `lazy_fields`, and old namespaces simply have no records for them. `LazyMap<i64, _>` already works there (`indexes.due`). Oldest-first is `expiry.next_key(None)`, one bounded record read.

**SC-6. Settlement detection.** Yes, at O(work of the commit).
- Before `indexes::sync`, collect the roots of `dirty_runs`/`dirty_intents`. After sync, a candidate is settled iff `root_members[root]` exists and `root_active[root]` and `root_effects[root]` are absent or empty.
- Removal cost:
  - O(cohort) for runs (`root_members`), intents (`root_intents`), receipts (`indexes.receipts[run]`) and budget.
  - O(live) for compacting the positional `receipts` sequence and rebuilding offsets.
  - O(live) for `waits.retain`, because there is no per-run wait index.
  - It is never O(finished).
- Removed runs and intents are marked dirty, and a second `indexes::sync` cleans their index entries (`refresh_run`/`refresh_intent` handle missing records, `indexes.rs:520-537`, `:698-711`).

**SC-7. Expired-event redelivery collides run IDs.**
- An event redelivered after expiry is accepted as new (spec §3). Its destination key `digest(key, binding, revision, trigger)` (`lib.rs:715-716`) repeats, so the run ID `event-<key>` repeats. Its children and intents repeat too, because they derive from the run ID. History is insert-once and would refuse them.
- Resolution: events accepted from now on include their acceptance `sequence` in the destination key. The ID format `event-sha256:<64 hex>` that aq-server parses (`api.rs:223`) is unchanged.

**SC-8. Receipts owned by no run.**
- `"admin"` receipts (`lib.rs:3080`, `3132`, `3179`) and `"event:<key>"` receipts (`lib.rs:754-762`) belong to no cohort. Today GC never removes them.
- Resolution: `admin` receipts move to history (root `"admin"`) in the commit that writes them. `event:<key>` receipts move to history when their event drops.

**SC-9. Events past expiry that are still needed.**
- Today GC keeps such events:
  - while a live waiter is on one of their correlation keys;
  - while a destination is still queued (`run: None`);
  - while an uncertain async completion contract matches (`retention.rs:259-294`).
- The spec's table says "as today" without these conditions. Resolution: such entries are deferred by the event's own window (`event_expires_at_ms - accepted_at_ms`) from the current clock. The deferral counts toward the 64.
- A destination durably blocked by its *input* (`destination_blocked`/`input` receipt) never admits, so it does not hold the event. A dependency-blocked destination is live work: it holds the event and counts toward `max_queued_destinations`.

**SC-10. Keyed receipt of a still-live run.**
- The spec says the receipt expires at acceptance + window. Dropping it while its run is live would let a retry start a second run: aq-server re-admits when `admission_receipt` returns `None` (`driver.rs:515-560`).
- Resolution: an expired admission whose `run_id` is still in `runs` is deferred. This is stricter than the table and bounded by live work.

**SC-11. Trusted time.**
- Commits without a host time (`install`, `activate`, `set_retention_policy`, the `Engine::new` rebuild) neither drop window entries nor move the clock.
- `retention.clock_ms = max(clock, now)`. Window expiries are computed from `max(now, clock)`, so a backwards host time never shortens a window.
- `Engine::activate` derives its time from `receipts.last()` (`lib.rs:491-499`), which is now often empty. It uses `max(that, clock_ms)`.

**SC-12. Storage trait.**
- The history write rides on the State as `#[serde(skip)] settled: Vec<SettledCohort>` (full runs, intents and receipts). Backends persist only the public `history_items()`. `MemoryStorage::retaining_settled()` keeps the full cohorts for tests.
- New trait methods:
  - `persists_history() -> bool`, defaulting to `false`. `Engine::new` refuses `false`, so a wrapper that drops history fails closed.
  - `history_page`.
  - `delete_history`.
- Every wrapper must forward the three methods. In this repo: `tests/keyed_admission.rs:151,266`, `tests/semantics.rs:982`, `tests/catalog_inspection.rs:251`, `examples/operating_envelope.rs:46`, and the benchmark's `Measured`. In aq-server: see the follow-on section.
- Backends store the State with `settled` cleared.

**SC-13. Format 3.**
- Cohort removal stays a heavy commit. `receipts` is a positional `LazySeq`, so removal compacts it and promotes it to a full replacement (`lazy.rs:953-955`), and `waits.retain` falls back. Making it light needs sequence-keyed receipts, which is a schema and decoding change. That is not cheap, so the builder keeps it heavy.
- Window drops of events and keyed admissions stay incremental. `events`/`retention.event_tombstones` leave `HISTORY_REMOVALS`, and `stage` proves each deletion against the deleted expiry record and the header clock. An unproven deletion takes the heavy path.
- The D10 cost test's premise ("retained history") becomes "rows in `run_history`". It is rewritten in Task 5.

**SC-14. `collect_garbage`.** It keeps today's artifact phase and audit pruning (spec §3: artifact retention is unchanged). It also runs the same bounded drop and drain as a commit. `GcReport` gains `admissions`. `terminal_run_ms` is now unused, like `max_tombstones`: both are accepted and ignored.

**SC-15. Old SQL tombstones.** Each full commit now deletes at most 500 `disposal_tombstones` rows of kind `event`/`run`/`intent`. Artifact rows stay.

**SC-16. The 2 KB burst bound is tight.**
- Estimated retained bytes per run in a burst:
  - event 936 B;
  - epoch and expiry 160 B;
  - event correlation indexes about 390 B;
  - keyed receipt 240 B;
  - window members about 150 B;
  - 2 `runtime_index` event rows about 240 B.
- That is about 2.1 KB stored. Task 1 measures the baseline field split. If Task 12 measures more than 2 KB, the plan reports it and does not change the bound.

**SC-17. Versioning.** aq-server pins `=0.1.3` for a pre-D10 tree (`vendor/aq-playbooks-0173673`), while `main` is also `0.1.3`. The trait and API changes here are breaking, so runner, postgres and CLI become `0.2.0` (Task 12).

## Global Constraints

- Protected storage (writer format 2) works as it is. No namespace changes storage format, and no encryption setting changes.
- History holds public projections only. It is inserted once, never updated, and not encrypted. The host deletes it, by finish time.
- Window drop: at most **64** entries per commit, oldest first. Its cost depends on the number dropped, not on the record.
- Drain of pre-existing namespaces: at most **50** cohorts and **500** tombstone or fence entries per commit. There is no offline migration.
- Event expiry is admission + `max(policy.event_replay_ms, contract.retention_ms)`. Keyed admission expiry is acceptance + `policy.admission_replay_ms`, default **7 days** (`604_800_000` ms).
- Inspection keeps its page order, filters and public result shape. Stored states from before this change must still decode, on format 2 and format 3.
- `collect_garbage` stays callable and is never required. `max_tombstones` (and `terminal_run_ms`) are accepted and ignored.
- Acceptance numbers: 10,000 runs, 10 s per run, 10-minute windows, ≤ **1.2×**; burst of 1,000 runs, ≤ **2 KB** per run; upgrade of 1,000 legacy runs.
- `cargo test` builds examples. Every task keeps `examples/*.rs` compiling.
- Definition of done for each task:
  - `cargo fmt --check`;
  - `cargo clippy --locked --workspace --all-targets -- -D warnings`;
  - `cargo test --locked --workspace`;
  - `crates/aq-playbooks-postgres/scripts/test-postgres.sh` (disposable container; runs the postgres tests and `admin_cli`).
- No aq-server edits. Commit on `feat/internal-work` and do not push.

## Review Focus

1. **Host time moving backwards between commits.** Work is never refused, the window clock never moves back, and a newly written entry never expires early. Test: Task 6, `backwards_host_time_neither_refuses_work_nor_shortens_windows`.
2. **An upgraded namespace that still has unfinished legacy cohorts.** The drain leaves them in place, they keep running to completion, and they leave in their settling commit. A commit with more than 50 settled candidates re-arms the drain, so no settled cohort is stranded. Tests: Task 9, `legacy_live_cohorts_survive_the_drain_and_leave_when_they_finish` and the unit test `settle_cap_rearms_the_drain`.
3. **A conflicting or failed commit after history was prepared.** No history row appears, and the cohort is still live for the retry. Tests: Task 2, `conflicting_commit_writes_no_history`; Task 3, `rolled_back_commit_leaves_no_history_rows`.
4. **Paging receipts globally while live and history keys interleave.** Order is strict, with no duplicate or gap at page edges. Tests: Task 2, `inspection_pages_across_live_and_history_without_gaps`; Task 3, `history_union_pages_in_item_key_order`.
5. **An expired entry held by a waiter that never finishes.** It is re-keyed into the future and does not consume the 64 on every commit, so later due entries still drop. Test: Task 6, `deferred_entries_do_not_block_later_due_entries`.

---

## File map

| File | Responsibility |
| --- | --- |
| `crates/aq-playbooks-runner/src/history.rs` (new) | `HistoryItem`, `SettledCohort`, projection of a settled cohort |
| `crates/aq-playbooks-runner/src/retention.rs` | window index, fences (`verify_removals`), `finish_commit`, settle, drop, drain, thin `gc` |
| `crates/aq-playbooks-runner/src/storage.rs` | trait history methods; `MemoryStorage` history |
| `crates/aq-playbooks-runner/src/transition.rs` | `lazy_fields` additions; `RecordStorage` history and patch fences |
| `crates/aq-playbooks-runner/src/inspection.rs` | `page_with_history` merge |
| `crates/aq-playbooks-runner/src/lib.rs`, `admission.rs` | `transaction_choice_at`, clock, `admit_event_at`/`admit_once_at`, destination key, policy validation |
| `crates/aq-playbooks-runner/tests/live_state.rs` (new) | all new runner behaviour tests |
| `crates/aq-playbooks-postgres/migrations/0006_run_history.sql` (new) | history table, indexes, insert-once/writer triggers |
| `crates/aq-playbooks-postgres/src/history.rs` (new) | history insert, page and delete SQL |
| `crates/aq-playbooks-postgres/src/{lib.rs,retention.rs,records.rs}` | commit paths, fences, union inspection, tombstone backlog |
| `crates/aq-playbooks-postgres/tests/live_state.rs` (new) | all new PostgreSQL tests (f2 protected and f3) |
| `crates/aq-playbooks-postgres/examples/engine_growth{,_drain}.rs` | acceptance harness |
| `crates/aq-playbooks-cli/src/{main.rs,simulation.rs}` | views through storage inspection |
| `docs/implementation/0003-live-state-only.md` (+ `.results.json`) | implementation notes and acceptance evidence |

---

### Task 1: Bring the benchmark onto the branch; record baseline and a legacy namespace

**Files:**
- Create: `crates/aq-playbooks-postgres/examples/engine_growth.rs`, `crates/aq-playbooks-postgres/examples/engine_growth_drain.rs` (from `scratch/engine-growth-measure` `5928f5d`)

**Interfaces:**
- Consumes: the current public API, unchanged.
- Produces: the two examples (CLI `engine_growth <a|b|c|d|e> <runs> <url>`, `engine_growth_drain <a|b|c> <limit> <url>`); the legacy dump `.local/engine-growth-legacy/legacy-a-1000.dump` (git-ignored) and its `README` (sha256, source commit, bench key `[7u8;32]`, namespace `growth`).

- [ ] **Step 1: Copy the examples verbatim.** Run `git show 5928f5d:crates/aq-playbooks-postgres/examples/engine_growth.rs > crates/aq-playbooks-postgres/examples/engine_growth.rs`, then the same for `engine_growth_drain.rs`. Remove the "Scratch measurement (not product code)" sentence from both module docs.
- [ ] **Step 2: Build.** Run `CARGO_INCREMENTAL=0 cargo build --release --locked -p aq-playbooks-postgres --example engine_growth --example engine_growth_drain`. Expected: success with no warnings.
- [ ] **Step 3: Smoke run.** Start a disposable container with the report's flags (report "Commands" section). Run `target/release/examples/engine_growth a 5 "<url dbname=cfg_smoke>"`. Expected: 5 `run` lines, and the `checkpoint` at run 5 shows `last_run_status "Completed/Some(\"ok\")"`.
- [ ] **Step 4: Legacy namespace for acceptance 4.**
  1. In a fresh database `cfg_legacy`, run `engine_growth a 1000`. This is the unchanged engine; it takes about 2 h.
  2. Run `docker exec <c> pg_dump -U postgres -d cfg_legacy -Fc > .local/engine-growth-legacy/legacy-a-1000.dump`.
  3. Write the README. If the dump is lost, regenerate it from this task's commit in a temporary worktree with its own target directory.
- [ ] **Step 5: Commit.**

```bash
git add crates/aq-playbooks-postgres/examples/engine_growth.rs crates/aq-playbooks-postgres/examples/engine_growth_drain.rs
git commit -m "bench(postgres): bring the engine growth benchmark onto the branch"
```

### Task 2: History types, storage trait history API, in-memory backends

**Files:**
- Create: `crates/aq-playbooks-runner/src/history.rs`, `crates/aq-playbooks-runner/tests/live_state.rs`
- Modify: `crates/aq-playbooks-runner/src/{lib.rs,storage.rs,inspection.rs,transition.rs}`. Forward the new trait methods in `tests/keyed_admission.rs:151,266`, `tests/semantics.rs:982`, `tests/catalog_inspection.rs:251`, `crates/aq-playbooks-postgres/examples/operating_envelope.rs:46` and `examples/engine_growth.rs` (`Measured`).

**Interfaces:**
- Produces (`history.rs`, re-exported from `lib.rs`):
  - `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)] pub struct HistoryItem { pub kind: InspectionKind, pub key: String, pub run: String, pub root: String, pub sequence: u64, pub finished_at_ms: i64, pub value: serde_json::Value }`. `key` and `sequence` are exactly those of `inspection::rows`: the run ID; the intent ID with `invocation`; `format!("{:020}", seq)` with `seq`.
  - `#[derive(Debug, Clone)] pub struct SettledCohort { pub root: String, pub finished_at_ms: i64, pub runs: Vec<Run>, pub intents: Vec<Intent>, pub receipts: Vec<Receipt> }`
  - `impl SettledCohort { pub fn history_items(&self) -> Vec<HistoryItem> }` and `pub fn history_items(settled: &[SettledCohort]) -> Vec<HistoryItem>`
  - `State`: `#[serde(skip)] pub settled: Vec<SettledCohort>`
- Produces (`inspection.rs`): `pub fn page_with_history(state: &State, revision: u64, query: &InspectionQuery, history: Vec<HistoryItem>) -> Result<InspectionPage, StorageError>`
- Produces (`Storage` trait, defaults):
  - `fn persists_history(&self) -> bool { false }`
  - `fn history_page(&self, namespace: &str, query: &InspectionQuery) -> Result<Vec<HistoryItem>, StorageError> { Ok(vec![]) }`, returning ≤ `limit + 1` matching items in key order
  - `fn delete_history(&self, namespace: &str, owner: &str, before_ms: i64) -> Result<u64, StorageError> { Err(StorageError::Backend("history deletion unsupported".into())) }`
  - the default `inspect` becomes `load` + `history_page` + `page_with_history`
- Produces (`MemoryStorage`): `pub fn retaining_settled() -> Self`, `pub fn settled_cohorts(&self, namespace: &str) -> Vec<SettledCohort>`, `pub fn settled_run(&self, namespace: &str, id: &str) -> Option<Run>`. `RecordStorage` persists history the same way, without retention of full cohorts.
- Produces (`Engine`): `pub fn inspect(&self, query: &InspectionQuery) -> Result<InspectionPage>`, `pub fn delete_history_before(&self, before_ms: i64) -> Result<u64>`. `Engine::new` returns `Error::Refused("storage backend does not persist run history".into())` when `!store.persists_history()`, before `acquire`.

- [ ] **Step 1: Write the failing tests** in `tests/live_state.rs`.
  - Copy the helpers `set`, `authority`, `edges`, `ret`, `artifact`, `install`, `command`, `invoke_artifact`, `event_contract`, `wait_artifact`, `event`, `child_dependency` and `parent_artifact` from `tests/semantics.rs:5-380`.
  - Add `fn engine() -> Engine<MemoryStorage>` using `MemoryStorage::retaining_settled()` with namespace `"test"`.
  - Add `fn synthetic_cohort(id: &str, finished_at_ms: i64, receipts: &[u64]) -> SettledCohort`: one Completed run `id`, root `id`, with receipts at the given sequences.
  - Add `fn page(store: &impl Storage, kind: InspectionKind, after: Option<String>, limit: usize) -> InspectionPage`.

```rust
#[test]
fn memory_history_is_written_with_the_commit_and_merged_into_inspection() {
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    state.settled.push(synthetic_cohort("run-a", 10, &[1, 2]));
    let revision = store.compare_and_swap("ns", "w", 0, &state).unwrap();
    let runs = page(&store, InspectionKind::Runs, None, 10);
    assert_eq!(runs.revision, revision);
    assert_eq!(runs.items, vec![inspection::public_run(&state.settled[0].runs[0])]);
    assert_eq!(page(&store, InspectionKind::Receipts, None, 10).items.len(), 2);
    assert!(store.load("ns").unwrap().state.settled.is_empty());
    assert!(matches!(store.compare_and_swap("ns", "w", revision, &state),
        Err(StorageError::Backend(m)) if m.contains("already recorded")));
}
#[test]
fn conflicting_commit_writes_no_history() {
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    state.settled.push(synthetic_cohort("run-a", 10, &[1]));
    assert_eq!(store.compare_and_swap("ns", "w", 7, &state), Err(StorageError::Conflict));
    assert!(page(&store, InspectionKind::Runs, None, 10).items.is_empty());
}
#[test]
fn inspection_pages_across_live_and_history_without_gaps() {
    // live receipts at sequences 2 and 4, history receipts at 1, 3, 5
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    for seq in [2, 4] { state.receipts.push(Receipt { sequence: seq, run: "live".into(), step: "s".into(), at_ms: 1, kind: "admitted".into(), detail: json!({}) }); }
    state.settled.push(synthetic_cohort("done", 10, &[1, 3, 5]));
    store.compare_and_swap("ns", "w", 0, &state).unwrap();
    let mut seen = vec![];
    let mut after = None;
    loop {
        let p = page(&store, InspectionKind::Receipts, after, 2);
        seen.extend(p.items.iter().map(|v| v["sequence"].as_u64().unwrap()));
        if p.next.is_none() { break }
        after = p.next;
    }
    assert_eq!(seen, vec![1, 2, 3, 4, 5]);
}
#[test]
fn delete_history_before_removes_only_older_cohorts() {
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    state.settled = vec![synthetic_cohort("old", 10, &[1]), synthetic_cohort("new", 20, &[2])];
    store.compare_and_swap("ns", "w", 0, &state).unwrap();
    assert_eq!(store.delete_history("ns", "w", 20).unwrap(), 2); // run + receipt of "old"
    let ids: Vec<_> = page(&store, InspectionKind::Runs, None, 10).items.iter().map(|v| v["id"].clone()).collect();
    assert_eq!(ids, vec![json!("new")]);
}
#[test]
fn engine_refuses_storage_that_does_not_persist_history() {
    struct NoHistory(MemoryStorage); // forwards load/compare_and_swap/acquire/release only
    let refused = Engine::new(NoHistory(MemoryStorage::default()), "ns", "w").err().unwrap();
    assert!(matches!(refused, Error::Refused(m) if m.contains("history")));
}
```

- [ ] **Step 2: Run the tests to verify they fail.** Run `cargo test --locked -p aq-playbooks-runner --test live_state`. Expected: FAIL to compile, because `settled`, `SettledCohort` and `delete_history` are undefined.
- [ ] **Step 3: Implement the interfaces above.**
  - `MemoryStorage` keeps `history: BTreeMap<(String, String), HistoryItem>` per namespace, keyed `(kind.key(), key)`.
  - In `compare_and_swap`, insert only after every check passes, refuse an existing key (`"run history item already recorded"`), and store `state` with `settled` cleared.
  - `RecordStorage` does the same in `compare_and_swap` and `transition_v1`. In `transition_v1`, take `state.settled` after the plan and before `drop(state)`.
  - Add the forwarders in every wrapper listed under Files.
- [ ] **Step 4: Run the tests to verify they pass.** Run `cargo test --locked -p aq-playbooks-runner --test live_state`, then `cargo test --locked --workspace`. Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add crates/aq-playbooks-runner crates/aq-playbooks-postgres/examples
git commit -m "feat(runner): storage history write with the commit and history-aware inspection"
```

### Task 3: PostgreSQL `run_history` table

**Files:**
- Create: `crates/aq-playbooks-postgres/migrations/0006_run_history.sql`, `crates/aq-playbooks-postgres/src/history.rs`, `crates/aq-playbooks-postgres/tests/live_state.rs`
- Modify: `crates/aq-playbooks-postgres/src/lib.rs` (`MIGRATIONS`, `commit_state`, `inspect`, trait methods), `src/records.rs` (`transition_records`, `inspect`), `tests/support/mod.rs` (add `pub fn protected() -> PostgresStorage` with key `[7; 32]` and `pub fn records() -> PostgresStorage`)

**Interfaces:**
- Consumes: `HistoryItem`, `history_items`, `page_with_history`, `State::settled` (Task 2).
- Produces:
  - `pub(crate) fn insert(tx: &mut ObservedTransaction<'_>, namespace: &str, items: &[HistoryItem]) -> Result<(), StorageError>`
  - `pub(crate) fn page(tx: &mut ObservedTransaction<'_>, namespace: &str, query: &InspectionQuery) -> Result<Vec<HistoryItem>, StorageError>`
  - `pub(crate) fn delete_before(tx: &mut ObservedTransaction<'_>, namespace: &str, before_ms: i64) -> Result<u64, StorageError>`
  - `PostgresStorage` implements `persists_history() -> true`, `history_page` and `delete_history` (lease owner required, writer marker set).
- Schema (`0006_run_history.sql`):
  - Table `aq_playbooks.run_history(namespace text NOT NULL REFERENCES aq_playbooks.namespaces(namespace), kind text NOT NULL CHECK(kind IN ('runs','effects','receipts')), item_key text COLLATE "C" NOT NULL, run_id text NOT NULL, root_id text NOT NULL, sequence bigint NOT NULL CHECK(sequence>=0), finished_at_ms bigint NOT NULL, public_value jsonb NOT NULL, PRIMARY KEY(namespace,kind,item_key))`.
  - Indexes `run_history_by_run(namespace,kind,run_id,item_key)` and `run_history_by_finish(namespace,finished_at_ms)`.
  - A trigger `BEFORE UPDATE` raises `'run history rows are immutable'`.
  - A trigger `BEFORE INSERT OR DELETE` requires `current_setting('aq_playbooks.writer_namespace',true) = namespace`.
  - `BEFORE TRUNCATE` runs `aq_playbooks.reject_artifact_mutation()`.

- [ ] **Step 1: Write the failing tests** in `tests/live_state.rs` (`#![cfg(feature = "postgres-tests")]`, `mod support;`).
  - Write a helper `fn history_state(cohorts: Vec<SettledCohort>) -> State`, and copy `synthetic_cohort` from the runner tests.
  - Write `fn both() -> [(&'static str, PostgresStorage); 2]`, returning `[("f2", support::protected()), ("f3", support::records())]`.
  - Write `fn runs_query() -> InspectionQuery` (kind Runs, limit 1000) and `fn all_receipt_sequences(store: &PostgresStorage, ns: &str, limit: usize) -> Vec<u64>`, which pages Receipts until `next` is `None`.

```rust
#[test]
fn protected_history_rows_are_plaintext_public_and_insert_once() {
    let (ns, store) = (support::namespace("history"), support::protected());
    store.acquire(&ns, "w").unwrap();
    let revision = store.compare_and_swap(&ns, "w", 0, &history_state(vec![synthetic_cohort("run-a", 10, &[1, 2])])).unwrap();
    let mut c = support::client();
    let rows: i64 = c.query_one("SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1", &[&ns]).unwrap().get(0);
    assert_eq!(rows, 3);
    let envelope = c.query_one("SELECT snapshot IS NULL, protection_key_id IS NOT NULL FROM aq_playbooks.namespaces WHERE namespace=$1", &[&ns]).unwrap();
    assert!(envelope.get::<_, bool>(0) && envelope.get::<_, bool>(1));
    let mut tx = support::marked(&mut c, &ns);
    let err = tx.execute("UPDATE aq_playbooks.run_history SET finished_at_ms=0 WHERE namespace=$1", &[&ns]).unwrap_err();
    assert!(err.to_string().contains("run history rows are immutable"));
    assert!(store.compare_and_swap(&ns, "w", revision, &history_state(vec![synthetic_cohort("run-a", 10, &[1, 2])])).is_err());
}
#[test]
fn history_union_pages_in_item_key_order() {
    for (label, store) in both() {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let mut state = history_state(vec![synthetic_cohort("done", 10, &[1, 3, 5])]);
        for seq in [2, 4] { state.receipts.push(Receipt { sequence: seq, run: "live".into(), step: "s".into(), at_ms: 1, kind: "admitted".into(), detail: json!({}) }); }
        store.compare_and_swap(&ns, "w", 0, &state).unwrap();
        // page with limit 2 until next is None
        assert_eq!(all_receipt_sequences(&store, &ns, 2), vec![1, 2, 3, 4, 5], "{label}");
    }
}
#[test]
fn rolled_back_commit_leaves_no_history_rows() {
    for (label, store) in both() {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        assert_eq!(store.compare_and_swap(&ns, "w", 9, &history_state(vec![synthetic_cohort("run-a", 10, &[1])])), Err(StorageError::Conflict));
        assert!(store.history_page(&ns, &runs_query()).unwrap().is_empty(), "{label}");
    }
}
#[test]
fn delete_history_before_is_an_indexed_delete() {
    // cohorts finished at 10 and 20; delete_history(ns, "w", 20) == 2; only "new" remains
}
```

- [ ] **Step 2: Run the tests to verify they fail.** Run `crates/aq-playbooks-postgres/scripts/test-postgres.sh --test live_state`. Expected: FAIL, because `relation "aq_playbooks.run_history" does not exist` (or the code does not compile).
- [ ] **Step 3: Implement.**
  - Add `(6, include_str!("../migrations/0006_run_history.sql"))`.
  - `commit_state` calls `history::insert(&mut tx, namespace, &history_items(&state.settled))` after `retention::verify_and_record`.
  - In `transition_records`, the incremental path inserts before `tx.commit()`. The heavy path sets `next.settled = std::mem::take(&mut state.settled)` before `drop(state)`.
  - Format 2 `inspect`: one statement, `SELECT item_key,public_value FROM (SELECT item_key,public_value FROM aq_playbooks.inspection_rows WHERE <filters> UNION ALL SELECT item_key,public_value FROM aq_playbooks.run_history WHERE <same filters>) u ORDER BY item_key LIMIT $5`.
  - Format 3 `inspect`: `records::inspect` plus `history::page`, merged with `page_with_history` semantics.
- [ ] **Step 4: Run the tests to verify they pass.** Run `crates/aq-playbooks-postgres/scripts/test-postgres.sh --test live_state`, then the full script. Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add crates/aq-playbooks-postgres
git commit -m "feat(postgres): insert-once run_history table and history-aware inspection"
```

### Task 4: Retention state fields, retention clock and removal fences

**Files:**
- Modify: `crates/aq-playbooks-runner/src/{retention.rs,admission.rs,storage.rs,transition.rs,lib.rs}`, `crates/aq-playbooks-postgres/src/{lib.rs,retention.rs,records.rs}`. Update `RetentionPolicy` literals in tests and examples, using `..RetentionPolicy::default()`.
- Test: `crates/aq-playbooks-runner/tests/live_state.rs`, `crates/aq-playbooks-postgres/tests/live_state.rs`

**Interfaces:**
- Produces (`retention.rs`):
  - `RetentionPolicy.admission_replay_ms: u64`, with `#[serde(default = "default_admission_replay_ms")]` = `604_800_000`, also in `Default`.
  - `RetentionState` gains `#[serde(default)] clock_ms: Option<i64>`, `#[serde(default)] expiry: LazyMap<i64, BTreeSet<String>>` and `#[serde(default)] admission_expires_at_ms: LazyMap<String, i64>`, all `pub`.
  - `lazy_fields` adds `"retention.expiry"` and `"retention.admission_expires_at_ms"`.
  - `pub const WINDOW_EVENT: &str = "e:"; pub const WINDOW_ADMISSION: &str = "k:";`
  - `pub fn event_expiry(s: &State, key: &str) -> Option<i64>`: `event_expires_at_ms`, else `accepted_at_ms + policy.event_replay_ms`.
  - `pub fn admission_expiry(s: &State, key: &str) -> Option<i64>`: `admission_expires_at_ms`, else `accepted_at_ms + policy.admission_replay_ms`.
  - `pub fn expired(expiry: i64, clock_ms: Option<i64>) -> bool`
  - `pub(crate) fn advance_clock(s: &mut State, now_ms: i64)`
  - `pub const REFUSE_RUN_REMOVAL: &str = "run removal requires a history record in the same commit"; pub const REFUSE_EARLY_REMOVAL: &str = "window entry removed before its expiry"; pub const REFUSE_CLOCK: &str = "retention clock cannot decrease";`
  - `pub struct Removals { pub legacy_runs: Vec<String>, pub legacy_intents: Vec<String>, pub legacy_events: Vec<String> }` (removed in Task 10)
  - `pub fn verify_removals(previous: &State, next: &State) -> Result<Removals, StorageError>`
- Produces (`lib.rs`): `fn transaction_at<T>(&self, now_ms: Option<i64>, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T>` and `fn transaction_choice_at<T>(&self, now_ms: Option<i64>, f: impl FnOnce(&mut State) -> Result<(T, bool)>) -> Result<T>`. Every public method that takes `now_ms` uses `Some(now_ms)`. When the plan changed the state, `advance_clock` runs after `indexes::sync`.
- `admission::verify_receipt_retention` keeps its signature and delegates to `verify_removals`.

`verify_removals` rules. Each previous record that is missing from `next` is allowed when:

| Removed | Allowed when | Otherwise |
| --- | --- | --- |
| run | it is in some `next.settled[].runs`, is terminal, its children are terminal and its intents are settled; or (legacy) a `disposals` entry `kind == "run"` exists | `REFUSE_RUN_REMOVAL` / `"unresolved run obligations cannot be disposed"` |
| intent | its run is in `next.settled` and its status is settled; or (legacy) its run has a disposal | `REFUSE_RUN_REMOVAL` |
| event | `expired(event_expiry(previous, key), next.retention.clock_ms)`; or (legacy) a matching-digest `event_tombstones` entry | `REFUSE_EARLY_REMOVAL` |
| keyed admission | `expired(admission_expiry(previous, key), next clock)` (a *changed* receipt is always refused, with the existing text) | `REFUSE_EARLY_REMOVAL` |

The clock must not decrease (`REFUSE_CLOCK`). Event-tombstone and fence removal is always allowed.

- [ ] **Step 1: Write the failing tests.** In runner `tests/live_state.rs`:

```rust
#[test]
fn legacy_retention_state_and_policy_decode_with_defaults() {
    let policy: RetentionPolicy = serde_json::from_value(json!({"event_replay_ms":1,"terminal_run_ms":1,"audit_ms":1,"max_tombstones":1})).unwrap();
    assert_eq!(policy.admission_replay_ms, 604_800_000);
    assert_eq!(RetentionPolicy::default().admission_replay_ms, 604_800_000);
    let mut legacy = serde_json::to_value(State::default()).unwrap();
    for f in ["clock_ms", "expiry", "admission_expires_at_ms"] { legacy["retention"].as_object_mut().unwrap().remove(f); }
    let state: State = serde_json::from_value(legacy).unwrap();
    assert!(state.retention.clock_ms.is_none() && state.retention.expiry.is_empty());
}
fn removal_fences_hold<S: Storage>(store: S) {
    store.acquire("ns", "w").unwrap();
    let mut s = State::default();
    s.retention.clock_ms = Some(500);
    s.keyed_admissions.insert("k".into(), receipt("k", 0));      // helper: accepted_at_ms 0
    s.retention.admission_expires_at_ms.insert("k".into(), 1_000);
    s.runs.insert("run-a".into(), completed_run("run-a"));       // helper: terminal root run
    let r = store.compare_and_swap("ns", "w", 0, &s).unwrap();
    let mut next = s.clone();
    next.keyed_admissions.remove("k");
    next.retention.clock_ms = Some(999);
    assert!(matches!(store.compare_and_swap("ns", "w", r, &next), Err(StorageError::Backend(m)) if m == retention::REFUSE_EARLY_REMOVAL));
    next.retention.clock_ms = Some(1_000);
    let r = store.compare_and_swap("ns", "w", r, &next).unwrap();
    let mut gone = next.clone();
    gone.runs.remove("run-a");
    assert!(matches!(store.compare_and_swap("ns", "w", r, &gone), Err(StorageError::Backend(m)) if m == retention::REFUSE_RUN_REMOVAL));
    gone.settled.push(SettledCohort { root: "run-a".into(), finished_at_ms: 1, runs: vec![completed_run("run-a")], intents: vec![], receipts: vec![] });
    let r = store.compare_and_swap("ns", "w", r, &gone).unwrap();
    let mut back = gone.clone();
    back.settled.clear();
    back.retention.clock_ms = Some(10);
    assert!(matches!(store.compare_and_swap("ns", "w", r, &back), Err(StorageError::Backend(m)) if m == retention::REFUSE_CLOCK));
}
#[test] fn memory_storage_removal_fences() { removal_fences_hold(MemoryStorage::default()) }
#[test] fn record_storage_removal_fences() { removal_fences_hold(RecordStorage::new()) }
#[test]
fn commits_advance_the_clock_only_with_host_time() {
    let e = engine();
    install(&e, artifact("a", "done", BTreeMap::from([("done".into(), ret("ok"))]), Registry::default(), BTreeMap::new()), "a");
    e.admit("a", "main", Value::Null, authority(), 5_000).unwrap();
    e.admit("a", "main", Value::Null, authority(), 10).unwrap();
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, Some(5_000));
}
```

  In postgres `tests/live_state.rs`, add `removal_fences_hold_on_postgres` running the same body against both `support::protected()` and `support::records()`, plus:

```rust
#[test]
fn full_commit_reads_only_created_identities_from_disposal_tombstones() {
    // insert 1_000 legacy tombstone rows (kind 'event') with the writer marker, then one commit
    // that creates run "run-new": the commit succeeds, and creating an identity that has a
    // tombstone row (kind 'run', identity "run-old") is refused with "disposed run identity cannot be reused".
}
```

- [ ] **Step 2: Run the tests to verify they fail.** Run `cargo test --locked -p aq-playbooks-runner --test live_state` and `crates/aq-playbooks-postgres/scripts/test-postgres.sh --test live_state`. Expected: FAIL, because the fields are unknown.
- [ ] **Step 3: Implement.**
  - `MemoryStorage` and `RecordStorage::compare_and_swap` call `verify_removals`.
  - Add `fn verify_patch_removals(data: &RecordData, patch: &StatePatch, next: &State) -> Result<()>` to `transition.rs` and call it from `RecordStorage::transition_v1` with the same rules over deleted records.
  - PostgreSQL `commit_state` calls `verify_removals` instead of `verify_receipt_retention`.
  - In `verify_and_record`:
    - write tombstones only for `Removals.legacy_*`;
    - delete the epoch-retirement rule for removing event tombstones;
    - replace the full `SELECT ... disposal_tombstones WHERE namespace=$1` scan with `... AND (kind,identity) IN (SELECT * FROM unnest($2::text[],$3::text[]))` over identities present in `next` and absent in `previous` (runs, intents, events, artifacts). The artifact digest comparison is unchanged.
  - In `records.rs`:
    - remove `"events"` and `"retention.event_tombstones"` from `HISTORY_REMOVALS`;
    - `stage` takes `clock_ms: Option<i64>`. It allows a `keyed_admissions` or `events` deletion incrementally only when the same patch deletes its `retention.admission_expires_at_ms` or `retention.event_expires_at_ms` record with an old value ≤ clock. Otherwise it sets `history_removed = true`, so the complete path runs `verify_removals`;
    - `verify_header` checks `REFUSE_CLOCK`.
- [ ] **Step 4: Run the tests to verify they pass.** Run both commands, then `cargo test --locked --workspace`. Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add crates
git commit -m "feat(retention): retention clock, window fields and history/expiry removal fences"
```

### Task 5: A settled cohort leaves the record in the commit that settles it

**Files:**
- Modify: `crates/aq-playbooks-runner/src/{retention.rs,lib.rs,indexes.rs}`, `crates/aq-playbooks-cli/src/{main.rs,simulation.rs}`
- Rewrite tests that assert finished runs stay in the State:
  - `crates/aq-playbooks-runner/tests/{semantics.rs,review_regressions.rs,keyed_admission.rs,transition_equivalence.rs}`
  - `crates/aq-playbooks-postgres/tests/{protected_indexes.rs,keyed_admission.rs,persistence.rs,transition_cost.rs}`
  - `crates/aq-playbooks-cli/tests/{admin_cli.rs,host_pilots.rs,cli.rs}`
- Test: both `live_state.rs`

**Interfaces:**
- Consumes: Tasks 2–4.
- Produces (`retention.rs`):
  - `pub const COHORTS_PER_COMMIT: usize = 50;`
  - `#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)] pub struct CommitReport { pub cohorts: usize, pub runs: usize, pub receipts: usize, pub events: usize, pub admissions: usize, pub deferred: usize, pub drained: usize, pub more: bool }`
  - `pub(crate) fn candidate_roots(s: &State) -> BTreeSet<String>`
  - `pub(crate) fn settle(s: &mut State, candidates: BTreeSet<String>, limit: usize, report: &mut CommitReport) -> Result<()>`
  - `pub(crate) fn remove_cohort(s: &mut State, root: &str) -> Result<SettledCohort>`
  - `pub(crate) fn move_unowned_receipts(s: &mut State, owner: &str) -> Result<Option<SettledCohort>>` (for `"admin"` now and `"event:<key>"` in Task 6)
  - `pub(crate) fn finish_commit(s: &mut State, now_ms: Option<i64>, candidates: BTreeSet<String>) -> Result<CommitReport>`
- Produces (`lib.rs`): `fn transaction_report_at<T>(&self, now_ms: Option<i64>, f: impl FnOnce(&mut State) -> Result<(T, bool)>) -> Result<(T, CommitReport)>`, of which `transaction_choice_at` is a thin wrapper. Both paths run `candidate_roots` → `indexes::sync` → `finish_commit` → `indexes::sync`.
- Behaviour:
  - `remove_cohort` removes the cohort's runs, intents, `recovery_requests` of those intents, waits (`waits.retain`), receipts (compacting and then rebuilding `indexes.receipts`/`indexes.all_receipts` offsets), `indexes.receipts` entries and budget. It marks removed runs and intents dirty.
  - `finished_at_ms` is the latest terminal-receipt `at_ms` in the cohort, found through `terminal_sequence` → `indexes.all_receipts`.
  - New events' destination key is `digest(&(&key, s.sequence + 1, &binding, activation.revision, &trigger.id))` (SC-7).
  - `activate` uses `max(receipts.last().at_ms, clock_ms)`.
  - The CLI `run`/`cancel` views and `simulation::inspect` read through `Storage::inspect` (live + history) for each kind.

- [ ] **Step 1: Write the failing tests** in runner `tests/live_state.rs`. Add a helper `fn runs_page(e: &Engine<MemoryStorage>, run: Option<&str>) -> Vec<Value>` (kind Runs, limit 1000).

```rust
#[test]
fn finished_root_leaves_the_record_in_the_commit_that_finishes_it() {
    let e = engine();
    install(&e, artifact("a", "done", BTreeMap::from([("done".into(), ret("ok"))]), Registry::default(), BTreeMap::new()), "a");
    let id = e.admit("a", "main", Value::Null, authority(), 1).unwrap();
    let before = e.store.load("test").unwrap().revision;
    assert!(e.advance(&id, 2).unwrap());
    let v = e.store.load("test").unwrap();
    assert_eq!(v.revision, before + 1);
    assert!(v.state.runs.is_empty() && v.state.intents.is_empty() && v.state.receipts.is_empty() && v.state.budgets.is_empty());
    assert!(v.state.indexes.root_members.is_empty() && v.state.indexes.run_roots.is_empty() && v.state.indexes.receipts.is_empty());
    let run = &runs_page(&e, Some(&id))[0];
    assert_eq!((run["status"].as_str(), run["outcome"].as_str()), (Some("completed"), Some("ok")));
}
#[test]
fn cohort_stays_while_an_independent_child_runs() {
    let e = engine();
    let child = wait_artifact();
    e.install(child.clone()).unwrap();
    install(&e, parent_artifact(&child, CallMode::Independent), "parent");
    let id = e.admit("parent", "main", Value::Null, authority(), 0).unwrap();
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs.len(), 2);
    assert!(s.runs[&id].status.terminal());
    e.admit_event("host", event("e1", 2), 5).unwrap();
    e.drive(&mut FakeHost::default(), 5, 20).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(runs_page(&e, None).len(), 2);
}
#[test]
fn cancelled_run_stays_until_its_operation_settles_then_late_results_are_not_found() {
    let e = engine();
    install(&e, invoke_artifact(command(Recovery::KeyedIdempotent), None), "invoke");
    let id = e.admit("invoke", "main", Value::Null, authority(), 0).unwrap();
    let mut host = FakeHost { responses: BTreeMap::from([("test.command".into(), vec![DispatchResult::Unknown { reason: "lost".into() }])]), ..Default::default() };
    e.drive(&mut host, 0, 10).unwrap();
    e.cancel(&id, 1).unwrap();
    e.advance(&id, 1).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].status, RunStatus::Cancelling);
    let intent = s.intents.keys().next().unwrap().clone();
    let done = DispatchResult::Completed { outcome: "ok".into(), output: Value::Null };
    e.record_result(&intent, done.clone(), 2).unwrap();
    e.advance(&id, 2).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(runs_page(&e, Some(&id))[0]["status"], "cancelled");
    assert!(matches!(e.record_result(&intent, done, 3), Err(Error::NotFound(_))));
    assert!(matches!(e.advance(&id, 3), Err(Error::NotFound(_))));
    assert!(matches!(e.cancel(&id, 3), Err(Error::NotFound(_))));
}
#[test]
fn history_equals_the_projection_of_the_removed_cohort() {
    // run the invoke scenario to completion with a Completed response; for the settled cohort in
    // e.store.settled_cohorts("test"): every public_run/public_intent/public_receipt equals the
    // value Engine::inspect returns for the same key and kind.
}
#[test]
fn admin_receipts_move_to_history_in_the_commit_that_writes_them() {
    let e = engine();
    install(&e, artifact("a", "done", BTreeMap::from([("done".into(), ret("ok"))]), Registry::default(), BTreeMap::new()), "a");
    e.set_activation_controls("a", ActivationControls::default(), 3).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.receipts.iter().all(|r| r.run != "admin"));
    let receipts = e.inspect(&InspectionQuery { kind: InspectionKind::Receipts, run: Some("admin".into()), after: None, limit: 10 }).unwrap();
    assert_eq!(receipts.items[0]["kind"], "activation_controls");
}
#[test]
fn record_storage_settles_like_memory_storage() {
    // run cohort_stays_while_an_independent_child_runs's script on Engine<RecordStorage>
    // and Engine<MemoryStorage>; after every call the serialized snapshots are equal and the
    // Runs/Effects/Receipts inspection pages are equal.
}
```

  In postgres `tests/live_state.rs`:

```rust
#[test]
fn finished_run_leaves_protected_snapshot_and_projections_and_writes_no_tombstone() {
    // Engine<PostgresStorage> over support::protected(); support::artifact("a") installed and activated;
    // admit + advance: load().state.runs is empty; no inspection_rows and no runtime_index row has
    // run_id/item_key = id; run_history has kind 'runs' item_key = id; disposal_tombstones count == 0.
    // Same assertions with support::records() (except inspection_rows/runtime_index, absent on f3).
}
```

- [ ] **Step 2: Run the tests to verify they fail.** Run `cargo test --locked -p aq-playbooks-runner --test live_state finished_root`. Expected: FAIL, with `assertion failed: v.state.runs.is_empty()`.
- [ ] **Step 3: Implement the interfaces above.** `finish_commit` in this task does three things, in this order:
  1. `advance_clock`;
  2. `settle(..., COHORTS_PER_COMMIT)`;
  3. `move_unowned_receipts(s, "admin")`.
- [ ] **Step 4: Rewrite the existing tests that assert finished runs remain.**
  - Every runner test engine uses `MemoryStorage::retaining_settled()`.
  - A read of a settled run's private fields (`output`, `variables`, `attempts`, intent `result`) becomes `e.store.settled_run("test", &id)` / `settled_cohorts`.
  - A read of public fields becomes `e.inspect(..)`.
  - Counts of `snapshot().runs`/`receipts`/`intents` after settlement assert history counts instead.
  - `transition_cost.rs` seeds "history" as cohorts in `run_history`/`RecordStorage` history, not as live runs. It still asserts zero full loads and zero fallbacks and flat record reads for `admit` and `event`. For `dispatch` (which settles), it asserts that record reads are flat across histories 0/32/128/512 and that full commits are exactly the settling ones.
  - CLI `admin_cli.rs:201` reads the run from the storage inspection view.
  - Record each rewritten test name and its old assertion for the implementation notes (Task 12), in `.local/0003-rewritten-tests.txt`.
- [ ] **Step 5: Run the tests to verify they pass.** Run `cargo test --locked --workspace` and `crates/aq-playbooks-postgres/scripts/test-postgres.sh`. Expected: PASS.
- [ ] **Step 6: Commit.**

```bash
git add crates
git commit -m "feat(runner): settled cohorts leave the record as history in the settling commit"
```

### Task 6: Window entries with expiry and per-commit bounded drop

**Files:**
- Modify: `crates/aq-playbooks-runner/src/{retention.rs,lib.rs,admission.rs}`
- Test: both `live_state.rs`

**Interfaces:**
- Consumes: `finish_commit`, `CommitReport`, fences (Tasks 4–5).
- Produces:
  - `pub const WINDOW_DROPS_PER_COMMIT: usize = 64;`
  - `pub(crate) fn window_insert(s: &mut State, expires_at_ms: i64, member: String)`
  - `pub(crate) fn drop_expired(s: &mut State, limit: usize, report: &mut CommitReport) -> Result<()>`, which uses `s.retention.clock_ms` as now
- Behaviour:
  - `admit_event_in_epoch` computes the expiry from `max(now, clock)` and calls `window_insert(expiry, "e:<key>")`.
  - `admit_once` records `admission_expires_at_ms[key] = max(now, clock) + admission_replay_ms` and calls `window_insert(.., "k:<key>")`.
  - `drop_expired` pops the oldest member while its expiry ≤ clock, up to `limit` examined members:
    - **`k:`** — if `runs` contains its `run_id`, defer it to `clock + admission_replay_ms`. Otherwise remove the receipt and its expiry.
    - **`e:`** — if a live waiter, a queued destination that is not input-blocked, or an uncertain completion contract holds it (SC-9), defer it to `clock + (event_expires_at_ms - accepted_at_ms)`. Otherwise remove the event, `event_epochs`, `event_expires_at_ms`, `event_memberships` and `indexes.events` entries, and call `move_unowned_receipts(s, "event:<key>")`.
  - No tombstone, correlation fence or disposal is written.
  - `finish_commit` order is `advance_clock` → `settle` → `drop_expired(WINDOW_DROPS_PER_COMMIT)` → admin move. The clock step and the drop run only when `now_ms` is `Some`; settle and the admin move always run.

- [ ] **Step 1: Write the failing tests** in runner `tests/live_state.rs`. Add helpers `fn ret_engine() -> Engine<MemoryStorage>` (installs binding `"a"`, a return-only playbook) and `fn keyed(e: &Engine<MemoryStorage>, key: &str) -> KeyedAdmissionRequest` (binding `"a"`, current activation revision, `authority()`).

```rust
#[test]
fn window_drop_is_bounded_to_64_per_commit_oldest_first() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy { admission_replay_ms: 1_000, ..RetentionPolicy::default() }).unwrap();
    for i in 0..65 {
        let d = e.admit_once(keyed(&e, &format!("k-{i:03}")), i).unwrap();
        e.advance(&d.receipt.run_id, i).unwrap();
    }
    assert_eq!(e.snapshot().unwrap().keyed_admissions.len(), 65);
    e.admit_once(keyed(&e, "k-new"), 2_000).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.keyed_admissions.keys().cloned().collect::<Vec<_>>(), vec!["k-064".to_string(), "k-new".to_string()]);
}
#[test]
fn expired_event_leaves_without_tombstone_or_fence_and_redelivery_is_new() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy { event_replay_ms: 1, ..RetentionPolicy::default() }).unwrap();
    let first = e.admit_event("host", event("e1", 2), 0).unwrap(); // expires at max(1, 1_000)
    e.admit("waiter", "main", Value::Null, authority(), 1_001).unwrap();
    let s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&first.key));
    assert!(s.retention.event_tombstones.is_empty() && s.retention.expired_correlations.is_empty());
    assert!(!e.admit_event("host", event("e1", 2), 1_002).unwrap().duplicate);
}
#[test]
fn wait_registered_after_its_event_expired_times_out() { // NR-1 option (a)
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy { event_replay_ms: 1, ..RetentionPolicy::default() }).unwrap();
    e.admit_event("host", event("e1", 2), 0).unwrap();
    let id = e.admit("waiter", "main", Value::Null, authority(), 1_001).unwrap();
    e.advance(&id, 1_001).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
    e.advance(&id, 1_101).unwrap();
    e.advance(&id, 1_102).unwrap();
    assert_eq!(runs_page(&e, Some(&id))[0]["outcome"], "error");
}
#[test]
fn keyed_admission_of_a_live_run_outlives_its_window() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy { admission_replay_ms: 1_000, ..RetentionPolicy::default() }).unwrap();
    let first = e.admit_once(keyed(&e, "k"), 0).unwrap();
    e.admit_once(keyed(&e, "other"), 2_000).unwrap();
    let again = e.admit_once(keyed(&e, "k"), 2_001).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.receipt.run_id, first.receipt.run_id);
}
#[test]
fn deferred_entries_do_not_block_later_due_entries() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    install(&e, artifact("a", "done", BTreeMap::from([("done".into(), ret("ok"))]), Registry::default(), BTreeMap::new()), "a");
    e.set_retention_policy(RetentionPolicy { admission_replay_ms: 1_000, event_replay_ms: 1, ..RetentionPolicy::default() }).unwrap();
    let waiter = e.admit("waiter", "main", Value::Null, authority(), 0).unwrap();
    e.advance(&waiter, 0).unwrap();                          // live wait, minimum revision 2
    e.admit_event("host", event("held", 1), 0).unwrap();     // revision 1: no match; held by the waiter; window 1_000
    for i in 0..65 { let d = e.admit_once(keyed(&e, &format!("k-{i:03}")), 1_000 + i).unwrap(); e.advance(&d.receipt.run_id, 1_000 + i).unwrap(); }
    e.admit_once(keyed(&e, "x"), 3_000).unwrap();            // event deferred to 4_000 + 63 drops
    e.admit_once(keyed(&e, "y"), 3_001).unwrap();            // the remaining 2 drop; the event is not re-examined
    let s = e.snapshot().unwrap();
    assert_eq!(s.keyed_admissions.len(), 2);
    assert_eq!(s.events.len(), 1);
    assert_eq!(s.retention.expiry.keys().next().copied(), Some(4_000));
}
#[test]
fn backwards_host_time_neither_refuses_work_nor_shortens_windows() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy { admission_replay_ms: 1_000, ..RetentionPolicy::default() }).unwrap();
    e.admit("a", "main", Value::Null, authority(), 5_000).unwrap();
    let d = e.admit_once(keyed(&e, "late"), 10).unwrap();     // backwards: accepted, not refused
    e.advance(&d.receipt.run_id, 11).unwrap();
    e.admit("a", "main", Value::Null, authority(), 12).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.retention.clock_ms, Some(5_000));
    assert_eq!(s.retention.admission_expires_at_ms.get("late").copied(), Some(6_000));
    assert!(s.keyed_admissions.contains_key("late"));
}
#[test]
fn burst_at_a_frozen_clock_leaves_at_most_2_kb_of_window_entries_per_run() {
    // ret_engine plus wait_artifact-style event per run: 200 keyed admissions + 200 events at now = 0,
    // each run driven to completion; (canonical_bytes(state) after run 200 - after run 1) / 199 <= 2_048;
    // every remaining live key in runs/intents/receipts/waits/budgets is empty.
}
```

  Add `record_storage_drops_like_memory_storage` (the 64-drop script on both backends; snapshots and inspection equal). In postgres `tests/live_state.rs`, add `window_drop_on_postgres_is_incremental_on_f3_and_fenced_on_f2`: run the 64-drop script on f2 protected and f3. On f3, `transition_stats().heavy` does not increase across the dropping commit.
- [ ] **Step 2: Run the tests to verify they fail.** Run `cargo test --locked -p aq-playbooks-runner --test live_state window`. Expected: FAIL, with 66 receipts left.
- [ ] **Step 3: Implement the interfaces above.** `admit_event_in_epoch` keeps reading legacy `event_tombstones` (`lib.rs:625-634`), and wait registration keeps reading legacy `expired_correlations` (`lib.rs:1852-1860`), until Task 9 drains them.
- [ ] **Step 4: Run the tests to verify they pass.** Run both test commands and `cargo test --locked --workspace`. Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add crates
git commit -m "feat(retention): expiry-ordered window entries dropped 64 per commit"
```

### Task 7: Stale requests refused by producer time

**Files:**
- Modify: `crates/aq-playbooks-runner/src/{lib.rs,admission.rs}`
- Test: `crates/aq-playbooks-runner/tests/live_state.rs`

**Interfaces:**
- Produces:
  - `pub const STALE_REQUEST: &str = "stale request: producer time is older than the replay window";`
  - `pub fn admit_event_at(&self, authenticated_producer: &str, event: EventEnvelope, epoch: u64, requested_at_ms: Option<i64>, now_ms: i64) -> Result<EventDisposition>`. The window is `max(policy.event_replay_ms, contract.retention_ms)`.
  - `pub fn admit_once_at(&self, request: KeyedAdmissionRequest, requested_at_ms: Option<i64>, now_ms: i64) -> Result<KeyedAdmissionDisposition>`. The window is `policy.admission_replay_ms`.
  - `admit_event`/`admit_event_in_epoch`/`admit_once` delegate with `None`.
- Rules:
  - A live duplicate (same key still in the record) answers as today, whatever its producer time.
  - Otherwise, if `requested_at_ms < max(now, clock) - window`, return `Error::Refused(STALE_REQUEST.into())` with no write.
  - Producer time never enters a digest or a stored record.

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn stale_producer_time_is_refused_and_missing_time_is_accepted_as_new() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    install(&e, artifact("a", "done", BTreeMap::from([("done".into(), ret("ok"))]), Registry::default(), BTreeMap::new()), "a");
    e.set_retention_policy(RetentionPolicy { admission_replay_ms: 600_000, event_replay_ms: 600_000, ..RetentionPolicy::default() }).unwrap();
    let now = 1_000_000;
    assert!(matches!(e.admit_once_at(keyed(&e, "k1"), Some(now - 600_001), now), Err(Error::Refused(m)) if m == STALE_REQUEST));
    let first = e.admit_once_at(keyed(&e, "k1"), Some(now - 600_000), now).unwrap();
    assert!(!first.duplicate);
    assert!(e.admit_once_at(keyed(&e, "k1"), Some(0), now + 1).unwrap().duplicate);
    assert!(matches!(e.admit_event_at("host", event("e1", 2), 0, Some(now - 600_001), now), Err(Error::Refused(m)) if m == STALE_REQUEST));
    assert!(!e.admit_event_at("host", event("e1", 2), 0, Some(now - 600_000), now).unwrap().duplicate);
    e.advance(&first.receipt.run_id, now + 2).unwrap();
    let later = now + 600_001;
    e.admit("a", "main", Value::Null, authority(), later).unwrap(); // drops k1
    let again = e.admit_once_at(keyed(&e, "k1"), None, later + 1).unwrap();
    assert!(!again.duplicate);
    assert_ne!(again.receipt.run_id, first.receipt.run_id);
}
```

- [ ] **Step 2: Run the test to verify it fails.** Run `cargo test --locked -p aq-playbooks-runner --test live_state stale`. Expected: FAIL to compile, because `admit_once_at` is undefined.
- [ ] **Step 3: Implement the signatures above.**
- [ ] **Step 4: Run the test to verify it passes.** Same command; expected PASS. Then run `cargo test --locked --workspace`.
- [ ] **Step 5: Commit.**

```bash
git add crates/aq-playbooks-runner
git commit -m "feat(runner): refuse stale events and keyed admissions by producer time"
```

### Task 8: Quotas count live work

**Files:**
- Modify: `crates/aq-playbooks-runner/src/{lib.rs,retention.rs}` (`set_retention_policy`, and the `gc` tombstone quota checks at `retention.rs:302-325`), `crates/aq-playbooks-postgres/src/retention.rs` (the count check at `:174-185`)
- Test: `crates/aq-playbooks-runner/tests/live_state.rs`

**Interfaces:**
- `set_retention_policy(&self, policy: RetentionPolicy) -> Result<()>` refuses only `event_replay_ms == 0 || admission_replay_ms == 0 || audit_ms == 0`, with the existing message changed to `"retention windows must be positive"`.
- `max_tombstones` and `terminal_run_ms` are stored and never read.
- `quota_counts`/`Accounting::counts` are unchanged; they now see live work plus the window.

- [ ] **Step 1: Write the failing tests.**

```rust
#[test]
fn max_runs_counts_live_runs_only() {
    let e = Engine::with_config(MemoryStorage::retaining_settled(), "test", "i-1", EngineConfig { max_runs: 2, ..EngineConfig::default() }).unwrap();
    install(&e, artifact("a", "done", BTreeMap::from([("done".into(), ret("ok"))]), Registry::default(), BTreeMap::new()), "a");
    for t in 0..5 { let id = e.admit("a", "main", Value::Null, authority(), t).unwrap(); e.advance(&id, t).unwrap(); }
    e.admit("a", "main", Value::Null, authority(), 10).unwrap();
    e.admit("a", "main", Value::Null, authority(), 11).unwrap();
    assert!(matches!(e.admit("a", "main", Value::Null, authority(), 12), Err(Error::Refused(m)) if m.contains("quota")));
}
#[test]
fn max_receipts_counts_the_window_and_frees_when_it_passes() {
    let e = Engine::with_config(MemoryStorage::retaining_settled(), "test", "i-1", EngineConfig { max_receipts: 10, ..EngineConfig::default() }).unwrap();
    install(&e, artifact("a", "done", BTreeMap::from([("done".into(), ret("ok"))]), Registry::default(), BTreeMap::new()), "a");
    e.set_retention_policy(RetentionPolicy { admission_replay_ms: 1_000, ..RetentionPolicy::default() }).unwrap();
    for i in 0..10 { let d = e.admit_once(keyed(&e, &format!("k{i}")), i).unwrap(); e.advance(&d.receipt.run_id, i).unwrap(); }
    assert!(matches!(e.admit_once(keyed(&e, "k10"), 10), Err(Error::Refused(m)) if m.contains("quota")));
    assert!(!e.admit_once(keyed(&e, "k10"), 2_000).unwrap().duplicate);
}
#[test]
fn max_tombstones_and_terminal_run_ms_are_accepted_and_ignored() {
    let e = engine();
    e.set_retention_policy(RetentionPolicy { max_tombstones: 0, terminal_run_ms: 0, ..RetentionPolicy::default() }).unwrap();
    assert_eq!(e.snapshot().unwrap().retention.policy.max_tombstones, 0);
}
```

- [ ] **Step 2: Run the tests to verify they fail.** Run `cargo test --locked -p aq-playbooks-runner --test live_state max_`. Expected: FAIL; the third test is refused with "retention horizons and tombstone quota must be positive".
- [ ] **Step 3: Implement.** Remove the GC tombstone and fence quota refusals and the PostgreSQL durable tombstone count refusal. Update `protected_indexes.rs::disposal_quota_refuses_atomically_and_expired_unreferenced_artifact_metadata_is_collected` to assert only the artifact half, and note it for Task 12.
- [ ] **Step 4: Run the tests to verify they pass.** Run `cargo test --locked --workspace` and the postgres script. Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add crates
git commit -m "feat(retention): quotas count live work; max_tombstones is ignored"
```

### Task 9: Drain pre-existing namespaces

**Files:**
- Modify: `crates/aq-playbooks-runner/src/retention.rs`, `crates/aq-playbooks-postgres/src/{retention.rs,lib.rs,records.rs}`
- Test: both `live_state.rs`

**Interfaces:**
- Produces:
  - `#[derive(Debug, Clone, Serialize, Deserialize, Default)] pub struct DrainState { pub done: bool, pub root_cursor: Option<String>, pub phase: u8, pub cursor: Option<String> }`, as `RetentionState.drain` with `#[serde(default)]`
  - `pub const DRAIN_COHORTS_PER_COMMIT: usize = 50; pub const DRAIN_ENTRIES_PER_COMMIT: usize = 500;`
  - `pub(crate) fn drain_step(s: &mut State, report: &mut CommitReport) -> Result<()>`
  - PostgreSQL: `pub(crate) fn drain_tombstones(tx: &mut ObservedTransaction<'_>, namespace: &str) -> Result<u64, StorageError>`, with `const SQL_TOMBSTONES_PER_COMMIT: i64 = 500`
- Behaviour, while `!drain.done`, in every changed commit, timed or not, after `settle`:
  - It examines up to `DRAIN_COHORTS_PER_COMMIT - report.cohorts` roots after `root_cursor` and removes the settled ones (as `remove_cohort`, giving history).
  - Separately it spends up to `DRAIN_ENTRIES_PER_COMMIT` on these phases in order:
    0. remove `event_tombstones`;
    1. remove `expired_correlations`;
    2. index keyed admissions into `admission_expires_at_ms` + `expiry` (from `accepted_at_ms + policy.admission_replay_ms`);
    3. index events into `expiry` (from `event_expiry`, idempotent set insert);
    4. clear `retention.disposals` (counts its length).
  - When all phases are exhausted, `done = true`.
  - When `settle` hits `COHORTS_PER_COMMIT`, it re-arms the drain (`done = false`, `root_cursor = None`).
  - PostgreSQL deletes `disposal_tombstones` rows of kind `event`/`run`/`intent` with `WHERE ctid IN (SELECT ctid ... LIMIT 500)` in `commit_state` and the f3 incremental path.

- [ ] **Step 1: Write the failing tests.** Add a helper `fn legacy(cohorts: usize, tombstones: usize) -> MemoryStorage`:
  1. Create a `retaining_settled` store, install and activate `"a"` through a throwaway engine, and release it.
  2. `load` the state. Inject `cohorts` settled one-run cohorts: Completed `run-0-legacy-{i:04}` with `admitted` and `terminal` receipts and a budget. These IDs sort before new `run-<hex>` IDs, so the drain cursor meets them first. Inject `tombstones` entries `tomb-{i:04}` into `event_tombstones`.
  3. Set `indexes.initialized = false`, remove `retention.drain` from the JSON (legacy), and `compare_and_swap` it as owner `"writer"`.

```rust
#[test]
fn legacy_namespace_drains_50_cohorts_and_500_entries_per_commit() {
    let e = Engine::new(legacy(120, 1_200), "test", "i-1").unwrap(); // the Engine::new commit is commit 1
    let count = |e: &Engine<MemoryStorage>| { let s = e.snapshot().unwrap(); (s.runs.len(), s.retention.event_tombstones.len()) };
    assert_eq!(count(&e), (70, 700));
    e.admit("a", "main", Value::Null, authority(), 1).unwrap();
    assert_eq!(count(&e), (21, 200)); // 20 legacy cohorts left plus the new, still live run
    e.admit("a", "main", Value::Null, authority(), 2).unwrap();
    assert_eq!(count(&e), (2, 0));
    assert!(e.snapshot().unwrap().retention.drain.done);
    assert_eq!(runs_page(&e, None).iter().filter(|r| r["id"].as_str().unwrap().starts_with("run-0-legacy-")).count(), 120);
}
#[test]
fn legacy_live_cohorts_survive_the_drain_and_leave_when_they_finish() {
    // legacy(60, 0) plus 5 injected legacy roots `run-0-live-{i}` in status Admitted on binding "a":
    // after Engine::new and one more commit: the 60 settled cohorts are in history, the 5 live runs
    // are still in snapshot().runs; advancing each live run once completes it and removes it in
    // that commit; finally snapshot().runs is empty and history lists 65 runs.
}
```

  In `src/retention.rs` `#[cfg(test)] mod tests`, add `settle_cap_rearms_the_drain`. It builds a State with 60 settled one-run cohorts, all passed as candidates, with `drain.done = true`. After `settle(.., COHORTS_PER_COMMIT, ..)`: 10 runs remain, `drain.done == false` and `drain.root_cursor == None`. After one `drain_step`: 0 runs remain.

  In postgres `tests/live_state.rs`, add `legacy_protected_namespace_drains_records_and_sql_tombstones`:
  - commit a legacy state of 120 cohorts through `compare_and_swap` on f2 protected, with `indexes.initialized = false`;
  - insert 1_200 `disposal_tombstones` rows (kind `event`) with the writer marker;
  - after `Engine::new`: 70 runs and 700 rows;
  - after 2 more commits: 0 legacy runs and 0 rows;
  - `run_history` holds 120 `runs` items.
- [ ] **Step 2: Run the tests to verify they fail.** Run `cargo test --locked -p aq-playbooks-runner --test live_state legacy`. Expected: FAIL, with `left: (120, 1200)`.
- [ ] **Step 3: Implement the interfaces above.** Phases 0–1 now remove what `advance_producer_epoch` used to compact. Leave that method's retain logic, which is now a no-op on drained namespaces.
- [ ] **Step 4: Run the tests to verify they pass.** Run `cargo test --locked --workspace` and the postgres script. Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add crates
git commit -m "feat(retention): drain legacy cohorts and tombstones in bounded steps per commit"
```

### Task 10: `collect_garbage` as a thin wrapper; legacy removal paths retired

**Files:**
- Modify: `crates/aq-playbooks-runner/src/{retention.rs,lib.rs}`, `crates/aq-playbooks-postgres/src/{retention.rs,records.rs}`, `crates/aq-playbooks-cli/src/main.rs` (unchanged call; help text)
- Rewrite GC tests:
  - `tests/semantics.rs`: `gc_preserves_bound_transitive_closure_then_collects_terminal_cohorts_in_pages`, `gc_preserves_possible_async_completion_while_lost_admission_handle_is_unknown`, `gc_tombstones_expired_event_identity_and_epoch_fence_compacts_safely`, `producer_epoch_compaction_keeps_history_fences_for_existing_invocations`, `pruned_general_wait_history_refuses_registration_and_captured_replay_horizon_survives_policy_change`
  - `tests/review_regressions.rs`: `completed_cohort_gc_removes_ready_timer_and_dependency_index_residue`, `expired_replay_tombstones_and_retired_epochs_refuse_old_identities`, `garbage_collection_bounds_preserved_candidate_examination`, `garbage_collection_preserves_transitively_bound_artifacts_without_runs`, `retired_producer_epochs_need_no_new_tombstones_for_late_gc`
  - `tests/keyed_admission.rs::gc_and_direct_storage_cas_cannot_forget_or_rebind_a_key`
  - `crates/aq-playbooks-postgres/tests/protected_indexes.rs`: `event_gc_replay_tombstones_survive_restart_and_explicit_new_epoch_has_new_identity`, `forged_disposal_cannot_erase_unresolved_children_or_external_effects`
  - `crates/aq-playbooks-cli/tests/admin_cli.rs:259-265`

**Interfaces:**
- `pub fn collect_garbage(&self, now_ms: i64, limit: usize) -> Result<GcReport>`. It keeps the `1..=1000` limit check and the "clock cannot move backwards" check against `last_gc_at_ms`.
  - It runs `transaction_report_at(Some(now_ms), ..)`, whose closure runs `retention::gc`. That function is now only the artifact phase (unchanged, bounded by `limit`), catalog-history, `artifact_disposals` audit pruning and `last_gc_at_ms`.
  - The commit's `finish_commit` performs the same bounded settle, drop and drain as any commit.
  - `GcReport { runs, events, artifacts, receipts, preserved, more }` is filled from the `CommitReport` and the artifact phase. It gains `#[serde(default)] pub admissions: usize`.
  - `more = report.more || !drain.done || artifact phase unfinished`.
- `Removals` and all legacy branches (disposal-based run and intent removal, tombstone-based event removal, SQL tombstone writes for runs, intents and events) are deleted. No code writes `retention.disposals`, `event_tombstones` or `expired_correlations`.

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn collect_garbage_is_optional_and_reports_the_bounded_drop() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy { admission_replay_ms: 1_000, ..RetentionPolicy::default() }).unwrap();
    for i in 0..70 { let d = e.admit_once(keyed(&e, &format!("k{i:03}")), i).unwrap(); e.advance(&d.receipt.run_id, i).unwrap(); }
    let report = e.collect_garbage(5_000, 10).unwrap();
    assert_eq!((report.runs, report.admissions, report.more), (0, 64, true));
    let report = e.collect_garbage(5_001, 10).unwrap();
    assert_eq!((report.admissions, report.more), (6, false));
    assert!(e.snapshot().unwrap().retention.disposals.is_empty());
    assert!(matches!(e.collect_garbage(4_000, 10), Err(Error::Refused(_))));
}
```

- [ ] **Step 2: Run the test to verify it fails.** Run `cargo test --locked -p aq-playbooks-runner --test live_state collect_garbage`. Expected: FAIL, because `admissions` is missing or there is 1 run and 0 admissions.
- [ ] **Step 3: Implement.** Rewrite the listed tests to assert the live-state equivalents:
  - "terminal cohort pages" become settle-on-finish plus history;
  - "tombstones refuse old identities" become "expired identity without producer time is new; with stale producer time refused";
  - "forged disposal" becomes "removal without a history record or before expiry is refused" (`REFUSE_RUN_REMOVAL` / `REFUSE_EARLY_REMOVAL`).
  - Keep the artifact-closure and async-handle preservation assertions as they are.
  - Add each to `.local/0003-rewritten-tests.txt`.
- [ ] **Step 4: Run the tests to verify they pass.** Run `cargo test --locked --workspace` and the postgres script. Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add crates
git commit -m "refactor(retention): collect_garbage runs the commit's bounded drop; legacy disposal paths removed"
```

### Task 11: Format 3 parity on PostgreSQL

**Files:**
- Test: `crates/aq-playbooks-postgres/tests/live_state.rs`
- Modify (only if the test finds a gap): `crates/aq-playbooks-postgres/src/records.rs`

**Interfaces:**
- Consumes: everything above. Produces no new API. Builder decision recorded: cohort removal stays heavy on f3 (SC-13).

- [ ] **Step 1: Write the test.**

```rust
#[test]
fn format_3_matches_the_reference_for_settle_drop_stale_and_drain() {
    // Script: install "a"; 3 keyed admissions + drive (settle) at 0..2; policy admission_replay_ms 1_000;
    // admit at 2_000 (drops 3); stale admit_once_at refused; legacy drain of 60 cohorts seeded by CAS.
    // Run on Engine<MemoryStorage> and Engine<PostgresStorage::connect_records>:
    //  - after every call: serialized snapshots equal and Runs/Effects/Receipts pages equal;
    //  - TransitionStats delta: settling commits heavy == 1 each; the dropping commit heavy == 0,
    //    incremental == 1; fallbacks empty on non-settling commits.
}
```

- [ ] **Step 2: Run the test.** Run `crates/aq-playbooks-postgres/scripts/test-postgres.sh --test live_state format_3`. Expected: PASS, or a concrete divergence. Fix any divergence in `records.rs` (`stage`/heavy routing) and re-run until PASS.
- [ ] **Step 3: Commit.**

```bash
git add crates/aq-playbooks-postgres
git commit -m "test(postgres): format 3 parity for settlement, window drop and drain"
```

### Task 12: Acceptance runs 1–4 and 6, implementation notes, version

**Files:**
- Modify: `crates/aq-playbooks-postgres/examples/engine_growth.rs`, `crates/aq-playbooks-postgres/examples/engine_growth_drain.rs`, `Cargo.toml` (workspace dependency pins), `crates/aq-playbooks-{runner,postgres,cli}/Cargo.toml` (`version = "0.2.0"`), `Cargo.lock`
- Create: `docs/implementation/0003-live-state-only.md`, `docs/implementation/0003-live-state-only.results.json`

**Interfaces:**
- `engine_growth <steady|burst|a|b|c|d|e> <runs> <url>`:
  - `steady`: protected; run *k* starts at *k*·10,000 ms; `event_replay_ms = admission_replay_ms = 600_000`; never calls `collect_garbage`. Every run is admitted with `admit_once_at(.., Some(now), now)`.
  - `burst`: same policy at a frozen clock, then the NR-2 sequence.
  - Configs a–e keep their old meaning.
- `Measured` captures `inspection::public_run(run)` for every run in `state.settled` at commit time (an in-memory map from ID to Value).
- Each scenario prints `{"kind":"verdict","acceptance":<n>,"pass":<bool>, ...measured values}`.
- `engine_growth_drain upgrade <runs> <steady_bytes> <url>`:
  - opens the restored legacy namespace `growth` (protected key `[7u8;32]`);
  - sets the 10-minute policy;
  - serves `runs` new runs at 10 s spacing, starting at `1000·100_000 + 86_400_000 + 1_000` ms;
  - prints record bytes per commit and the verdict.

- [ ] **Step 1: Extend the benchmark** with the interfaces above. The verdict checks are:
  1. Record bytes (`SIZE_SQL` total) and median step time at run 10,000 are ≤ 1.2 × their values at run 200.
  2. Per-run growth over the burst is ≤ 2,048 B; after the burst no runs, intents, waits or budgets are left. Under NR-2, the final record is ≤ 1.2 × run 1; the literal "after 20" figure is also printed.
  3. After `steady`, paging `Runs` (limit 1000) lists exactly 10,000 distinct IDs, and each history value equals the captured projection.
  4. `upgrade`: every legacy ID `run-*` from the dump appears in history; the record is ≤ 1.2 × `steady_bytes` once `drain.done`; the new runs all complete `ok`.
- [ ] **Step 2: Build.** Run `CARGO_INCREMENTAL=0 cargo build --release --locked -p aq-playbooks-postgres --example engine_growth --example engine_growth_drain`. Expected: success.
- [ ] **Step 3: Run acceptance 1–3** on a fresh pinned container (report settings):
  - `engine_growth steady 10000 <url cfg_steady> > .local/0003/steady.jsonl`
  - `engine_growth burst 1000 <url cfg_burst> > .local/0003/burst.jsonl`

  Expected: `verdict` lines 1, 2 and 3 with `"pass":true`.
- [ ] **Step 4: Run acceptance 4.** Restore `.local/engine-growth-legacy/legacy-a-1000.dump` into `cfg_upgrade` with `pg_restore`, then run `engine_growth_drain upgrade 200 <steady run-200 bytes> <url cfg_upgrade> > .local/0003/upgrade.jsonl`. Expected: verdict 4 with `"pass":true`.
- [ ] **Step 5: Run acceptance 6.** Run `cargo fmt --check && cargo clippy --locked --workspace --all-targets -- -D warnings && cargo test --locked --workspace && crates/aq-playbooks-postgres/scripts/test-postgres.sh`. Expected: all pass.
- [ ] **Step 6: Bump versions** to `0.2.0` (runner, postgres, cli; workspace pins `=0.2.0`) and run `cargo test --locked --workspace` again. `Cargo.lock` is updated by `cargo update -p aq-playbooks-runner -p aq-playbooks-postgres -p aq-playbooks-cli --offline`.
- [ ] **Step 7: Write `docs/implementation/0003-live-state-only.md`.** It covers:
  - what changed, with file:line;
  - the spec-check rulings applied (NR-1, NR-2, SC-1–SC-17);
  - **which request types carry producer time**: `Engine::admit_event_at`, `Engine::admit_once_at`, and that aq-playbook-host `Request::Event`/`Request::AdmitOnce` must pass it;
  - the history table shape;
  - format 3 heavy cohort removal;
  - the **list of every rewritten test with its old and new assertion** (from `.local/0003-rewritten-tests.txt`);
  - acceptance results 1–4 and 6 with environment, image digest and binary sha256;
  - limits.

  `docs/implementation/0003-live-state-only.results.json` holds the verdict lines and checkpoint summaries.
- [ ] **Step 8: Commit.**

```bash
git add Cargo.toml Cargo.lock crates docs/implementation/0003-live-state-only.md docs/implementation/0003-live-state-only.results.json
git commit -m "docs(implementation): 0003 live-state-only acceptance and notes; engine 0.2.0"
```

---

## aq-server follow-on (not tasks for this repo)

**How aq-server vendors this engine** (`/home/jkern/dev/aq-platform/wt/status-sync-server`):
- **Source tree.** An exact source tree goes in `vendor/aq-playbooks-<shortrev>/`. It is produced with `git archive --format=tar <rev> Cargo.toml Cargo.lock crates scripts docs/implementation/current-catalog-inspection-design.md`. The tar itself is kept as `vendor/aq-playbooks-source-<ver>.tar`.
- **Pin.** `vendor/aq-playbooks-source-<ver>.json` records `schema_version`, `source_repository`, `source_revision`, `publication_profile`, `archive_format`, `archive_sha256`, `vendor_root` and `files[{path,mode,bytes,sha256}]`.
- **Check.** `scripts/check-playbook-source.py` (CI `lint`) verifies every pin file by file.
- **Wiring.**
  - `crates/aq-playbook-host/Cargo.toml:8-10` path dependencies and `=` versions move to the new directory and `=0.2.0`.
  - The root `Cargo.toml` `exclude` gets the new vendor directory.
  - `crates/aq-playbook-host/README.md` gets a version section.
  - Earlier vendor trees and archives stay unchanged.
- **Docker.** Docker builds that share a cargo cache need the `touch` step over `vendor` (aq-server AGENTS.md).

**Public API differences between `vendor/aq-playbooks-0173673` and this branch that the server's call sites hit:**

1. **Lazy State containers (D10, already on `main`).** `State.runs`, `receipts`, `intents`, `waits`, `events`, `budgets`, `keyed_admissions`, the `indexes.*` maps and `retention.*` maps are `LazyMap`/`LazySeq`/`LazySet`. Through `Deref`, `.get`, `.values`, `.len` and indexing still compile. Code that names `BTreeMap`/`Vec` types for these fields, or builds them, needs `.into()`.
2. **Finished runs are absent from `snapshot()`.**
   - `artifacts/tests.rs:124` looks for roots in `state.runs`; it must use `Request::Inspect`.
   - `coding/tests.rs:261,465` use `state.runs.values().all(terminal)` as a completion check. That is now vacuously true and must check history instead.
   - Run lists and timelines already go through `Request::Inspect` (`inspection.rs:296-448`, `driver.rs:809`) and keep working.
   - `Request::Runs` (`IndexQuery::Runs`) now lists live runs only.
3. **`Storage` trait.** It gains `persists_history`, `history_page` and `delete_history`, and `Engine::new` refuses a backend without history. The test wrappers `crates/aq-playbook-host/tests/owner.rs:123` (`Blocked`), `tests/catalog.rs:22` (`Counted`) and `tests/catalog.rs:130` (`Opaque`) must forward all three.
4. **Producer time.**
   - `aq-playbook-host` `Request::AdmitOnce` and `Request::Event` (`lib.rs:125-137`) gain `#[serde(default)] requested_at_ms: Option<i64>` and call `admit_once_at` / `admit_event_at` (`lib.rs:634-661`).
   - `driver.rs:544` (AdmitOnce) passes the creation time of the `playbook_admissions` row. `driver.rs:709` and `:958` (Event) pass the outbox row's first-delivery time.
   - `KeyedAdmissionRequest` and `EventEnvelope` are unchanged, so `service.rs:867` and the `EventEnvelope` literals compile as they are.
5. **Admission recovery.** `admission_receipt` returns `None` once the window passes and the run has settled. In `driver.rs:515-560`, a `None` outside recovery re-admits. With `requested_at_ms` set, the engine refuses that as `STALE_REQUEST`. Map that refusal to the existing hold/`Unknown` path, never to a fresh admission.
6. **Retention policy.**
   - `RetentionPolicy` gains `admission_replay_ms`.
   - aq-server never calls `set_retention_policy` today. The owner thread (`aq-playbook-host/src/lib.rs:363-367`, after `engine.config = ...`) should apply a policy from `Limits`: `event_replay_ms: 600_000` and `admission_replay_ms: 600_000` (10 minutes, spec "aq-server"), with `audit_ms` at its default. `terminal_run_ms` and `max_tombstones` are ignored.
   - The engine limits in `runtime.rs:159-165` (`max_runs` 128, `max_receipts` 4096, `max_state_bytes` 8 MiB) now count live work and the window. Keep them.
7. **`EventStatus` and late results.**
   - `EventStatus` (`aq-playbook-host/src/lib.rs:629-633`) reads `snapshot().events`. It works within the 10-minute window, which covers its use right after `Fanout` (`driver.rs:734`). An expired key returns `Invalid`.
   - A late result for an operation of a removed cohort returns `NotFound` from `record_result`. Host adapters treat that as dropped.
8. **Schema migration.** `PostgresStorage::initialize` applies migration `0006` (`aq_playbooks.run_history`) on the owner thread's first connect. Protected namespaces keep format 2 and encryption, and history rows are plaintext public projections, like `inspection_rows`.
9. **Acceptance 5.** The 500-run routing scale test (`docs/implementation/0011-R3-scale.md`) must pass its limit after the re-vendor.
