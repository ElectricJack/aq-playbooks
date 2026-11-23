use super::*;

#[derive(Default)]
pub(super) struct Destinations {
    starts: Vec<Destination>,
    blockers: Vec<(String, &'static str, String)>,
}

impl Destinations {
    pub(super) fn collect(
        s: &State,
        key: &str,
        event: &EventEnvelope,
        target: Option<&str>,
    ) -> Result<Self> {
        let mut plan = Plan {
            key,
            payload: &event.payload,
            destinations: Self::default(),
        };
        let candidates = s
            .indexes
            .triggers
            .get(&indexes::trigger_key(&event.contract, &event.scope)?)
            .cloned()
            .unwrap_or_default();
        for binding in candidates
            .into_iter()
            .filter(|b| target.is_none_or(|target| b == target))
        {
            let activation = &s.bindings[&binding];
            if activation.authority.scope != event.scope {
                continue;
            }
            let artifact = &s.artifacts[&activation.artifact];
            let route = Route {
                binding: &binding,
                activation,
                artifact,
            };
            for trigger in artifact
                .content
                .playbook
                .triggers
                .iter()
                .filter(|trigger| trigger.event == event.contract)
            {
                plan.register(s, &route, trigger)?;
            }
        }
        Ok(plan.destinations)
    }
    pub(super) fn record_blockers(self, s: &mut State, key: &str, now_ms: i64) -> Vec<Destination> {
        for (destination, blocked_kind, reason) in self.blockers {
            receipt(
                s,
                &format!("event:{key}"),
                "admission",
                now_ms,
                "destination_blocked",
                json!({"destination":destination,"blocked_kind":blocked_kind,"reason":reason}),
            );
        }
        self.starts
    }
}

struct Plan<'a> {
    key: &'a str,
    payload: &'a Value,
    destinations: Destinations,
}

struct Route<'a> {
    binding: &'a str,
    activation: &'a Activation,
    artifact: &'a Artifact,
}

impl Plan<'_> {
    fn register(&mut self, s: &State, route: &Route<'_>, trigger: &Trigger) -> Result<()> {
        // The first sequence number this acceptance assigns (the
        // event's own, or its first blocked-destination
        // receipt's) keeps the run ids of an event accepted again
        // after its window apart (SC-7). Stored destinations keep
        // their keys, so existing run ids are unchanged.
        let destination_key = digest(&(
            &self.key,
            s.sequence + 1,
            &route.binding,
            route.activation.revision,
            &trigger.id,
        ))?;
        let input = match self.input(route.artifact, trigger) {
            Ok(input) => input,
            Err(error) => {
                self.destinations.blockers.push((
                    destination_key.clone(),
                    "input",
                    error.to_string(),
                ));
                Value::Null
            }
        };
        let deps = match resolve(s, &route.artifact.digest, &route.activation.authority) {
            Ok(deps) => deps,
            Err(error) => {
                self.destinations.blockers.push((
                    destination_key.clone(),
                    "dependency",
                    error.to_string(),
                ));
                BTreeMap::new()
            }
        };
        ensure_admission_protection(s, &route.artifact.digest, &deps, s.catalog_revision)?;
        self.destinations.starts.push(Destination {
            key: destination_key,
            artifact: route.artifact.digest.clone(),
            entry: trigger.entry.clone(),
            input,
            authority: route.activation.authority.clone(),
            dependencies: deps,
            run: None,
        });
        Ok(())
    }

    fn input(&self, a: &Artifact, trigger: &Trigger) -> Result<Value> {
        let mut ctx = EvalContext::new();
        ctx.insert("event".into(), EvalValue::Known(self.payload.clone()));
        let input = known_expr(&trigger.input, &ctx)?;
        a.content.playbook.entries[&trigger.entry]
            .input
            .validate(&input)?;
        check_bytes(&input, a.content.playbook.limits.max_payload_bytes)?;
        Ok(input)
    }
}

pub(crate) fn fanout_event(
    s: &mut State,
    event_key: &str,
    limit: usize,
    now_ms: i64,
) -> Result<usize> {
    let mut event = s
        .events
        .get(event_key)
        .ok_or_else(|| Error::NotFound(event_key.into()))?
        .clone();
    let mut count = 0;
    for d in event.destinations.iter_mut().filter(|d| d.run.is_none()) {
        if count >= limit {
            break;
        }
        // An unresolved input belongs to the frozen event/proposal. A
        // later activation cannot repair or reinterpret it.
        if s.receipts.iter().any(|receipt| {
            receipt.kind == "destination_blocked"
                && receipt.detail.get("destination").and_then(Value::as_str) == Some(&d.key)
                && receipt.detail.get("blocked_kind").and_then(Value::as_str) == Some("input")
        }) {
            continue;
        }
        // Missing immutable bytes may be installed later. Resolution
        // still uses the accepted exact artifact, never current bindings.
        if d.dependencies.is_empty() {
            let Ok(dependencies) = resolve_at(s, &d.artifact, &d.authority, event.catalog_revision)
            else {
                continue;
            };
            d.dependencies = dependencies;
        }
        let id = format!("event-{}", d.key);
        admit_run(
            s,
            id.clone(),
            id.clone(),
            None,
            None,
            &d.artifact,
            &d.entry,
            d.input.clone(),
            d.authority.clone(),
            d.dependencies.clone(),
            0,
            now_ms,
        )?;
        s.runs.get_mut(&id).unwrap().catalog_revision = event.catalog_revision;
        d.run = Some(id);
        count += 1;
    }
    s.events.insert(event_key.into(), event);
    s.replay_touched.events.insert(event_key.into());
    Ok(count)
}
