//! Validate a pinned invocation and initialize its durable run and shared budget.
use super::*;

pub(super) struct Invocation<'a> {
    pub(super) artifact: &'a str,
    pub(super) entry: &'a str,
    pub(super) input: Value,
    pub(super) authority: Authority,
    pub(super) dependencies: BTreeMap<String, String>,
}

pub(super) struct Lineage<'a> {
    pub(super) id: &'a str,
    pub(super) root: &'a str,
    pub(super) parent: Option<String>,
    pub(super) origin: Option<String>,
    pub(super) depth: u32,
}

impl<'a> Lineage<'a> {
    // Independently running children still draw from their originating root's budget.
    pub(super) fn child(run: &'a Run, id: &'a str, mode: &CallMode) -> Self {
        Self {
            id,
            root: &run.root,
            parent: (*mode != CallMode::Independent).then(|| run.id.clone()),
            origin: Some(run.id.clone()),
            depth: run.depth + 1,
        }
    }
}

struct EntryPoint {
    step: String,
    catalog_revision: u64,
}

impl Invocation<'_> {
    pub(super) fn admit_root(self, s: &mut State, id: &str, now: i64) -> Result<()> {
        self.admit(
            s,
            Lineage {
                id,
                root: id,
                parent: None,
                origin: None,
                depth: 0,
            },
            now,
        )
    }

    pub(super) fn admit(self, s: &mut State, lineage: Lineage<'_>, now: i64) -> Result<()> {
        let id = lineage.id;
        if s.runs.contains_key(id) {
            return Ok(());
        }
        let revision = s
            .runs
            .get(lineage.root)
            .map_or(s.catalog_revision, |root| root.catalog_revision);
        let step = self.start_step(s, revision)?;
        let audit = json!({"artifact": self.artifact});
        let root = lineage.root;
        let run = self.into_run(
            lineage,
            EntryPoint {
                step: step.clone(),
                catalog_revision: revision,
            },
            now,
        );
        s.runs.insert(id.into(), run);
        s.budgets.entry(root.into()).or_default();
        indexes::mark_run(s, id);
        receipt(s, id, &step, now, "admitted", audit);
        Ok(())
    }

    fn start_step(&self, state: &State, revision: u64) -> Result<String> {
        ensure_admission_protection(state, self.artifact, &self.dependencies, revision)?;
        let artifact = state
            .artifacts
            .get(self.artifact)
            .ok_or_else(|| Error::NotFound(self.artifact.into()))?;
        check_authority(artifact, &self.authority)?;
        let playbook = &artifact.content.playbook;
        let entry = playbook
            .entries
            .get(self.entry)
            .ok_or_else(|| Error::NotFound(self.entry.into()))?;
        entry.input.validate(&self.input)?;
        check_bytes(&self.input, playbook.limits.max_payload_bytes)?;
        Ok(entry.start.clone())
    }

    fn into_run(self, lineage: Lineage<'_>, entry: EntryPoint, now: i64) -> Run {
        let EntryPoint {
            step,
            catalog_revision,
        } = entry;
        Run {
            catalog_revision,
            id: lineage.id.into(),
            root: lineage.root.into(),
            parent: lineage.parent,
            origin: lineage.origin,
            artifact: self.artifact.into(),
            entry: self.entry.into(),
            dependencies: self.dependencies,
            authority: self.authority,
            step,
            status: RunStatus::Admitted,
            input: self.input,
            variables: BTreeMap::new(),
            outcome: None,
            output: None,
            reason: None,
            admitted_at_ms: now,
            visits: 0,
            depth: lineage.depth,
            children: vec![],
            pending: None,
            loops: vec![],
            attempts: BTreeMap::new(),
            pending_return: None,
            pending_failure: false,
            terminal_sequence: None,
        }
    }
}
