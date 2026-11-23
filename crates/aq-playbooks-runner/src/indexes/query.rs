use super::*;

fn fair_scopes<T>(scopes: &BTreeMap<String, T>, after: Option<&str>) -> Vec<String> {
    let mut v: Vec<_> = scopes
        .keys()
        .filter(|s| after.is_none_or(|a| s.as_str() > a))
        .cloned()
        .collect();
    v.extend(
        scopes
            .keys()
            .filter(|s| after.is_some_and(|a| s.as_str() <= a))
            .cloned(),
    );
    v
}
impl RuntimeIndexes {
    pub fn query(&self, state: &State, query: &IndexQuery) -> IndexPage {
        let limit = query.page_limit();
        let mut page = Page {
            indexes: self,
            state,
            limit,
            items: vec![],
        };
        match query {
            IndexQuery::Ready { scope_after, .. } => page.ready(scope_after.as_deref()),
            IndexQuery::Effects {
                kind, scope_after, ..
            } => page.effects(*kind, scope_after.as_deref()),
            IndexQuery::DueTimers { now_ms, .. } => page.due(*now_ms),
            IndexQuery::CorrelatedEvents {
                key,
                after_sequence,
                ..
            } => page.events(key, *after_sequence),
            IndexQuery::Waiters { key, .. } => page.waiters(key),
            IndexQuery::Runs { after, .. } => page.runs(after.as_deref()),
            IndexQuery::Receipts {
                run,
                after_sequence,
                ..
            } => page.receipts(run.as_deref(), *after_sequence),
        }
        let mut items = page.items;
        if limit == 0 {
            items.clear()
        }
        query.page(items)
    }
}

impl IndexItem {
    fn scoped(id: &str, scope: Option<&String>) -> Self {
        Self {
            key: id.into(),
            scope: scope.cloned().unwrap_or_default(),
            sequence: 0,
            at_ms: None,
        }
    }
}

struct Page<'a> {
    indexes: &'a RuntimeIndexes,
    state: &'a State,
    limit: usize,
    items: Vec<IndexItem>,
}
impl Page<'_> {
    fn ready(&mut self, scope_after: Option<&str>) {
        for scope in fair_scopes(
            &self.indexes.ready,
            scope_after.or(self.indexes.scope_cursor.as_deref()),
        ) {
            if let Some((seq, id)) = self.indexes.ready[&scope].first_key_value() {
                self.items.push(IndexItem {
                    key: id.clone(),
                    scope: scope.clone(),
                    sequence: *seq,
                    at_ms: None,
                });
                if self.items.len() >= self.limit {
                    break;
                }
            }
        }
    }
    fn completions(&mut self) {
        for id in self.indexes.completion_ready.iter().take(self.limit) {
            self.items.push(IndexItem::scoped(
                id,
                self.state
                    .intents
                    .get(id)
                    .map(|record| &record.authority.scope),
            ));
        }
    }
    fn queued(
        &mut self,
        kind: EffectQueueKind,
        scope_after: Option<&str>,
        scopes: &BTreeMap<String, BTreeSet<String>>,
    ) {
        for scope in fair_scopes(
            scopes,
            scope_after.or_else(|| {
                self.indexes
                    .effect_cursor
                    .get(kind.key())
                    .map(String::as_str)
            }),
        ) {
            let cursor = self
                .indexes
                .effect_item_cursor
                .get(&format!("{}:{}", kind.key(), scope));
            let selected = scopes[&scope]
                .iter()
                .find(|id| cursor.is_none_or(|cursor| *id > cursor))
                .or_else(|| scopes[&scope].first());
            if let Some(id) = selected {
                self.items.push(IndexItem {
                    key: id.clone(),
                    scope: scope.clone(),
                    sequence: 0,
                    at_ms: None,
                });
            }
            if self.items.len() >= self.limit {
                break;
            }
        }
    }
    fn effects(&mut self, kind: EffectQueueKind, scope_after: Option<&str>) {
        if kind == EffectQueueKind::Completion {
            self.completions();
        } else if let Some(scopes) = self.indexes.effects.get(kind.key()) {
            self.queued(kind, scope_after, scopes);
        }
    }
    fn due(&mut self, now_ms: i64) {
        let mut after: Option<i64> = None;
        while let Some(at) = self.indexes.due.next_key(after.as_ref()) {
            if at > now_ms {
                break;
            }
            after = Some(at);
            let at = &at;
            let ids = &self.indexes.due[at];
            for id in ids {
                let t = &self.indexes.timers[id];
                self.items.push(IndexItem {
                    key: id.clone(),
                    scope: self
                        .state
                        .runs
                        .get(&t.run)
                        .map(|r| r.authority.scope.clone())
                        .unwrap_or_default(),
                    sequence: 0,
                    at_ms: Some(*at),
                });
                if self.items.len() >= self.limit {
                    break;
                }
            }
            if self.items.len() >= self.limit {
                break;
            }
        }
    }
    fn events(&mut self, key: &str, after_sequence: u64) {
        if let Some(events) = self.indexes.events.get(key) {
            for (seq, id) in events
                .range((
                    std::ops::Bound::Excluded(after_sequence),
                    std::ops::Bound::Unbounded,
                ))
                .take(self.limit)
            {
                self.items.push(IndexItem {
                    key: id.clone(),
                    scope: self
                        .state
                        .events
                        .get(id)
                        .map(|e| e.envelope.scope.clone())
                        .unwrap_or_default(),
                    sequence: *seq,
                    at_ms: self.state.events.get(id).map(|e| e.accepted_at_ms),
                });
            }
        }
    }
    fn waiters(&mut self, key: &str) {
        if let Some(waiters) = self.indexes.waiters.get(key) {
            for id in waiters.iter().take(self.limit) {
                self.items.push(IndexItem::scoped(
                    id,
                    self.state
                        .runs
                        .get(id)
                        .map(|record| &record.authority.scope),
                ));
            }
        }
    }
    fn runs(&mut self, after: Option<&str>) {
        for id in self
            .indexes
            .runs
            .iter()
            .filter(|id| after.is_none_or(|a| id.as_str() > a))
            .take(self.limit)
        {
            self.items.push(IndexItem::scoped(
                id,
                self.state
                    .runs
                    .get(id)
                    .map(|record| &record.authority.scope),
            ));
        }
    }
    fn receipts(&mut self, run: Option<&str>, after_sequence: u64) {
        let receipts = if let Some(run) = run {
            self.indexes.receipts.get(run)
        } else {
            Some(&*self.indexes.all_receipts)
        };
        if let Some(receipts) = receipts {
            for (seq, offset) in receipts
                .range((
                    std::ops::Bound::Excluded(after_sequence),
                    std::ops::Bound::Unbounded,
                ))
                .take(self.limit)
            {
                self.items.push(IndexItem {
                    // The receipt's sequence (its stable identity),
                    // never its position in the record.
                    key: format!("{seq:020}"),
                    scope: String::new(),
                    sequence: *seq,
                    at_ms: self.state.receipts.get(*offset).map(|r| r.at_ms),
                });
            }
        }
    }
}
