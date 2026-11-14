# aq-playbooks

A standalone Rust workflow compiler and runner for repeatable project policy. AQ can embed the runner; host services retain authority over work, evidence, decisions, findings, and source control.

The workspace contains five crates:

| Crate | Responsibility |
| --- | --- |
| `aq-playbooks-model` | Versioned artifacts, canonical JSON/hashes, bounded schemas and expressions, typed named joins, structural verification, fixed-interval occurrence planning |
| `aq-playbooks-compiler` | Source/proposal validation, outcome-sensitive type checking, deterministic packaging, semantic diffs |
| `aq-playbooks-runner` | Atomic transitions, event admission, waits, effects/recovery, children/joins/cancellation, fair indexed scheduling, retention, controls, quotas, in-memory storage and simulation |
| `aq-playbooks-postgres` | Private migrations, exclusive namespace ownership, transactional snapshots/indexes, protected persistence, key rotation and immutable artifact storage |
| `aq-playbooks-cli` | The authoring agent's tools (`check`, `seal`, `try`, `compare`, `facts`, `save`, `ask`, `unable`), inspection, standalone administration and the AQ host catalogs; its tests keep a stateful fake host and three pilots |

## Try it

Rust 1.96 was used for verification. Offline commands need no database or AQ checkout.

```sh
cargo test --workspace
cargo run -p aq-playbooks-cli -- commands list
cargo run -p aq-playbooks-cli -- examples --output .local/examples
cd .local/examples/notify-when-ready
cargo run -p aq-playbooks-cli -- check
cargo run -p aq-playbooks-cli -- try cases/still-waiting.json
cargo run -p aq-playbooks-cli -- compare
```

Build into the checkout's own `target/` (leave `CARGO_TARGET_DIR` unset; in AQ, each slot keeps its own `target/`). The workspace dev/test profile keeps disk growth bounded: workspace crates get `line-tables-only` debuginfo (backtraces still report file:line), dependencies get none, and incremental compilation is off. Opt back in for a debugging session with `CARGO_INCREMENTAL=1` or `CARGO_PROFILE_DEV_DEBUG=2`.

`examples` writes the worked authoring workspaces of `docs/authoring/examples`: a document, the files the host places beside it, the finished draft, its cases, the previously active artifact, and documents the agent cannot turn into steps with the `ask` or `unable` it runs for them. The authoring tools run only with a workspace's root as the current directory. `tests/authoring_examples.rs` runs them on every example.

`commands` and `events` support `list` and `describe`; in a workspace they read its `catalog.json`. Run `--help` for exact arguments.

## Durable administration

`install`, `activate`, `run`, `inspect`, `cancel`, `reconcile`, `controls`, `authority`, `retention`, `gc`, `retire-producer`, `metrics`, and `rotate-keys` require an explicit `--database-url` (or `AQ_PLAYBOOKS_DATABASE_URL`) and `--namespace`. Mutations acquire exclusive namespace ownership and refuse another active owner. Authority and run input are explicit JSON files. Activation and execution are separate from installation.

The standalone CLI admits runs and evaluates pure transitions. Production effects require an embedded host implementing `aq_playbooks_runner::Host`; the CLI does not invent AQ adapters. `reconcile --request` records an evidence-backed recovery request; actual readback/cancellation uses that registered host. `inspect --kind runs|effects|receipts --after CURSOR --limit N` uses bounded indexed public projections. The convenience PostgreSQL connection uses `NoTls`; hosts can supply a TLS-configured client.

Run real persistence tests against a disposable PostgreSQL 17 container:

```sh
crates/aq-playbooks-postgres/scripts/test-postgres.sh
```

The script creates and removes its own database container. See the [adapter documentation](crates/aq-playbooks-postgres/README.md) for direct database testing and measurements.

## Format and implementation status

Read the [design](docs/specs/0001-aq-playbooks.md), [implementation decisions and limits](docs/specs/0002-implementation.md), [portable format](crates/aq-playbooks-model/FORMAT.md), and [acceptance evidence](docs/acceptance.md). The [authoring skill](skills/aq-playbook-author/SKILL.md) is shipped with this repository.

The component supports fair indexed discovery, bounded safe inspection, explicit retention cleanup with replay fences, protected snapshots/artifacts, compatible named callable selection and typed named join results. PostgreSQL still commits a whole namespace snapshot, with incremental native index/projection updates; namespace size therefore remains an important performance bound. Parallel work uses owned async children/operations and joins; foreach traversal is serial. Unsupported recursive/dynamic workflows and other deferred constructs are rejected explicitly.

`Engine.metrics.snapshot()` exposes bounded process timing histograms and admission/refusal/deduplication counters for host export. `metrics` reports aggregate state gauges and retained-receipt timings; this explicit diagnostic scans the snapshot and requires keys for protected stores.

AQ now owns the required underlying graph, messaging, selection, notification and session services, and has qualified a bounded protected embedding and first durable message adapter. The full twelve-command host and central supervision remain separately implemented and qualified. See the [verified host integration boundary](docs/aq-host-integration.md). The standalone CLI and fake-host pilots do not enable those services or claim live execution.

For confidential persistence, supply `--protection-keys /path/to/owner-only-keyring.json` consistently. The keyring selects an active key and retains old keys needed for rotation/backups; it is never stored in PostgreSQL. See the adapter documentation before rotating keys or restoring an encrypted backup. Without a configured provider, runs with sensitive command contracts and events with declared sensitive payload fields are refused before admission.

The [seventy policy examples](docs/research/2026-10-30-sdlc-policy-exploration.md) have a [capability disposition](docs/scenario-dispositions.csv). The [v1 capability selection](docs/research/0001-v1-to-v2-capability-selection.md) is supporting research, not a compatibility requirement.
