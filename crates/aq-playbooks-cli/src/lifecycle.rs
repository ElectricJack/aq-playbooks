use super::{StoreArgs, advance_until_settled, artifact, engine, print, read, time};
use anyhow::Result;
use aq_playbooks_cli::simulation;
use aq_playbooks_runner::Authority;
use clap::Subcommand;
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Subcommand)]
pub(super) enum Lifecycle {
    Install {
        artifact: PathBuf,
        #[command(flatten)]
        store: StoreArgs,
    },
    Activate {
        binding: String,
        artifact: String,
        #[arg(long)]
        authority: PathBuf,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Admit and evaluate pure transitions. Host effects remain durable intents.
    Run {
        binding: String,
        #[arg(long, default_value = "main")]
        entry: String,
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        authority: PathBuf,
        #[command(flatten)]
        store: StoreArgs,
    },
}

impl Lifecycle {
    pub(super) fn execute(self) -> Result<()> {
        match self {
            Self::Install {
                artifact: path,
                store,
            } => {
                let a = artifact(&path)?;
                let digest = a.digest.clone();
                let e = engine(&store)?;
                e.install(a)?;
                e.release()?;
                print(&json!({"installed":digest}))
            }
            Self::Activate {
                binding,
                artifact,
                authority,
                store,
            } => {
                let e = engine(&store)?;
                e.activate_at(&binding, &artifact, read(&authority)?, time()?)?;
                e.release()?;
                print(&json!({"binding":binding,"artifact":artifact}))
            }
            Self::Run {
                binding,
                entry,
                input,
                authority,
                store,
            } => {
                let e = engine(&store)?;
                let id = e.admit(
                    &binding,
                    &entry,
                    read::<Value>(&input)?,
                    read::<Authority>(&authority)?,
                    time()?,
                )?;
                advance_until_settled(&e, &id)?;
                let view = simulation::inspect(&e, Some(&id))?;
                e.release()?;
                print(&view)
            }
        }
    }
}
