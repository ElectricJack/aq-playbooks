use super::*;

pub(super) struct Pilot {
    pub(super) engine: Engine<MemoryStorage>,
    pub(super) host: FakeHost,
    pub(super) root: String,
}

#[derive(Clone, Copy)]
pub(super) struct Replies {
    checks: bool,
    judgment: bool,
}
impl Replies {
    pub(super) const MANUAL: Self = Self {
        checks: false,
        judgment: false,
    };
    pub(super) const CHECKS: Self = Self {
        checks: true,
        judgment: false,
    };
}

pub(super) fn setup(name: &str) -> Pilot {
    let scenario = pilots::scenario(name).unwrap();
    let engine = Engine::new(
        MemoryStorage::retaining_settled(),
        format!("host-{name}"),
        "runtime-1",
    )
    .unwrap();
    for artifact in &scenario.artifacts {
        engine.install(artifact.clone()).unwrap();
    }
    for (binding, digest) in &scenario.bindings {
        engine
            .activate(binding, digest, scenario.authority.clone())
            .unwrap();
    }
    let start = &scenario.starts[0];
    let root = engine
        .admit(
            &start.binding,
            &start.entry,
            start.input.clone(),
            start.authority.clone(),
            0,
        )
        .unwrap();
    let mut host = FakeHost::default();
    host.grant_all("pilot-user", &["project"]);
    host.visibility
        .insert("supervisor".into(), BTreeSet::from(["project".into()]));
    host.subjects.insert("candidate-1".into(), 1);
    host.subjects.insert("main".into(), 1);
    let view = match name {
        "watcher" => "health_observations",
        _ => "project",
    };
    host.capture_observation(Observation {
        reference: "snapshot-1".into(),
        partition: "project-health".into(),
        scope: "project".into(),
        visibility: "project".into(),
        view: view.into(),
        subject: Subject {
            id: "candidate-1".into(),
            revision: 1,
        },
        pages: vec![json!({"synthetic":true})],
        complete: true,
        observed_at_ms: 0,
        expires_at_ms: 60_000,
        episode_revisions: BTreeMap::new(),
    })
    .unwrap();
    Pilot { engine, host, root }
}

/// Explicitly configured fake checks and judgments. Async work remains pending
/// until each test supplies an authoritative domain completion below.
impl Pilot {
    pub(super) fn drive(&mut self, now: i64, replies: Replies) {
        self.drive_with_hook(now, replies, |_, _| {});
    }

    pub(super) fn drive_with_hook(
        &mut self,
        now: i64,
        replies: Replies,
        mut before_dispatch: impl FnMut(&Engine<MemoryStorage>, &mut FakeHost),
    ) {
        self.host.now_ms = now as u64;
        for _ in 0..300 {
            let advanced = self.advance_runs(now);
            self.prepare_replies(replies);
            before_dispatch(&self.engine, &mut self.host);
            let dispatched = self
                .engine
                .dispatch_next(&mut self.host, now)
                .unwrap()
                .is_some();
            if !advanced && !dispatched {
                return;
            }
        }
        panic!("pilot exceeded integration transition bound");
    }
    fn advance_runs(&self, now: i64) -> bool {
        let mut changed = false;
        let ids: Vec<_> = self
            .engine
            .snapshot()
            .unwrap()
            .runs
            .keys()
            .cloned()
            .collect();
        for id in ids {
            // An earlier advance in this pass may have settled the cohort
            // of `id`, which then left the State.
            match self.engine.advance(&id, now) {
                Ok(progress) => changed |= progress,
                Err(Error::NotFound(_)) => {}
                Err(error) => panic!("advance {id}: {error}"),
            }
        }
        changed
    }
    fn prepare_replies(&mut self, replies: Replies) {
        for intent in self
            .engine
            .snapshot()
            .unwrap()
            .intents
            .values()
            .filter(|i| i.status == IntentStatus::Prepared)
        {
            if replies.checks && intent.command.name == "check.request" {
                let subject: Subject =
                    serde_json::from_value(intent.input["subject"].clone()).unwrap();
                self.host.configured_outcomes.insert(
                    FakeHost::check_key(&subject, intent.input["suite"].as_str().unwrap()),
                    "passed".into(),
                );
            }
            if replies.judgment && intent.command.name == "judgment.request" {
                self.host
                    .configured_outcomes
                    .insert(intent.id.clone(), "assessed".into());
                self.host
                    .configured_choices
                    .insert(intent.id.clone(), "permitted-default".into());
            }
        }
    }
}

/// A finished root's run as its cohort left the State (asserting it left).
pub(super) fn settled(engine: &Engine<MemoryStorage>, id: &str) -> Run {
    assert!(
        !engine.snapshot().unwrap().runs.contains_key(id),
        "{id} is live"
    );
    engine.store.settled_run(&engine.namespace, id).unwrap()
}
/// Every run and operation that has left the State with a settled cohort.
pub(super) fn settled_items(engine: &Engine<MemoryStorage>) -> (Vec<Run>, Vec<Intent>) {
    let cohorts = engine.store.settled_cohorts(&engine.namespace);
    (
        cohorts.iter().flat_map(|c| c.runs.clone()).collect(),
        cohorts.iter().flat_map(|c| c.intents.clone()).collect(),
    )
}

impl Pilot {
    pub(super) fn complete(&mut self, kind: &str, disposition: &str, now: i64) {
        let work = self
            .host
            .work
            .values()
            .find(|work| work.kind == kind && work.completion.is_none())
            .unwrap()
            .clone();
        self.host.now_ms = now as u64;
        let event = self
            .host
            .complete_work(
                &work.reference,
                disposition,
                &format!("domain-result:{}", work.reference),
            )
            .unwrap();
        self.engine
            .admit_event("aq.host.bridge@1", event.clone(), now)
            .unwrap();
        assert!(
            self.engine
                .admit_event("aq.host.bridge@1", event, now)
                .unwrap()
                .duplicate
        );
    }
}
