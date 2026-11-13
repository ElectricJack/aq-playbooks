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

`Engine::rebase_retention_clock(to_ms, stale_floor_ms, actor, reason,
now_ms)`, or the CLI:

```
aq-playbooks rebase-retention-clock --to-ms <corrected real time> \
  --stale-floor-ms <last correct host time before the pin> \
  --actor <who> --reason <why> --confirm-clock-rebase \
  --database-url … --namespace …
```

It is one commit. It is refused unless `now_ms ≤ to_ms < clock` and
`stale_floor_ms ≤ to_ms`, and it needs an actor and a reason. The CLI passes
the machine's time as `now_ms`, so `--to-ms` must not be in its past. The
commit:

1. sets the retention clock to `to_ms`;
2. caps every recorded expiry later than `to_ms` plus its own window (the
   admission window for a keyed receipt, `max(event_replay_ms,
   contract.retention_ms)` for an event) at exactly that. An expiry at or
   below its cap stays. So what the pin wrote expires in real time again,
   and no entry is remembered for less than its window from `to_ms`. Held
   entries (a receipt whose run is live, an event a wait or destination
   needs) stay, and are deferred again in the normal way if still held at
   their new expiry. Entries the pin had not yet drained become unexpired:
   they are remembered longer, which is harmless;
3. records the stale floor: the later of the existing floor and
   `stale_floor_ms`;
4. writes an admin receipt (`retention_clock_rebased`) that goes to history
   in the same commit. It holds `from_ms`, `to_ms`, `stale_floor_ms`, the
   capped counts, `actor`, `reason`, and the commit's host time as `at_ms`.

The cost is one pass over the expiry order above `to_ms` plus the smallest
window. That is the entries the pin wrote, all in this one commit.

**The fences.** The Task 4 fences (the clock never decreases; a recorded
expiry only moves later) open only for a commit that writes this receipt to
history. The shared check (`retention::verify_removals`, used by the
in-memory store, the record store's full commit and patch, and PostgreSQL
protected and plain format 2 and format 3) allows:

- the clock to go from exactly the receipt's `from_ms` to exactly its
  `to_ms`;
- a recorded expiry to be lowered, but not below `to_ms` plus the policy
  window of its kind, and only if the clock took the rebase.

A format 3 transition that carries the receipt takes the complete path,
where that check runs. Without the receipt, or with a receipt left in the
live State, or one for another clock, the fences stay closed. History rows
are inserted once, so a receipt licenses only the commit that writes it. The
stale floor never decreases and is never removed (`REFUSE_STALE_FLOOR`).

**What the stale floor guarantees.** After the rebase, a request for a key
the engine does not remember is stale if its request time is at or before
the floor. The floor never changes what happens to a key the engine still
remembers. Once `max(now, clock)` reaches `floor + window`, the ordinary
cut-off is later than the floor anyway.

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
  never admitted. Set it as late as covers the pin, and no later. Requests
  created *during* the pin (after the floor) that were refused as stale are
  accepted after the rebase. The safest choice when the start of the pin is
  unknown is `--stale-floor-ms` equal to `--to-ms`, but then every request
  created before the rebase that the engine never admitted is refused for
  good.
- Finding the last correct clock: the host's logs, or the newest history
  item or receipt dated before the jump (the first item dated far in the
  future is the jump).

The rebase does not touch run timers, wait deadlines, or the `admitted_at`
and `accepted_at` times that were written at `P`. Waits and operation
deadlines set from `P` still fire at `P` + their deadline.
