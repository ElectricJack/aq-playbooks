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
   request with `requested_at_ms < max(now, clock) − window` is refused
   with `Error::Refused`, and nothing is written. The message starts with
   `STALE_REQUEST` (`"stale request: producer time is older than the replay
   window"`) and names the request time, the cut-off, the retention time,
   the window and the host time.
   - For an expired, unheld entry still in the record, the check runs
     *before* the commit that forgets it. A stale request leaves the entry
     and the clock where they were.
   - A request time exactly at the cut-off is accepted.
3. **No request time** (`None`) is not checked. An expired key is then
   accepted as new. This is the cost of keeping nothing. A host that needs the
   refusal sends the request time.

A request time in the future is never stale. It is compared and nothing
more: it does not move the clock, does not change any expiry, and cannot
make a remembered key new. The producer (the host) is trusted for the
request time, as it is for the key itself.

### A pinned retention clock

Staleness is judged against `max(now, clock)`, the same time expiry is
judged against. The retention clock is the maximum host time any commit has
carried, so one far-future host time pins it. While it is pinned:

- every entry expires and drains at 64 per commit, and new entries count
  from the pinned clock, so they do not expire in real time;
- **every request that carries a real request time is refused as stale**,
  because the cut-off is the pinned clock minus the window.

The refusal says so. When the retention clock is ahead of the host time, the
message adds the clock, how far it is ahead, and that every request with a
real request time is refused while it stays ahead. Example:

```
stale request: producer time is older than the replay window: request time
1700000000000 ms is before 8999999400000 ms (retention time 9000000000000 ms
minus the 600000 ms window; host time 1700000000000 ms); the retention clock
(9000000000000 ms) is ahead of host time by 7300000000000 ms, and while it
is, every request that carries a real request time is refused as stale (a
clock pinned by a far-future host time stays ahead)
```

Requests without a request time (`None`, including every call through the
old methods) still go through. The engine has no way to move the clock back:
`REFUSE_CLOCK` forbids it. An operator who sees this message should find the
bad host time and treat the namespace's clock as an incident.
