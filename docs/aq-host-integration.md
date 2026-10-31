# AQ host integration boundary

The Rust engine is independently buildable. Its production host is AQ, which must bind the twelve contracts to authoritative domain services and drive the embedded runner. A passing fake-host test is not evidence that these production services exist.

## Verified external dependency

On 2026-10-30 the sibling `agent-q` checkout exposes foundation routes for health/readiness, user identity/administration, enrollment, worker heartbeat/probe/revocation, and updates. Its current server and protocol crates do not expose the playbook work, judgment, decision, findings, messaging, check, or candidate integration services.

This matches the [AQ ecosystem design](../../../agent-q/docs/architecture/0001-ecosystem-and-releases.md), which assigns integration adapters and product acceptance to `agent-q` and states that implementation follows the foundation. Relevant current route registrations are in [`aq-server/src/lib.rs`](../../../agent-q/crates/aq-server/src/lib.rs), [`admin_api.rs`](../../../agent-q/crates/aq-server/src/admin_api.rs), and [`worker_api.rs`](../../../agent-q/crates/aq-server/src/worker_api.rs).

This is a blocker for live AQ end-to-end acceptance, not for finishing or testing the standalone playbook component. Implementing missing AQ task/execution/decision services inside the workflow engine would violate the ownership boundary.

## Host responsibilities

| Boundary | Required AQ implementation |
| --- | --- |
| Registration | Capture exact typed command/event fingerprints, finite context views and work-change variants; never register every daemon method automatically |
| Authority | Authenticate caller/producer separately from payload fields; retain actor, exact scope and capability ceiling; recheck current grants and object revisions; stamp envelope subject/revision consistently with authoritative payloads |
| Work and judgments | Durable work admission and terminal results, independent reviewer/disposition checks, bounded model assessments, truthful operation handles and cancellation |
| Decisions and checks | Authorized revision-bound human decisions and executed check evidence; neither can be replaced by an agent-completed document |
| Candidates | Use the integration owner's public services and aq-git's immutable candidate/conditional publication contract; independently enforce evidence and destination preconditions |
| Observations and findings | Frozen finite complete observations, pre-scan episode revisions, trusted evaluations, atomic finding changes and notification intents |
| Messaging | Stable identities, scoped visibility, idempotent enqueue, separate acceptance/delivery/acknowledgment |
| Delivery | Commit domain transition and event outbox together; retry stable producer/event/epoch identities until durable runner admission; retire epochs explicitly before compacting replay identities |
| Runtime | Own one engine namespace, drive bounded ready/timer/effect queues, publish the scheduler's durable calendar occurrences, reconcile uncertain operations, and provide backups and configured protection keys; export process metrics to a retained telemetry backend |
| Release | Pin released library and contract versions; run assembled-product acceptance without path dependencies on sibling source checkouts |

The engine exposes `Host::authorize`, `dispatch`, `reconcile`, and `cancel`. Implementations dispatch only the exact registered command contract, preserve `Intent.id`, input digest and original authority, and return truthful typed outcomes. `Storage` provides atomic CAS, indexed discovery and bounded safe inspection. Readback/recovery remains host-authoritative even when an operator records an evidence-backed recovery request through the CLI.

The synthetic contract catalog and stateful fake host in `aq-playbooks-cli` are executable integration fixtures. Their checks cover the four pilot workflows, source revisions, findings completeness, notification recovery, visibility, publication conflicts and late outcomes. They are not adapters for existing daemon endpoints.

## Live acceptance still required

After AQ implements and binds these services, exercise the actual coding/integration, routing, findings-watcher, and supervisor-report pilots through the embedded runner. Include daemon restart between domain commit and event delivery, lost effect responses, revoked authority, changed candidate/base revisions, notification retry, late author completion, and protected database backup/restore with the correct keyring. Record the exact AQ/library/contract versions tested.
