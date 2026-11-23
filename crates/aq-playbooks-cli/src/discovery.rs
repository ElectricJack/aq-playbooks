use super::{artifact, authoring, print, read};
use anyhow::{Context, Result};
use aq_playbooks_cli::catalog;
use aq_playbooks_model::{CatalogExtension, Registry};
use clap::Subcommand;
use serde::Serialize;
use serde_json::json;
use std::{fs, path::PathBuf};

#[derive(Subcommand)]
pub(super) enum Discovery {
    Artifact {
        #[command(subcommand)]
        action: ArtifactAction,
    },
    Commands {
        #[arg(long)]
        registry: Option<PathBuf>,
        #[command(subcommand)]
        action: CatalogAction,
    },
    Events {
        #[arg(long)]
        registry: Option<PathBuf>,
        #[command(subcommand)]
        action: CatalogAction,
    },
    /// Write the worked authoring workspaces (each a directory to run the
    /// authoring tools in) under DIR.
    Examples {
        #[arg(long)]
        output: PathBuf,
    },
}

impl Discovery {
    pub(super) fn execute(self) -> Result<()> {
        match self {
            Self::Artifact {
                action: ArtifactAction::Inspect { path },
            } => print(&artifact(&path)?),
            Self::Commands { registry, action } => {
                let (registry, extensions) = catalog_file(registry)?;
                choose("commands", &registry.commands, &extensions, action)
            }
            Self::Events { registry, action } => {
                let (registry, extensions) = catalog_file(registry)?;
                choose("events", &registry.events, &extensions, action)
            }
            Self::Examples { output } => {
                let files = aq_playbooks_compiler::docs::example_files();
                for (path, text) in &files {
                    let target = output.join(path);
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&target, text)?;
                }
                let examples: std::collections::BTreeSet<&str> = files
                    .iter()
                    .filter_map(|(p, _)| p.split('/').next())
                    .collect();
                print(&json!({"directory":output,"examples":examples}))
            }
        }
    }
}

#[derive(Subcommand)]
pub(super) enum ArtifactAction {
    Inspect { path: PathBuf },
}
#[derive(Subcommand)]
pub(super) enum CatalogAction {
    List,
    Describe { name: String },
}

/// Lists or describes a contract. A contract that is the pinned catalog's
/// own is described with the host's notes on it (`"notes"`), the same notes
/// `CATALOG.md` renders.
fn choose<T: Serialize>(
    kind: &str,
    map: &std::collections::BTreeMap<String, T>,
    extensions: &[CatalogExtension],
    action: CatalogAction,
) -> Result<()> {
    match action {
        CatalogAction::List => print(&map.keys().collect::<Vec<_>>()),
        CatalogAction::Describe { name } => {
            let contract = map
                .get(&name)
                .with_context(|| format!("unknown registration: {name}"))?;
            let mut described = serde_json::to_value(contract)?;
            if let Some(notes) = extensions
                .iter()
                .find_map(|extension| extension.notes().get(kind)?.get(&name))
                && let Some(object) = described.as_object_mut()
            {
                object.insert("notes".into(), notes.clone());
                return print(&described);
            }
            if let Some(notes) =
                aq_playbooks_compiler::docs::published_notes(kind, &name, &described)
                && let Some(object) = described.as_object_mut()
            {
                object.insert("notes".into(), notes);
            }
            print(&described)
        }
    }
}
fn catalog_file(path: Option<PathBuf>) -> Result<(Registry, Vec<CatalogExtension>)> {
    if let Some(registry) = authoring::workspace_registry(path.as_deref())? {
        return Ok(registry);
    }
    let base = path
        .map(|p| read(&p))
        .unwrap_or_else(|| Ok(catalog::catalog()))?;
    Ok((base, vec![]))
}
