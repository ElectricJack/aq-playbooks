use super::*;
use crate::Intent;

pub(super) fn refresh_intent(s: &mut State, id: &str) -> Result<()> {
    let was_uncertain = s.indexes.uncertain_contract_membership.remove(id);
    if let Some(key) = &was_uncertain {
        remove_member(&mut s.indexes.uncertain_completion_contracts, key, id);
    }
    let result = refresh_intent_indexes(s, id);
    // A hold through an uncertain completion begins or ends: the events of
    // that contract and scope are judged again (`retention::rejoin`).
    let is_uncertain = s.indexes.uncertain_contract_membership.get(id).cloned();
    // An operation never dispatched holds nothing either way (a membership
    // it loses here is one an older engine recorded): nothing to judge.
    let after = match s.intents.get(id) {
        Some(i) => i.dispatch_sequence,
        // Gone from the State: every event (0).
        None => Some(0),
    };
    if was_uncertain != is_uncertain
        && let Some(after) = after
    {
        // Only events accepted after the dispatch can be its completion.
        for key in was_uncertain.into_iter().chain(is_uncertain) {
            let entry = s.replay_touched.contract_keys.entry(key).or_insert(after);
            *entry = (*entry).min(after);
        }
    }
    result
}

fn refresh_intent_indexes(s: &mut State, id: &str) -> Result<()> {
    unsubscribe(s, &format!("intent:{id}"));
    clear_effects(&mut s.indexes, id);
    remove_timer(s, &format!("effect:{id}"));
    let Some(i) = s.intents.get(id).cloned() else {
        remove_intent(&mut s.indexes, id);
        return Ok(());
    };
    let entry = IntentEntry { id, intent: &i };
    entry.index_root(s);
    if i.completion_event.is_some() {
        s.indexes.completion_ready.remove(id);
    }
    entry.queue_effects(s);
    entry.index_completion(s)?;
    wake_dependents(s, id);
    mark_run(s, &i.run);
    Ok(())
}

fn clear_effects(indexes: &mut RuntimeIndexes, id: &str) {
    for (kind, scope) in indexes.effect_memberships.remove(id).unwrap_or_default() {
        remove_effect(indexes, id, &kind, &scope);
    }
}

fn remove_effect(indexes: &mut RuntimeIndexes, id: &str, kind: &str, scope: &str) {
    let Some(scopes) = indexes.effects.get_mut(kind) else {
        return;
    };
    let Some(ids) = scopes.get_mut(scope) else {
        return;
    };
    ids.remove(id);
    if ids.is_empty() {
        scopes.remove(scope);
        indexes
            .effect_item_cursor
            .remove(&format!("{kind}:{scope}"));
    }
}

fn remove_intent(indexes: &mut RuntimeIndexes, id: &str) {
    indexes.completion_ready.remove(id);
    let Some(root) = indexes.intent_roots.remove(id) else {
        return;
    };
    if let Some(ids) = indexes.root_effects.get_mut(&root) {
        ids.remove(id);
    }
    remove_lazy_member(&mut indexes.root_intents, &root, id);
}

fn terminal(i: &Intent) -> bool {
    matches!(
        i.status,
        IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
    )
}

fn queues(i: &Intent) -> [(EffectQueueKind, bool); 3] {
    let cancelling = i.cancel_requested && !terminal(i);
    let recover = matches!(i.status, IntentStatus::Dispatched | IntentStatus::Unknown)
        || (cancelling && i.cancel_dispatched);
    [
        (
            EffectQueueKind::Dispatch,
            i.status == IntentStatus::Prepared,
        ),
        (EffectQueueKind::Cancel, cancelling && !i.cancel_dispatched),
        (EffectQueueKind::Recovery, recover),
    ]
}

struct IntentEntry<'a> {
    id: &'a str,
    intent: &'a Intent,
}

impl IntentEntry<'_> {
    fn index_root(&self, s: &mut State) {
        let Self { id, intent: i } = *self;
        let Some(root) = s
            .runs
            .get(&i.run)
            .map(|r| r.root.clone())
            .or_else(|| s.indexes.run_roots.get(&i.run).cloned())
        else {
            return;
        };

        s.indexes.intent_roots.insert(id.into(), root.clone());
        if !s
            .indexes
            .root_intents
            .get(&root)
            .is_some_and(|ids| ids.contains(id))
        {
            s.indexes
                .root_intents
                .entry(root.clone())
                .or_default()
                .insert(id.into());
        }
        if terminal(i) {
            if let Some(ids) = s.indexes.root_effects.get_mut(&root) {
                ids.remove(id);
            }
        } else {
            s.indexes
                .root_effects
                .entry(root)
                .or_default()
                .insert(id.into());
        }
    }
    fn queue_effects(&self, s: &mut State) {
        let Self { id, intent: i } = *self;
        for (kind, queued) in queues(i) {
            if !queued {
                continue;
            }
            self.enqueue(&mut s.indexes, kind);
        }
        if matches!(i.status, IntentStatus::Dispatched | IntentStatus::Accepted) {
            timer(
                s,
                TimerIndex {
                    key: format!("effect:{id}"),
                    run: i.run.clone(),
                    at_ms: i.deadline_at_ms,
                    intent: Some(id.into()),
                },
            );
        }
    }
    fn enqueue(&self, indexes: &mut RuntimeIndexes, kind: EffectQueueKind) {
        indexes
            .effects
            .entry(kind.key().into())
            .or_default()
            .entry(self.intent.authority.scope.clone())
            .or_default()
            .insert(self.id.into());
        indexes
            .effect_memberships
            .entry(self.id.into())
            .or_default()
            .push((kind.key().into(), self.intent.authority.scope.clone()));
    }
}

mod completion;
