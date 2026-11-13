# 0003 — Live state only: implementation notes

Spec: `docs/specs/0003-live-state-only.md`. The main notes are written at
the end of the plan (Task 12). This file starts with the request-time rule
(spec §3, ruling "Producer time").

## Request time and stale requests

After its window an event or keyed admission receipt leaves the record, and
the engine no longer remembers its key. A request that repeats such a key is
judged by time, not by a remembered key.

### Which requests carry a request time

| Engine method | Request time | Window |
| --- | --- | --- |
| `Engine::admit_event_at(producer, event, epoch, requested_at_ms, now_ms)` | `requested_at_ms: Option<i64>` | `max(policy.event_replay_ms, contract.retention_ms)` |
| `Engine::admit_once_at(request, requested_at_ms, now_ms)` | `requested_at_ms: Option<i64>` | `policy.admission_replay_ms` (default 7 days) |
| `Engine::admit_event`, `Engine::admit_event_in_epoch` | none (they pass `None`) | — |
| `Engine::admit_once` | none (it passes `None`) | — |

No other request type carries one. The CLI has no event or keyed-admission
command. In aq-playbook-host, `Request::Event` and `Request::AdmitOnce` are
the carriers. Until the server switches to the `_at` methods (aq-server
follow-on), every request goes through with no request time.

The request time is the producer's time for the *request*: for aq-server,
the time it first created the admission or the outbox event. It is not the
event's business time (`occurred_at_ms` / `observed_at_ms`). A calendar
trigger sets that to the due date, which can be days old. The request time is
not part of the event digest, the keyed request digest or any stored record.
It never moves the retention clock.

### The rule

1. **A key the engine still remembers answers as before, whatever its
   request time.** Inside its window a repeat is a duplicate, and changed
   content is a `Conflict`. The same holds after the window while the
   entry is held: a keyed receipt whose run is still live, or an expired
   event that a live wait, a queued destination or an unknown completion
   handle still needs.
2. **Otherwise** (the key is absent, or expired and held by nothing), a
   request with `requested_at_ms <= max(now, clock) − window` is refused
   with `Error::Stale`, and nothing is written.
   - The cut-off millisecond itself is stale. This corrects plan SC-1,
     which accepted it. An entry accepted at window start `S` expires at
     `S + w`, and from that millisecond on the engine no longer remembers
     it. A retry carrying its original request time `R ≤ S` then has
     `R ≤ start − w`, so "expired" and "stale" are exact complements: an
     honest retry can never be admitted a second time.
   - When a clock rebase recorded a stale floor (below), the cut-off is the
     later of that and the floor.
   - For an expired, unheld entry still in the record, the check runs
     *before* the commit that forgets it. A stale request leaves the entry
     and the clock where they were.
3. **No request time** (`None`) is not checked. An expired key is then
   accepted as new. This is the cost of keeping nothing. A host that needs the
   refusal sends the request time.

`Error::Stale { requested_at_ms, cutoff_ms, clock_ms, now_ms, window_ms,
floor_ms }` is a variant of its own. Hosts match the variant, not the text.
Its `Display` starts with `STALE_REQUEST` (`"stale request: producer time is
older than the replay window"`) and names the request time, the cut-off, the
retention time, the window and the host time (and the floor, if any).

**For the server follow-on.** aq-server's event loop fails the whole quantum
on any engine error except `Overloaded` (`driver.rs:752`, `delivery?`). When
it adopts the `_at` methods, it must treat `Error::Stale` as an outcome of
that one request: retire or hold that outbox row or admission. It must not
fail the quantum, or one stale row would stop every event behind it.
aq-playbook-host's `runner_error` turns every engine error it does not know
into a string (`other => Error::Engine(other.to_string())`), so it needs an
`Error::Stale` arm of its own.

### What a request time must be

The engine trusts the host for the request time, as it does for the key.
For the rule to hold, the host must:

- take it from the same clock as the host time it passes as `now_ms`;
- stamp it once, when the request is created (the outbox row or the
  retained admission), and never re-stamp it on a retry;
- never let it be later than the host time of the request's first
  submission. If the request time is `d` ms ahead, a retry within `d` ms
  after the expiry is admitted as new.

**A request time in the future is accepted** (it is never stale). It is
compared and nothing more: it does not move the clock, does not change any
expiry, and cannot make a remembered key new. It behaves exactly like
`None`. Refusing it would add no protection against a trusted host, since
re-stamping to "now" defeats the past-time check just as well. It would also
make a second outage: a row stamped during a forward clock jump that no
commit carried would be refused until real time caught up.

### Known gaps

- **Raising a window.** The stale window is the *current* policy or
  contract window, while each entry's expiry was fixed with the window that
  applied when it was accepted. After a window is raised, a key accepted
  under the old, shorter window and already forgotten can be retried with
  its original request time and admitted as new, as long as that time is
  within the new window. The gap lasts one new window after the raise.
- **Two-commit race.** Re-admitting an expired, unheld entry takes two
  commits. The stale check runs in both, against each commit's clock. If a
  commit with a later host time lands between them, the second may refuse
  as stale after the first has already forgotten the expired entry. Nothing
  protected is lost: the entry had expired and nothing held it. Only
  concurrent use of one `Engine` can hit this. aq-playbook-host serializes
  on an owner thread.

## A pinned retention clock

Staleness is judged against `max(now, clock)`, the same time expiry is
judged against. The retention clock is the maximum host time any commit has
carried, so one far-future host time `P` pins it. From that commit on:

- **Entries are forgotten early.** Every window entry has expired at `P`
  and drains at 64 per commit, up to a full window early. Retries of those
  keys without a request time (today's callers) are admitted as new.
- **Every request with a real request time is stale**, because the cut-off
  is `P` minus the window. With `_at` callers, event ingestion and keyed
  admission stop. Requests without a request time still go through.
- **The record grows.** What those requests write expires from the pin (an
  event at `P + w`, a keyed receipt at `P + admission_replay_ms`), and so do
  deferrals. `events`, `keyed_admissions` and the expiry order grow with
  traffic until real time reaches `P`. On protected format 2 every commit
  re-encrypts the whole record, so commit cost and write volume grow with
  it. In the end `max_events`, `max_receipts` or `max_state_bytes` refuse
  every admission (`Overloaded` in aq-playbook-host), including those
  without a request time.
- Activation receipts are dated `P`.
- What keeps working: driving existing runs, plain `admit`, inspection,
  history deletion, and duplicates of remembered keys.

### Recognising it

The stale refusal says when the clock is ahead of host time. Example (window
600,000 ms):

```
stale request: producer time is older than the replay window: request time
1700000000000 ms is at or before the cut-off 8999999400000 ms (retention time
9000000000000 ms minus the 600000 ms window; host time 1700000000000 ms); the
retention clock (9000000000000 ms) is ahead of host time by 7300000000000 ms,
and while it is, every request that carries a real request time is refused as
stale (a clock pinned by a far-future host time stays ahead until it is
rebased)
```

The signs are a "retention clock (…) is ahead of host time by …" part that is
much larger than normal clock skew, and that it appears on every request.
`Error::Stale.clock_ms` and `now_ms` carry the two times. The State's
`retention.clock_ms` shows the pinned value.

## If the clock was pinned

Three admin operations, each one licensed commit with its own admin receipt
in history:

- `Engine::rebase_retention_clock_dry_run(to_ms, stale_floor_ms, actor,
  reason, now_ms)` makes the same checks and returns the same report, on a
  snapshot. It commits nothing.
- `Engine::rebase_retention_clock(to_ms, stale_floor_ms, actor, reason,
  now_ms)` is the rebase.
- `Engine::lower_stale_floor(to_ms, actor, reason, now_ms)` undoes a floor
  set too high (see "The stale floor" below).

From the CLI:

```
# 1. Preview: old and new clock, floor, and what would be capped.
aq-playbooks rebase-retention-clock --dry-run --to-ms <corrected real time> \
  --stale-floor-ms <last correct host time before the pin> \
  --actor <who> --reason <why> --database-url … --namespace …
# 2. Commit.
aq-playbooks rebase-retention-clock --confirm-clock-rebase --to-ms … \
  --stale-floor-ms … --actor … --reason … --database-url … --namespace …
```

The CLI passes the machine's own time as `now_ms`.

### Preconditions

A mistaken rebase must not be able to do lasting damage. The rebase is
refused unless all of these hold:

- **The clock is pinned.** It is ahead of `now_ms` by more than the smallest
  policy window (`min(admission_replay_ms, event_replay_ms)`). That is the
  only case in which the pin refuses anything. A clock ahead by less cannot
  be rebased.
- **The target is the corrected real time.** `now_ms <= to_ms <= now_ms +
  slack`, where the slack is one minute, or a tenth of the smallest window
  if that is less. Choose `--to-ms` slightly ahead of every host's clock,
  within the slack. A target below a correct host's time would cap entries
  that host accepted at `A > to_ms` below `A + window`. A target much later
  than real time would itself pin the clock.
- **The floor is in the past:** `stale_floor_ms <= now_ms`. It is the last
  correct clock before the pin.
- An actor and a reason, both present and bounded.

**`now_ms` must be right.** Like every commit, the rebase trusts the host
time it is given, and the fence cannot know real time. A rebase with a wrong
`now_ms` (say 0) is accepted if the clock is far enough ahead of it. It then
moves the clock to that time and caps every window from there, far behind
real time. The engine checks only what it can see without scanning the
record: the raised floor must still be at most the target, so a host time
earlier than an earlier rebase's floor (less the slack) is refused. The rest
is the host's job:

- the server takes `now_ms` from its own clock and cross-checks it, and
  `to_ms`, against PostgreSQL `now()` (see "When aq-server exposes these
  operations");
- the machine running the CLI must have an NTP-synced clock;
- run the dry run first and read the old clock, the new clock and the
  counts.

### A clock ahead by less than the smallest window

That clock cannot be rebased, and it does not need to be. No honest request
is refused as stale, because the cut-off stays behind real time. Entries
expire at most that gap late. Wait it out: as real time passes the bad host
time, the clock is correct again.

Timers registered by commits at the bad host time (waits, joins, retries,
operation deadlines, run durations) fire up to that gap late, which is less
than the smallest window (up to 1 day with the default event window). Cancel
(`aq-playbooks cancel <run>`) or reconcile any run whose late timer matters.

### What the rebase does

1. It sets the retention clock to `to_ms`.
2. It caps every recorded expiry later than its own cap at exactly that
   cap. The cap is `to_ms` plus the admission window for a keyed receipt,
   and `to_ms + max(event_replay_ms, contract.retention_ms)` for an event,
   never the bare policy window when the contract window is longer. An
   expiry at or below its cap stays.
   - What the pin wrote expires in real time again, and no entry is
     remembered for less than its own window from `to_ms`.
   - Held entries stay, and are deferred again in the normal way.
   - Entries the pin had not yet drained become unexpired. They are
     remembered longer, which is harmless.
3. It caps every live run's timer that lies beyond `to_ms` plus its
   declared duration at that value:
   - a wait (`Wait.deadline_ms` of its step);
   - a join (`Join.deadline_ms`);
   - a retry (`RetryPolicy.backoff_ms`);
   - an operation deadline (`command.deadline_ms`, stored with the
     operation);
   - the run's duration basis: `admitted_at_ms` is lowered to `to_ms`, so
     `limits.max_duration_ms` counts from there.

   Each capped value is still at least its real registration plus its
   duration, because the registration was at or before `to_ms`. A wait set
   at the pinned time then times out normally, and the run settles and
   leaves the record. Every kind's duration is in the artifact or the
   operation, so a consistent State never needs a guess. A deadline after
   `to_ms` whose duration cannot be found (an inconsistent State, such as a
   retry on a step without a retry policy) is left as it is. Its run is
   listed in `needs_operator_action`: cancel it (`aq-playbooks cancel
   <run>`), or reconcile its operation.
4. It raises the stale floor to the later of the existing floor and
   `stale_floor_ms`.
5. It writes a `retention_clock_rebased` admin receipt. The receipt holds
   `from_ms`, `to_ms`, `stale_floor_ms`, the capped counts, `adjusted_runs`,
   `needs_operator_action`, `actor`, `reason`, and the host time as `at_ms`.
   For audit it also lists each capped timer (`adjustments`: run, kind,
   item, original and capped value), at most 1,000 of them; the rest are
   counted in `adjustments_not_listed`. That is how a run's history row,
   which shows `admitted_at_ms = to_ms` next to receipts dated at the bad
   time, can be traced back. The report returns the same.

The commit does nothing else. It carries no host time, so nothing settles
and nothing is dropped in it. Its cost is one pass over the expiry order
above the smallest cap, plus one pass over the live runs and operations.  It
is at most one record's worth (the remembered window is at most
`replay_keep` unheld entries plus those live work holds), the same order as
any protected format 2 commit.

### The fences

Without a licence, the Task 4 fences are unchanged: the clock never
decreases, a recorded expiry only moves later, and the stale floor never
changes, not even upwards.

A commit that carries a licence receipt (`retention_clock_rebased` or
`stale_floor_lowered`) in its own history must be exactly that licence.
Otherwise it is refused: `REFUSE_LICENCE`, or `REFUSE_EXPIRY` for a wrong cap and `REFUSE_STALE_FLOOR` for a wrong floor. This is checked by
`retention::verify_removals`, used by the in-memory store, the record
store's full commit, and PostgreSQL protected and plain format 2 and format
3. The record store's patch path checks the same rules. A format 3
transition that carries a licence takes the complete path.

- **The receipt is new.** Its sequence is after the stored sequence. A
  receipt cannot be replayed, even after `delete_history_before` removed its
  history row. At most one licence per commit.
- **A rebase** lowers the clock from exactly the receipt's `from_ms` (the
  stored clock) to exactly its `to_ms`, with `to_ms < from_ms`. It may lower
  a recorded expiry only to exactly its cap (above). It may only raise the
  floor, to at most `to_ms`.
- **A floor lowering** leaves the clock alone. It lowers the floor from
  exactly the receipt's `from_ms` (the stored floor) to exactly its `to_ms`,
  and changes no expiry.
- **Neither** adds or removes a run, operation, event or keyed receipt, or
  removes an audit receipt.
- **Neither** changes any header field its licence does not name: the
  retention policy, producer epochs, GC state, artifact disposals,
  artifacts, bindings, the catalog revision and history, execution denials,
  authority revocations or recovery requests.
- **Caps are computed from the stored state.** The policy, the installed
  contracts and the stored event are those before the commit, so a licensed
  commit cannot shrink a cap by changing the policy, a contract, or an
  event's contract reference.

So a receipt for another clock or floor, a receipt left in the live State,
a replayed receipt, or a licensed commit that also does other work is
refused. Run timers are not fenced, by any commit, as before.

### The stale floor

**What it guarantees.** After a rebase, a request for a key the engine does
not remember whose request time is at or before the floor is stale. The
floor never changes what happens to a key the engine still remembers. Once
`max(now, clock)` reaches `floor + window`, the ordinary cut-off is later
than the floor anyway. The floor must be in the past, so its extra effect
lasts at most one window.

- If the floor is at or after the last correct clock before the pin, every
  key the pin forgot early is covered. Their honest retries carry the
  request time of the original request, which was at or before that clock,
  so they are refused as stale, not admitted a second time.
- It cannot protect:
  - **retries with no request time.** A key forgotten during the pin and
    retried without a request time is accepted as new, as the spec already
    accepts for any expired key;
  - **keys whose request time is after the floor.** If the floor is set
    earlier than the pin started, keys forgotten early with request times
    between the floor and the pin can still be admitted again.
- **It also refuses new requests created at or before it** that the engine
  never admitted. Requests stamped after the floor that the pin refused
  are accepted after the rebase. Set the floor as late as covers the pin,
  and no later.
- Finding the last correct clock: the host's logs, or the newest history
  item or receipt dated before the jump (the first item dated far in the
  future is the jump).

**A floor set too high** (by a mistaken rebase) can be lowered:
`aq-playbooks lower-stale-floor --to-ms <value> --actor … --reason …
--confirm-floor-lowering`, or `Engine::lower_stale_floor`. It accepts that
keys forgotten after that time may be re-admitted as new by a retry with a
request time in between. The floor goes down only to the value the operator
states, in a commit of its own with a `stale_floor_lowered` receipt.

### When aq-server exposes these operations

The engine does no authority check. `actor` and `reason` are free text. The
server must:

- allow the operations only to an operator or admin role;
- set `actor` from the authenticated identity, not from the request;
- pass its own clock as `now_ms`;
- check `to_ms` (and the floor) against PostgreSQL `now()` before calling,
  and refuse when they disagree beyond the slack;
- require explicit confirmation, and offer the dry run first.

## No namespace limits

Owner direction (spec 0003, final section): there are no namespace limits.
The record holds work in progress. Finished work is deleted when it
finishes. Remembered keys and history are rolling sets of fixed size
(`replay_keep` and `history_keep`).

Nothing is ever refused because a namespace holds too much: no admission
(direct, keyed, or an event's fan-out), event, step, child start,
completion, wait, timer, cancel, settlement or admin commit. What bounds the
record is deletion alone.

Removed in Task 8b (the mechanism Task 8 built, and everything that served
only it):

- the quota check (`check_quota`, `over_quota`, `quota_counts`,
  `QuotaCounts`, `QuotaGate`, `quota_ceiling`), the `limit + limit / 8`
  ceiling, the commit classes (`QuotaMarks`, `State::quota_marks`) and the
  quota refusal texts;
- `TransitionRequest::validate` (the format 3 path's quota callback);
- the maintained byte and queued-destination sums of the format 3
  accounting (`Accounting` keeps each container's record count, which a
  bound container's `len` reads, and the header length). A PostgreSQL
  format 3 row written before stores `[count, bytes]` per container and a
  queued-destination count; both decode, and the extra numbers are ignored;
- the `with_config` refusal of zero or small limits
  ("invalid namespace resource bounds").

**Deprecated, accepted and ignored**, so stored states decode and existing
hosts compile:

- `EngineConfig::{max_runs, max_events, max_queued_destinations,
  max_state_bytes, max_receipts}`. `EngineConfig` decodes with any of them
  missing (`#[serde(default)]`), and `Engine::with_config` keeps the value
  unread;
- `RetentionPolicy::{max_tombstones, terminal_run_ms}`. A stored policy
  decodes with or without them (each has a serde default) and behaves the
  same either way (tested: `deprecated_limit_fields_decode_and_change_nothing`).

`set_retention_policy` refuses only a zero window (`event_replay_ms`,
`admission_replay_ms` or `audit_ms`): "retention windows must be positive".
GC no longer refuses for the event tombstone or correlation fence count,
and the age of a finished root it may collect is `audit_ms` alone.
PostgreSQL does not count `disposal_tombstones` rows on a full commit; its
cutoff deletes over those rows (`collect`) run only in a commit that moves
the GC clock. Rows written before this change are drained by Task 9 and
never refuse a commit.

**What stays, because it is not a namespace limit:** the limits a playbook
declares for itself in its artifact (step visits, children, depth,
iterations, duration, concurrent effects and children, payload and history
bytes), which bound one run and are part of the playbook's meaning; input
validation bounds (payload sizes, key and text lengths); the bounded amount
of deletion work per commit (64 window drops, 64 evictions, 50 cohorts, the
history trim batch); and the removal fences, which are about the
correctness of deletion, not capacity.

**A namespace written before this change** still holds the runs that
finished before the upgrade until the drain moves them out (Task 9). They
never make new work wait (tested:
`a_legacy_namespace_holding_finished_runs_admits_new_work_and_drains`).

## PostgreSQL: on-disk size of the namespace table

On protected format 2 every transition rewrites the namespace's one wide,
encrypted row. Ciphertext does not compress, so each commit writes a new
TOAST value the size of the record and leaves the old one dead. TOAST space
is reclaimed only by vacuum, which makes it reusable without shrinking the
file. The on-disk size of `aq_playbooks.namespaces` is therefore a plateau
of roughly

```text
commit rate × record size × (autovacuum naptime + vacuum time)
```

and does not grow with the number of runs. At benchmark pace (about 1,500
commits a minute, a 124 KB record) that is a few hundred MB; at aq-server's
pace (tens of commits a minute) a few MB.

**Optional operator tuning** (no migration sets it; it needs ownership of
the table):

```sql
ALTER TABLE aq_playbooks.namespaces SET (
    autovacuum_vacuum_cost_delay = 0,
    toast.autovacuum_vacuum_cost_delay = 0
);
```

It makes each vacuum pass of that table finish in one go, which shortens
only the vacuum-time term. **It did not measurably change the size.** At
200 runs (10 s per run, 10-minute windows, protected format 2, default
autovacuum with a one-minute naptime) the table was 224.4 MB with the
setting, against 201.8 MB without it (one run each; 170.5 MB against
145–152 MB after the next vacuum pass). Autovacuum made one pass per naptime
either way: the naptime dominates. `autovacuum_naptime` is server-wide and
left to the operator.

The structural remedy on record is to compress the snapshot before
encrypting it on protected format 2. That shrinks the record bytes, the
write volume per transition and this plateau together.
