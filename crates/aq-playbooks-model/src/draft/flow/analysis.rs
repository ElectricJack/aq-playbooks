//! Combine scope counts and the diagnostics of every reachable expression.
use super::*;

impl<'a> Check<'a> {
    /// The derivation over the whole draft.
    pub(in crate::draft) fn derived(&self) -> Derived {
        let playbook = &self.content.playbook;
        let parts: BTreeMap<&str, Parts> = playbook
            .steps
            .iter()
            .map(|(name, step)| (name.as_str(), self.content.parts(self.sources, name, step)))
            .collect();
        let failures = self.loop_failures(&parts);
        let mut analysis = DraftAnalysis {
            pass: Pass::new(self),
            derived: Derived::default(),
            sizes: BTreeMap::new(),
        };
        for (entry_name, entry) in &playbook.entries {
            let contexts = EntryFlow {
                entry_name,
                entry,
                parts: &parts,
                failures: &failures,
                check: self,
                frozen: BTreeSet::new(),
                limits: &mut analysis.derived.limits,
            }
            .contexts();
            analysis.scope((entry_name, entry), &contexts, &parts);
            analysis.steps((entry_name, entry), &contexts, &parts);
        }
        analysis.triggers(playbook);
        let DraftAnalysis {
            mut derived, sizes, ..
        } = analysis;
        derived.sizes = sizes.into_values().collect();
        if self.work.get() > MAX_SHAPE_WORK {
            issue(
                &mut derived.limits,
                "draft_limit",
                "playbook",
                format!(
                    "the draft reads values that can have many different shapes too often to \
                     check: more than {MAX_SHAPE_WORK} shapes to follow in all. A literal \
                     table counts one shape for each row, whatever its fields, and a lookup one \
                     for each result; each read through a name or an item follows all of them. \
                     Read the table in fewer places (bind what you need from it once with \
                     `let`), or use a smaller table."
                ),
            );
        }
        derived
    }
}
struct DraftAnalysis<'a, 'k> {
    pass: Pass<'a, 'k>,
    derived: Derived,
    sizes: BTreeMap<(String, String), ExprSize>,
}
impl<'a> DraftAnalysis<'a, '_> {
    fn scope(
        &mut self,
        (entry_name, entry): (&'a str, &'a EntryPoint),
        contexts: &BTreeMap<&'a str, Context<'a>>,
        parts: &BTreeMap<&'a str, Parts<'a>>,
    ) {
        let check = self.pass.check;
        // Ordinary typing holds, at every step, the type of `input`, of
        // the definitions and of every binding and item, in every form
        // it has anywhere.
        let mut anywhere: BTreeMap<&str, Build> = BTreeMap::new();
        for context in contexts.values() {
            for (name, carried) in context {
                anywhere.entry(name).or_default().all(carried);
            }
        }
        let fixed = check.nodes(&entry.input).saturating_add(
            check
                .sources
                .definitions
                .as_ref()
                .map_or(0, |d| check.nodes(d)),
        );
        let in_scope = anywhere.into_values().fold(fixed, |n, forms| {
            n.saturating_add(check.finish(forms).nodes)
        });
        let reached = contexts
            .keys()
            .filter(|name| parts.contains_key(*name))
            .count();
        self.derived
            .scope
            .push((entry_name.to_string(), reached as u64, in_scope));
    }
    fn steps(
        &mut self,
        (entry_name, entry): (&'a str, &'a EntryPoint),
        contexts: &BTreeMap<&'a str, Context<'a>>,
        parts: &BTreeMap<&'a str, Parts<'a>>,
    ) {
        let check = self.pass.check;
        for (name, context) in contexts {
            let Some(p) = parts.get(name) else {
                continue;
            };
            let path = format!("steps.{name}");
            let mut entered;
            let context = match p.own_item {
                Some(item) => {
                    entered = context.clone();
                    entered.insert(item, Rc::new(check.entering(name, item)));
                    &entered
                }
                None => context,
            };
            let place = Place::Step {
                entry_name,
                entry,
                context,
            };
            for (at, expr) in &p.exprs {
                let (reads, sized) = self.pass.evaluate((&path, at), expr, place);
                self.record(&path, reads, &sized);
                let key = (path.clone(), at.clone());
                if self.sizes.get(&key).is_none_or(|old| old.size < sized.size) {
                    self.sizes.insert(key, sized);
                }
            }
        }
    }
    fn triggers(&mut self, playbook: &'a Playbook) {
        for t in &playbook.triggers {
            let path = format!("triggers.{}", t.id);
            let payload = self
                .pass
                .check
                .content
                .registry
                .events
                .get(&t.event.name)
                .map(|e| &e.payload);
            let (reads, sized) =
                self.pass
                    .evaluate((&path, "input"), &t.input, Place::Trigger { payload });
            self.record(&path, reads, &sized);
            self.sizes.insert((path, "input".into()), sized);
        }
    }
    fn record(&mut self, path: &str, reads: Vec<(String, String)>, sized: &ExprSize) {
        self.derived
            .reads
            .extend(reads.into_iter().map(|(at, m)| (path.to_string(), at, m)));
        self.derived.total = self.derived.total.saturating_add(sized.size);
    }
}
