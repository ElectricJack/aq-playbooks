//! Propagate bindings through normal and failed edges until every reachable context stabilizes.
use super::*;
mod analysis;
mod loops;

fn merge<'a>(check: &Check<'a>, into: &mut Context<'a>, from: &Context<'a>) -> bool {
    let mut changed = false;
    for (name, carried) in from {
        match into.get_mut(name) {
            None => {
                into.insert(name, carried.clone());
                changed = true;
            }
            Some(existing) => {
                if Rc::ptr_eq(existing, carried) {
                    continue;
                }
                let mut both = Build::default();
                both.all(existing);
                both.all(carried);
                if both.forms.len() > existing.forms.len()
                    || both.lacks.len() > existing.lacks.len()
                {
                    *existing = Rc::new(check.finish(both));
                    changed = true;
                }
            }
        }
    }
    changed
}

struct EntryFlow<'a, 'q> {
    check: &'q Check<'a>,
    entry_name: &'a str,
    entry: &'a EntryPoint,
    parts: &'q BTreeMap<&'a str, Parts<'a>>,
    failures: &'q LoopFailures<'a>,
    frozen: BTreeSet<&'a str>,
    limits: &'q mut Vec<ValidationIssue>,
}
impl<'a> EntryFlow<'a, '_> {
    fn start(&self) -> Context<'a> {
        let entry_name = self.entry_name;
        let mut start = Context::new();
        for (name, p) in self.parts {
            for (bind, _) in p.exits.iter().filter_map(|exit| exit.holds.as_ref()) {
                let way = format!(
                    "the way from the start of entry `{entry_name}` that does not pass step `{name}`"
                );
                start.insert(
                    bind,
                    Rc::new(
                        self.check
                            .lacking(&format!("on {way}, `{bind}` is not bound")),
                    ),
                );
            }
        }
        start
    }

    fn item(&mut self, name: &'a str, collection: &'a Expr, current: &Context<'a>) -> Carried<'a> {
        if self.frozen.contains(name) {
            return self.check.opaque(1);
        }
        let mut pass = Pass::new(self.check);
        let place = Place::Step {
            entry_name: self.entry_name,
            entry: self.entry,
            context: current,
        };
        let out = pass.derive(
            collection,
            Site {
                at: "collection",
                scope: &Scope::new(),
                place,
            },
        );
        let item = self.check.items_of(&out.carried);
        if item.depth > MAX_ITEM_DEPTH {
            self.frozen.insert(name);
            issue(
                self.limits,
                "draft_limit",
                &format!("steps.{name}"),
                format!(
                    "steps.{name}.collection: the items nest more than \
                                     {MAX_ITEM_DEPTH} levels deep. A `for_each` collection may \
                                     not be built from the items of a loop that is itself built \
                                     from this one's."
                ),
            );
            return self.check.opaque(1);
        }
        item
    }

    fn targets(
        &mut self,
        name: &'a str,
        p: &Parts<'a>,
        current: &Context<'a>,
    ) -> Vec<(&'a str, Context<'a>)> {
        let check = self.check;
        let mut targets = vec![];
        for exit in &p.exits {
            let mut next = current.clone();
            if let Some((bind, held)) = &exit.holds {
                let mut item = |collection| self.item(name, collection, current);
                next.insert(
                    bind,
                    Rc::new(
                        Binding {
                            name: bind,
                            way: &exit.way,
                        }
                        .held(check, held, &mut item),
                    ),
                );
            }
            targets.push((exit.to, next));
        }
        for (to, unbinds, way) in self.failures.get(name).into_iter().flatten() {
            let next = self.unbind(current, unbinds, way);
            targets.push((*to, next));
        }
        targets
    }

    fn unbind(&self, current: &Context<'a>, unbinds: &[&'a str], way: &str) -> Context<'a> {
        let mut next = current.clone();
        for item in unbinds {
            next.insert(
                item,
                Rc::new(
                    self.check
                        .lacking(&format!("on {way}, `{item}` is not bound")),
                ),
            );
        }
        next
    }

    fn merge_target(
        &self,
        contexts: &mut BTreeMap<&'a str, Context<'a>>,
        target: &'a str,
        next: Context<'a>,
    ) -> bool {
        match contexts.get_mut(target) {
            Some(existing) => merge(self.check, existing, &next),
            None => {
                contexts.insert(target, next);
                true
            }
        }
    }

    fn contexts(mut self) -> BTreeMap<&'a str, Context<'a>> {
        let (entry_name, entry, parts) = (self.entry_name, self.entry, self.parts);
        let start = self.start();
        let mut contexts = BTreeMap::from([(entry.start.as_str(), start)]);
        let mut pending = VecDeque::from([entry.start.as_str()]);
        let mut queued = BTreeSet::from([entry.start.as_str()]);
        let mut visits = 0usize;
        let budget = MAX_FLOW_VISITS_PER_STEP * parts.len().max(1);
        while let Some(name) = pending.pop_front() {
            queued.remove(name);
            let Some(p) = parts.get(name) else {
                continue;
            };
            visits += 1;
            if visits > budget {
                issue(
                    self.limits,
                    "draft_limit",
                    "playbook",
                    format!(
                        "the steps of entry `{entry_name}` reach each other in too many ways to \
                         check (more than {budget} visits for {} steps). Shorten the draft: fewer \
                         steps that lead back to earlier ones.",
                        parts.len()
                    ),
                );
                break;
            }
            let mut current = contexts[name].clone();
            if let Some(item) = p.own_item {
                current.insert(item, Rc::new(self.check.entering(name, item)));
            }
            let targets = self.targets(name, p, &current);
            for (target, next) in targets {
                let changed = self.merge_target(&mut contexts, target, next);
                if changed && queued.insert(target) {
                    pending.push_back(target);
                }
            }
        }
        contexts
    }
}
