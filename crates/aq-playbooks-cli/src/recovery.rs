use super::{StoreArgs, connect_store, engine, print, read, time};
use anyhow::Result;
use aq_playbooks_runner::{InspectionKind, InspectionQuery, Storage};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
pub(super) fn reconcile(
    run: Option<String>,
    request: Option<PathBuf>,
    store: StoreArgs,
) -> Result<()> {
    if let Some(path) = request {
        let request: RecoveryInput = read(&path)?;
        let e = engine(&store)?;
        if let Some(run) = run {
            let state = e.snapshot()?;
            if !state.intents.contains_key(&request.intent) {
                // An operation that left with its finished run is
                // in history: it is settled, nothing to recover.
                let finished = e
                    .inspect(&InspectionQuery {
                        kind: InspectionKind::Effects,
                        run: Some(run.clone()),
                        after: None,
                        limit: 1000,
                    })?
                    .items
                    .iter()
                    .any(|item| item["id"] == request.intent.as_str());
                anyhow::ensure!(
                    !finished,
                    "recovery intent has finished with its run; nothing to recover"
                );
            }
            anyhow::ensure!(
                state
                    .intents
                    .get(&request.intent)
                    .is_some_and(|i| i.run == run),
                "recovery intent does not belong to selected run"
            );
        }
        let id = e.request_recovery(
            &request.intent,
            &request.actor,
            &request.reason,
            request.evidence_refs,
            time()?,
        )?;
        e.release()?;
        print(
            &json!({"recovery_request":id,"status":"recorded","execution":"registered host performs authoritative reconciliation"}),
        )
    } else {
        let db = connect_store(&store)?;
        print(&db.inspect(
            &store.namespace,
            &InspectionQuery {
                kind: InspectionKind::Effects,
                run,
                after: None,
                limit: 100,
            },
        )?)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryInput {
    intent: String,
    actor: String,
    reason: String,
    evidence_refs: Vec<String>,
}
