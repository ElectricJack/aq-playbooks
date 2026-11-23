use super::{StoreArgs, advance_until_settled, engine, print, time};
use anyhow::{Context, Result};
use aq_playbooks_cli::simulation;
use clap::Subcommand;
use serde_json::json;

#[derive(Subcommand)]
pub(super) enum Control {
    Cancel {
        run: String,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Set independent controls for new calls, event starts and execution authority.
    Controls {
        binding: String,
        #[arg(long)]
        calls_enabled: Option<bool>,
        #[arg(long)]
        event_starts_enabled: Option<bool>,
        #[arg(long)]
        execution_enabled: Option<bool>,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Replace the revoked capability set for one actor/scope. Empty explicitly restores it.
    Authority {
        actor: String,
        scope: String,
        #[arg(long)]
        revoke: Vec<String>,
        #[command(flatten)]
        store: StoreArgs,
    },
}

impl Control {
    pub(super) fn execute(self) -> Result<()> {
        match self {
            Self::Cancel { run, store } => {
                let e = engine(&store)?;
                e.cancel(&run, time()?)?;
                advance_until_settled(&e, &run)?;
                let view = simulation::inspect(&e, Some(&run))?;
                e.release()?;
                print(&view)
            }
            Self::Controls {
                binding,
                calls_enabled,
                event_starts_enabled,
                execution_enabled,
                store,
            } => {
                anyhow::ensure!(
                    calls_enabled.is_some()
                        || event_starts_enabled.is_some()
                        || execution_enabled.is_some(),
                    "specify at least one control"
                );
                let e = engine(&store)?;
                let state = e.snapshot()?;
                let mut controls = state
                    .bindings
                    .get(&binding)
                    .context("unknown callable binding")?
                    .controls
                    .clone();
                if let Some(value) = calls_enabled {
                    controls.calls_enabled = value;
                }
                if let Some(value) = event_starts_enabled {
                    controls.event_starts_enabled = value;
                }
                if let Some(value) = execution_enabled {
                    controls.execution_enabled = value;
                }
                e.set_activation_controls(&binding, controls.clone(), time()?)?;
                e.release()?;
                print(&json!({"binding":binding,"controls":controls}))
            }
            Self::Authority {
                actor,
                scope,
                revoke,
                store,
            } => {
                let e = engine(&store)?;
                e.set_execution_authority(&actor, &scope, revoke.into_iter().collect(), time()?)?;
                e.release()?;
                print(&json!({"authority_updated":true,"actor":actor,"scope":scope}))
            }
        }
    }
}
