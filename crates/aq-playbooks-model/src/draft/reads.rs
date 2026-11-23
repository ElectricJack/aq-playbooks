//! Resolve lexical, reserved and step names, and report missing values at their consumers.
use super::*;
impl<'a, 'k> Pass<'a, 'k> {
    pub(super) fn new(check: &'k Check<'a>) -> Self {
        Self {
            check,
            refusals: vec![],
            by_read: HashMap::new(),
            sunk: HashSet::new(),
            kept: vec![],
            charges: BTreeMap::new(),
        }
    }
    /// `scope` with the item of a `map` or `filter` over `collection`.
    pub(super) fn with_item(
        &self,
        scope: &Scope<'a>,
        item: &'a str,
        collection: &Out<'a>,
    ) -> Scope<'a> {
        let note = match &collection.origin {
            Some(origin) => format!("is an item of `{origin}`"),
            None => "is an item of its collection".to_string(),
        };
        let mut inner = scope.clone();
        // The item shadows any binding of the same name.
        inner.insert(
            item,
            Named {
                carried: self.check.items_of(&collection.carried),
                origin: collection.origin.as_ref().map(|o| format!("{o}[]").into()),
                note: note.into(),
            },
        );
        inner
    }
    /// What the reserved root `root` holds at `place`.
    fn reserved(&self, root: Reserved, place: Place<'a, '_>) -> Carried<'a> {
        let check = self.check;
        let elsewhere =
            |only: &str| check.lacking(&format!("`{}` is there only in {only}", root.name()));
        match (root, place) {
            (
                Reserved::Input,
                Place::Step {
                    entry_name, entry, ..
                },
            ) => check.shape(
                &entry.input,
                &format!("the input of entry `{entry_name}`"),
                root.name(),
            ),
            (Reserved::Input, Place::Trigger { .. }) => elsewhere("a step"),
            (Reserved::Event, Place::Trigger { payload }) => match payload {
                Some(schema) => check.shape(schema, "the trigger's event", root.name()),
                None => check.opaque(1),
            },
            (Reserved::Event, Place::Step { .. }) => elsewhere("a trigger's `input`"),
            (Reserved::Definitions, Place::Step { .. } | Place::Trigger { .. }) => {
                match &check.sources.definitions {
                    Some(schema) => check.shape(schema, "the captured definitions", root.name()),
                    None => check.opaque(1),
                }
            }
            // Every binding and item, each as it is at this step.
            (Reserved::Vars, Place::Step { context, .. }) => check.built(
                context
                    .iter()
                    .map(|(name, carried)| ((*name).to_string(), (**carried).clone()))
                    .collect(),
                false,
            ),
            (Reserved::Vars, Place::Trigger { .. }) => elsewhere("a step"),
        }
    }
    /// A reference: what `root.path` reads.
    pub(super) fn read(
        &mut self,
        root: &'a str,
        path: &'a [String],
        site: Site<'a, '_>,
    ) -> Out<'a> {
        let check = self.check;
        let Site { at, scope, .. } = site;
        let written = dotted(root, path);
        let mut found = Build::default();
        // A lexical name first: it shadows; then a reserved root; then a
        // binding or item of the step's context.
        let lexical = scope.get(root);
        let (source, note, origin) = self.resolve(root, path, site);
        // Following a value that can have many shapes costs one step for
        // each: that work is counted, and past a bound the draft is refused
        // without doing it.
        let shapes = source.as_ref().map_or(1, |s| s.forms.len()) as u64;
        if shapes > MANY_SHAPES {
            check.work.set(check.work.get().saturating_add(shapes));
        }
        if check.work.get() > MAX_SHAPE_WORK {
            self.charge(root, shapes);
            return Out {
                carried: check.scalar.clone(),
                size: shapes,
                origin: None,
            };
        }
        match &source {
            Some(source) => source.follow(path, &mut found),
            // No such root: ordinary typing says so.
            None => found.form(Form::Opaque(1)),
        }
        let mut carried = check.finish(found);
        if lexical.is_some()
            && let Some(gaps) = carried.gaps.take()
        {
            carried.gaps = Some(Rc::new(GapNode::Via(root.into(), gaps)));
        }
        // What is not there on some way is Missing here, read by this
        // reference: from now on it travels with the value.
        if !carried.lacks.is_empty() {
            let read = Rc::new(Read {
                at: at.to_string(),
                written: written.clone(),
                note,
                identity: false,
            });
            let leaves = std::mem::take(&mut carried.lacks)
                .into_iter()
                .map(|reason| {
                    Some(Rc::new(GapNode::Leaf {
                        reason,
                        read: read.clone(),
                    }))
                })
                .collect::<Vec<_>>();
            carried.gaps = join_gaps(std::iter::once(carried.gaps.take()).chain(leaves));
        }
        // Every reference counts the type it reads (and no less than the
        // shapes it had to follow): what a name stands for is counted once,
        // where it is written.
        let size = carried.nodes.max(shapes).max(1);
        self.charge(root, size);
        Out {
            carried,
            size,
            origin,
        }
    }
    fn resolve(
        &self,
        root: &'a str,
        path: &'a [String],
        site: Site<'a, '_>,
    ) -> (Option<Carried<'a>>, Option<String>, Option<Rc<str>>) {
        let Site { scope, place, .. } = site;
        let lexical = scope.get(root);
        match lexical {
            Some(named) => (
                Some(named.carried.clone()),
                Some(format!("`{root}` {}", named.note)),
                named.origin.as_ref().map(|o| dotted(o, path).into()),
            ),
            None => {
                let (source, shown) = match (Reserved::named(root), place) {
                    (Some(reserved), _) => (Some(self.reserved(reserved, place)), None),
                    (None, Place::Step { context, .. }) => (
                        context.get(root).map(|c| (**c).clone()),
                        self.check.item_notes.get(root),
                    ),
                    (None, Place::Trigger { .. }) => (None, None),
                };
                let origin = match (root, path.split_first()) {
                    ("vars", Some((binding, rest))) => dotted(binding, rest),
                    _ => dotted(root, path),
                };
                (
                    source,
                    shown.map(|n| format!("`{root}` {n}")),
                    Some(origin.into()),
                )
            }
        }
    }

    fn charge(&mut self, root: &str, size: u64) {
        let charge = self.charges.entry(root.to_string()).or_default();
        charge.each = charge.each.max(size);
        charge.times += 1;
        charge.total = charge.total.saturating_add(size);
    }
    /// The value with these gaps is needed here: every read behind them is
    /// refused. `key`: needed as a lookup key.
    pub(super) fn sink(&mut self, gaps: &Gaps, key: bool) {
        let Some(top) = gaps else {
            return;
        };
        let mut pending: Vec<(Rc<GapNode>, Option<Rc<str>>)> = vec![(top.clone(), None)];
        while let Some((node, through)) = pending.pop() {
            if !self.sunk.insert((Rc::as_ptr(&node), key)) {
                continue;
            }
            self.kept.push(node.clone());
            match &*node {
                GapNode::Leaf { reason, read } => {
                    self.refuse(read, reason, ReadUse { key, through });
                }
                GapNode::All(parts) => {
                    pending.extend(parts.iter().map(|p| (p.clone(), through.clone())));
                }
                GapNode::Via(name, inner) => pending.push((inner.clone(), Some(name.clone()))),
            }
        }
    }
    pub(super) fn refuse(
        &mut self,
        read: &Rc<Read>,
        reason: &Rc<str>,
        ReadUse { key, through }: ReadUse,
    ) {
        let index = *self.by_read.entry(Rc::as_ptr(read)).or_insert_with(|| {
            self.refusals.push(Refusal {
                read: read.clone(),
                reasons: vec![],
                more: 0,
                key: false,
                through: vec![],
            });
            self.refusals.len() - 1
        });
        let refusal = &mut self.refusals[index];
        if refusal.reasons.contains(reason) {
        } else if refusal.reasons.len() >= MAX_REASONS {
            refusal.more += 1;
        } else {
            refusal.reasons.push(reason.clone());
        }
        refusal.key |= key;
        if let Some(name) = through
            && !refusal.through.contains(&name)
        {
            refusal.through.push(name);
        }
    }
    /// One expression a step (or trigger) evaluates and needs the value of:
    /// its refused reads as (location, message), and its size.
    pub(super) fn evaluate(
        &mut self,
        (path, at): (&str, &str),
        expr: &'a Expr,
        place: Place<'a, '_>,
    ) -> (Vec<(String, String)>, ExprSize) {
        self.charges.clear();
        let out = self.derive(
            expr,
            Site {
                at,
                scope: &Scope::new(),
                place,
            },
        );
        self.sink(&out.carried.gaps, false);
        let reads = std::mem::take(&mut self.refusals)
            .into_iter()
            .map(|refusal| (refusal.read.at.clone(), refusal.message()))
            .collect();
        self.by_read.clear();
        self.sunk.clear();
        self.kept.clear();
        let most = self
            .charges
            .iter()
            .max_by_key(|(_, c)| c.total)
            .map(|(name, c)| (name.clone(), c.clone()));
        let sized = ExprSize {
            path: path.to_string(),
            at: at.to_string(),
            size: out.size,
            most,
        };
        (reads, sized)
    }
}
impl Refusal {
    fn message(&self) -> String {
        let read = &self.read;
        let mut notes: Vec<String> = read.note.iter().cloned().collect();
        if !self.through.is_empty() {
            let names: Vec<_> = self.through.iter().map(|n| format!("`{n}`")).collect();
            notes.push(format!(
                "its value is used through {}, outside any `exists`",
                names.join(", ")
            ));
        }
        let note = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join("; "))
        };
        let mut reasons: Vec<String> = self.reasons.iter().map(|r| r.to_string()).collect();
        if self.more > 0 {
            reasons.push(format!("and {} more ways", self.more));
        }
        let remedy = if read.identity {
            "Build both collections only from values that always have it: an item without its \
             identity stops the run, and `exists` cannot test it"
        } else {
            "Read it only where it is always present: after the outcome edge of the step that \
             returns it, or on the `completed` edge of a join whose `accepted_outcomes` lists \
             only outcomes that have it (a `decide` on `outcome` does not narrow it). `exists` \
             only tells you whether it is there: testing it does not make a read of it allowed, \
             in the same expression or in a later step. So branch on the edge first and read \
             the value in the step that edge leads to; use `exists` on its own, as a test \
             that routes to a step that does not read the value"
        };
        let mut message = format!(
            "`{}`{note} is not there on every way to this step: {}. {remedy}",
            read.written,
            reasons.join("; ")
        );
        if self.key {
            message.push_str(
                ". A lookup whose key is missing takes its default without a trace, so a \
                 lookup key is held to this too",
            );
        }
        message
    }
}
