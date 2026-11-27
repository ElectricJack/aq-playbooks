# aq-playbooks

aq-playbooks is the workflow engine of AQ v2: a standalone Rust compiler, runner and authoring toolchain for repeatable project policy. It turns a policy document into a sealed, versioned playbook artifact, admits and evaluates runs durably (in memory or in PostgreSQL), and publishes the versioned host catalogs that AQ's server binds to. The runner issues host effects as durable intents; the embedding host keeps authority over work, evidence, decisions, findings and source control. For the platform overview, see the umbrella repository, [aq-platform/agent-queue](https://github.com/aq-platform/agent-queue), which includes this repository as a submodule at `aq-playbooks/`.

The workspace contains seven crates:

| Crate | Responsibility |
| --- | --- |
| `aq-playbooks-model` | Versioned artifacts, canonical JSON/hashes, bounded schemas and expressions, typed named joins, structural verification, fixed-interval occurrence planning |
| `aq-playbooks-compiler` | Source/proposal validation, outcome-sensitive type checking, deterministic packaging, semantic diffs, and the authoring workspace and examples |
| `aq-playbooks-runner` | Atomic transitions, event admission, waits, effects/recovery, children/joins/cancellation, fair indexed scheduling, retention, controls, quotas, metrics, and in-memory storage |
| `aq-playbooks-simulation` | Offline simulation of sealed playbooks: scripted scenarios and the recorded authoring cases, run on the same engine |
| `aq-playbooks-postgres` | Private migrations, exclusive namespace ownership, transactional snapshots/indexes, protected persistence, key rotation and immutable artifact storage |
| `aq-playbooks-cli` | The `aq-playbooks` binary: the authoring agent's tools (`check`, `seal`, `try`, `compare`, `facts`, `save`, `ask`, `unable`), inspection, standalone administration, and the versioned AQ host catalogs; its tests keep a stateful fake host and three pilots (coding, watcher, report) |
| `aq-playbooks-plugin-conformance` | `aq-plugin-conformance`, a driver that checks an installed exec plugin package against its manifest and catalog without an AQ server or database (see its [README](crates/aq-playbooks-plugin-conformance/README.md)) |

## Try it

CI builds with Rust 1.96.1 (edition 2024). Offline commands need no database or AQ checkout.

```sh
cargo test --workspace
cargo run -p aq-playbooks-cli -- commands list
cargo run -p aq-playbooks-cli -- examples --output .local/examples
cd .local/examples/notify-when-ready
cargo run -p aq-playbooks-cli -- check
cargo run -p aq-playbooks-cli -- try cases/still-no-description.json
cargo run -p aq-playbooks-cli -- compare
```

Build into the checkout's own `target/` by default (in AQ, each slot keeps its own `target/`); setting `CARGO_TARGET_DIR` works as usual. The workspace dev/test profile keeps disk growth bounded: workspace crates get `line-tables-only` debuginfo (backtraces still report file:line), dependencies get none, and incremental compilation is off. Opt back in for a debugging session with `CARGO_INCREMENTAL=1` or `CARGO_PROFILE_DEV_DEBUG=2`.

`examples` writes the worked authoring workspaces of `crates/aq-playbooks-compiler/authoring-examples`: a document, the files the host places beside it, the finished draft, its cases, the previously active artifact, and documents the agent cannot turn into steps with the `ask` or `unable` it runs for them. The authoring tools run only with a workspace's root as the current directory. `crates/aq-playbooks-cli/tests/authoring_examples.rs` runs them on every example.

`commands` and `events` support `list` and `describe`; in a workspace they read its `catalog.json`. Run `--help` for exact arguments.

A local authoring workspace can include `extensions.json`, an array of registrations with `name`, `registry` and `notes`. Add the selected extension commands, events and capabilities to `record.json` as usual. To simulate a plugin call, supply its outcome and output in the case's `returns` map under the command name, such as `ext.test.echo`.

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

The host catalogs live under [`contracts/`](contracts): `aq-host` (1.0.0, twelve commands) and `aq-host-2.0.0` through `aq-host-2.7.0` (2.7.0 has 26 commands and 13 events). Each is published beside its predecessors, whose bytes stay frozen, and is pinned by a contract test in `crates/aq-playbooks-cli/tests`; `commands list` shows the 1.0.0 catalog unless a workspace or `--registry` supplies another. The runner reaches the outside world only through the `aq_playbooks_runner::Host` trait (`authorize`, `dispatch`, `reconcile`, `cancel`). This repository contains no AQ adapter for it: the CLI and the test fake host are synthetic fixtures, and they do not execute live AQ effects. The production host lives in AQ's server. The [host integration boundary](docs/aq-host-integration.md) is a dated snapshot (2026-10-31) of what the host must provide, so read its status claims as of that date.

For confidential persistence, supply `--protection-keys /path/to/owner-only-keyring.json` consistently. The keyring selects an active key and retains old keys needed for rotation/backups; it is never stored in PostgreSQL. See the adapter documentation before rotating keys or restoring an encrypted backup. Without a configured provider, runs with sensitive command contracts and events with declared sensitive payload fields are refused before admission.

The [seventy policy examples](docs/research/2026-10-30-sdlc-policy-exploration.md) have a [capability disposition](docs/scenario-dispositions.csv). The [v1 capability selection](docs/research/0001-v1-to-v2-capability-selection.md) is supporting research, not a compatibility requirement.
