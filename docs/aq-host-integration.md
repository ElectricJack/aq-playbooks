# AQ host integration boundary

The Rust engine is independently buildable. Its production host is AQ, which must bind the twelve contracts to authoritative domain services and drive the embedded runner. A passing fake-host test is not evidence that these production services exist.

## Current dependency and implementation status

This boundary is reconciled on2026-10-31 against committed daemon
`0551d84c0ba9653da8310d0596a833182fe73341` (state20), rather than the old
foundation-only inventory. AQ now has durable agent graph operations/recovery,
task/session messaging with distinct node/harness dispositions, revisioned
profile/project bindings and intelligence selection, human notification
transport/unknown recovery, and session/observation/transport services. Those are
owning services to reuse; their existence does not mean the twelve playbook
adapters or central supervisor authority are already implemented.

| Existing AQ owner | Reuse and remaining boundary |
| --- | --- |
| `agent_access::mutations` | Finite scoped graph intents, guarded versions and retained operation recovery; not arbitrary work creation, task completion or a central-supervisor credential |
| `agent_access::messaging` | Immutable task/session mail, scoped visibility and node/harness delivery; central supervisor/playbook authority adapter still required |
| `profile_catalog`, `project_bindings`, `intelligence`, `profiled_execution` | Existing catalog, selection, policy/input/Started ownership; no second selector or fabricated worker assignment inside policy |
| `notifications` | Durable bounded human delivery and unknown recovery; human acknowledgment/decision, episode and report-window policy remain separate |
| `observations`, `terminal` | Typed finite inspection/control seams; native source/runtime adoption and full waits/hibernation remain separately qualified |

Current code registration is in [daemon lib.rs](../../aq-server/crates/aq-server/src/lib.rs).
The detailed staged [AQ-12-through-17 plan](../../aq-server/docs/implementation/AQ-12-through-17-plan.md)
maps exact commands, authority, outbox/recovery and qualification gates. The
shared [ecosystem ownership](../../aq-server/docs/architecture/0001-ecosystem-and-releases.md)
still applies: AQ owns product/domain integration; the runner never reads AQ's
private domain tables or recreates their state owners.

The component publication profile is a pinned source archive/inventory and
commit, not a registry-publication claim. Current playbooks source
`42f944996eafad5af07d9ec1c72d3e3f2630c03b` retains public package identities0.1.0;
its four library src/manifests/migrations are byte-identical to qualified
PB04 stageA`7e669e179e6cc726185ac80627a6bdca146944f1`. Later differences are
unlinked experiment examples/scripts. AQ must vendor/verify exact committed
source closure without sibling live paths; it must not mutate same-version
published artifacts or link the CLI's synthetic catalog/fake host as adapters.
PB04 still misses its25ms budget and has no qualified normal workload envelope.
Bounded embedding/isolation is not performance closure or a500-agent claim.

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

### Session operations, waits and human delivery extension

The required [AQ operations/waits design](../../aq-server/docs/architecture/0004-session-operations-waits-and-notifications.md)
adds typed session/usage/wait/delivery context and events through versioned host
catalogs. It preserves the twelve ordinary commands. `message.send` and
`findings.reconcile` route through AQ's durable delivery services;
`decision.request` keeps authenticated human responses separate from transport
acceptance. Digest/escalation policy consumes those same episode and schedule
identities, including late author and uncertain external-send recovery.

Engine-native waits suspend workflow execution. An agent's self-suspension,
checkpoint, source retention and wake are AQ execution-domain operations with
their own scoped agent API; the runner does not own a parked process or duplicate
the continuation registry. Registered operation results/events expose their
truthful outcomes. A dependency notification does not itself prove source success.
Stall mechanics remain in AQ, with policy events available to workflows rather
than an unrestricted session-control command family.

AQ-27 qualifies these production services using the unchanged runner primitives;
INT-03 adds actual external delivery and cross-node continuation evidence. These
are planned host extensions; current AQ messaging/notification/observation owners
are reused through explicit adapters, not inferred from synthetic pilots.

### Existing pilot acceptance

After AQ binds the existing owners and implements the remaining services, exercise the actual coding/integration, routing, findings-watcher, and supervisor-report pilots through the embedded runner. Include daemon restart between domain commit and event delivery, lost effect responses, revoked authority, changed candidate/base revisions, notification retry, late author completion, and protected database backup/restore with the correct keyring. Record the exact AQ/library/contract versions tested.

## Revision-pinned implementation evidence

The [bounded v1 reference inventory](../../aq-server/docs/research/2026-10-30-v1-implementation-reference-map.md) records implementation symbols, selected test obligations and per-file hashes at revision `9a9b8e8ef992d1ff699755a9383ed3c91e2844ea`. It distinguishes preserved behavior from remote-authority and retention adaptations. This evidence informs the owning v2 tasks; replacement coverage and product qualification remain pending.

## Reconciled AQ host catalog 1.0.0 (D4)

The authoritative [schema artifact](../contracts/aq-host/registry.json),
[manifest](../contracts/aq-host/manifest.json) and
[fixtures](../contracts/aq-host/fixtures.json) are published together. The CLI,
fake host and all four pilots load that artifact. Version `1.0.0` has fingerprint
`sha256:627125cc393d8274bfaa9ffc785c338e566cc69af362ac027a3daa355c3509e1`.
The fingerprint covers canonical JSON of `[version, registry]`; command/event
fingerprints cover their individual canonical definitions. See the
[vendor instructions](../contracts/aq-host/README.md). A consumer must pin both
the source revision and this manifest, and deliberately rebind changed contracts.
There are twelve commands and seven events within the daemon's 262,144-byte
registry bound. The five context views, nine work kinds and six work.change
variants are unchanged and listed in the manifest.

### Recovery and outcomes

Every mutation except publication uses an immutable operation identity and input
binding: repeating the identical identity/input/authority must return the retained
admission or terminal result, without another domain effect. A conflicting reuse
refuses. This is the host's keyed idempotency obligation; registration alone does
not establish that the production adapter meets it. Observations may be re-read
with a new invocation identity, retaining each observation. Publication must query
the integration owner's authoritative receipt before deciding on resubmission.
No command requires naturally idempotent or unsafe recovery in this version.

| Command | Recovery | Exact named outcomes |
| --- | --- | --- |
| `context.read` | Observation | observed, incomplete, expired, refused |
| `work.ensure` | KeyedIdempotent | accepted, ensured, completed, refused |
| `work.change` | KeyedIdempotent | changed, conflict, refused |
| `judgment.request` | KeyedIdempotent | assessed, refused |
| `decision.request` | KeyedIdempotent | accepted, approved, rejected, expired, cancelled, refused |
| `check.request` | KeyedIdempotent | passed, failed, infrastructure_failure, refused |
| `candidate.prepare` | KeyedIdempotent | prepared, conflict, refused |
| `candidate.publish` | Reconcilable | published, conflict, refused |
| `message.send` | KeyedIdempotent | queued, refused |
| `findings.reconcile` | KeyedIdempotent | reconciled, conflict, refused |
| `operation.get` | Observation | running, completed, failed, cancelled, indeterminate, refused |
| `operation.cancel` | KeyedIdempotent | accepted, refused, too_late, cancelled, indeterminate |

`accepted` on an asynchronous request carries only `{handle}` and promises
admission. `work.ensure` returns `ensured` for independent work admission and
`completed` for a retained owned-work result. The latter contains a separate
business `disposition` (accepted, changes_requested, failed). `message.send`'s
`queued` records an identified enqueue; delivery and human acknowledgment remain
separate host facts. `operation.cancel`'s `accepted` is cancellation admission,
`cancelled` confirms termination, and `too_late` reports an already terminal
operation. Transport rejection and uncertain transport responses remain
`DispatchResult::Rejected` and `DispatchResult::Unknown`, respectively.

`decision.request` is asynchronous. It binds a question, declared choices,
eligible responders and expiry to an exact subject revision, admits a durable
handle, then completes with approved/rejected/expired/cancelled/refused. The host
authenticates the human responder separately from the workflow's JSON and checks
eligibility, current revision and deadline. No configured approval on the request
can complete it. Pending `operation.get` returns `running`; terminal readback
returns `completed` with `{outcome, output}`, preserving the original work or
decision business result. This replaces the ambiguous `succeeded` readback.
Cancellation or indeterminate state can be observed separately. Reconciliation
of a lost dispatch response must retain the same identity and authority.

The current bounded judgment adapter is synchronous (`assessed`); making model
assessment admission asynchronous is a future contract change. This choice
retains the existing routing pilot's behavior and does not imply execution of
an unimplemented production model adapter.

### Event names, producers and schemas

All seven events have semantic version `1.0.0`, authenticated producer
`aq.host.bridge@1`, scope `project`, payload bound 65,536 bytes, and retention
86,400,000 ms. Each envelope requires the correlation map field `operation`, whose
value is the authoritative operation handle (for domain notifications without
an admitted async handle, the originating domain operation identity). The handle
is opaque and preserved byte-for-byte; it is not inferred from subject ID. The
producer must be authenticated by the host bridge; a payload field claiming the
producer confers no authority.

Let `Subject = {id: bounded string, revision: nonnegative integer}` and
`Result = {reference: bounded string, subject: Subject}`. Strings are at most
4,096 bytes unless the artifact specifies a smaller bound. Objects are closed:
extra keys refuse. The machine schemas in registry.json are authoritative.

| Event | Authoritative source behind bridge | Payload | Envelope subject/revision paths |
| --- | --- | --- | --- |
| `aq.work.changed` | Work/graph owner | Result plus outcome: ensured/changed/completed/cancelled | subject.id, subject.revision |
| `aq.artifact.recorded` | Evidence owner | Result plus outcome: recorded | subject.id, subject.revision |
| `aq.operation.updated` | Owned work result owner | `{outcome: completed/refused, output: Result plus disposition: accepted/changes_requested/failed}` | output.subject.id, output.subject.revision |
| `aq.check.completed` | Executed check owner | Result plus outcome: passed/failed/infrastructure_failure/refused | subject.id, subject.revision |
| `aq.decision.completed` | Human decision owner | `{outcome: approved/rejected/expired/cancelled/refused, output: Result}` | output.subject.id, output.subject.revision |
| `aq.integration.changed` | Integration/publication owner | Result plus outcome: prepared/published/conflict/refused | subject.id, subject.revision |
| `aq.signal.reported` | Authorized signal owner | Result plus outcome: reported | subject.id, subject.revision |

The bridge publishes stable producer/event identities and causation references,
with occurrence and observation times. Subject and revision in the envelope must
match the specified payload paths. Owned-work and decision completions carry
`correlation.operation = accepted.handle`. A duplicate event with the same identity
and content is a replay; conflicting content refuses. Events do not grant the
workflow new authority. Domain state and the outgoing event must commit atomically
in the owning host service (D3 outbox), and delivery may retry until admitted.

The old synthetic names map as follows: work.changed → aq.work.changed,
artifact.recorded → aq.artifact.recorded, operation.changed → aq.operation.updated,
check.completed → aq.check.completed, decision.resolved → aq.decision.completed,
integration.changed → aq.integration.changed, signal.reported → aq.signal.reported.
Old producers and fingerprints are incompatible; do not alias them silently.
The daemon's old `aq.operation.updated` queued-message payload is also incompatible
with this work-completion schema despite sharing its name. Message enqueue is
represented by the `queued` command receipt; message delivery events need a new
versioned extension if required.

### Production adoption boundary

D5 must vendor this bundle by exact Git revision, relax the blanket
Reconcilable-only registry guard to accept the published per-command classes,
and replace independent command/event definitions with the exact artifact.
The existing daemon message/decision inputs, result fields, version strings,
producer mappings and operation lookup are also different; changing just the
outcome names cannot satisfy fingerprint equality. Production adapters must
translate to their owning services while enforcing this public schema and current
authority. D3's outbox and D8's triggers must use the event table above.
Production fixture/replay and assembled-pilot checks remain D5–D8/D11 work; D4's
synthetic results do not claim those adapters are already bound or qualified.
