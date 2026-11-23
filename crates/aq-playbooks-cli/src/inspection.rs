use super::{StoreArgs, connect_store, print, time};
use anyhow::Result;
use aq_playbooks_runner::{
    CatalogKind, CatalogQuery, Engine, InspectionKind, InspectionQuery, Storage,
};
use clap::{Subcommand, ValueEnum};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Subcommand)]
pub(super) enum Inspection {
    /// Diagnostic aggregate scan; protected stores require their configured keys.
    Metrics {
        #[command(flatten)]
        store: StoreArgs,
    },
    Inspect {
        run: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, value_enum, default_value = "runs")]
        kind: InspectKind,
        #[arg(long)]
        after: Option<String>,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Inspect current metadata while explicitly owning an existing idle namespace.
    /// Fails rather than stealing another process's lease. Never admits/drives work.
    InspectCatalog {
        #[arg(long, value_enum, default_value = "definitions")]
        kind: CatalogInspectKind,
        #[arg(long)]
        exact: Option<String>,
        #[arg(long, default_value_t = 25)]
        limit: usize,
        #[arg(long, default_value_t = 1)]
        pages: usize,
        #[arg(long, required = true)]
        acquire_idle_namespace: bool,
        #[command(flatten)]
        store: StoreArgs,
    },
}

impl Inspection {
    pub(super) fn execute(self) -> Result<()> {
        match self {
            Self::Metrics { store } => {
                let db = connect_store(&store)?;
                let state = db.load(&store.namespace)?.state;
                print(&aq_playbooks_runner::metrics::StateMetrics::from_state(
                    &state,
                    time()?,
                )?)
            }
            Self::Inspect {
                run,
                limit,
                kind,
                after,
                store,
            } => {
                let db = connect_store(&store)?;
                print(&db.inspect(
                    &store.namespace,
                    &InspectionQuery {
                        kind: kind.into(),
                        run,
                        after,
                        limit,
                    },
                )?)
            }
            Self::InspectCatalog {
                kind,
                exact,
                limit,
                pages,
                acquire_idle_namespace,
                store,
            } => {
                anyhow::ensure!(
                    acquire_idle_namespace && (1..=4).contains(&pages),
                    "explicit idle ownership and 1..4 pages required"
                );
                let mut query = CatalogQuery::first(kind.into(), limit);
                query.exact = exact;
                query.validate()?;
                inspect_catalog(&store, query, pages)
            }
        }
    }
}

fn inspect_catalog(store: &StoreArgs, mut query: CatalogQuery, pages: usize) -> Result<()> {
    let db = connect_store(store)?;
    anyhow::ensure!(
        db.incarnation(&store.namespace)?.is_some(),
        "catalog namespace does not exist"
    );
    let e = Engine::new(
        db,
        &store.namespace,
        format!(
            "catalog-cli-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ),
    )?;
    let observed = (|| -> Result<Vec<_>> {
        let mut output = Vec::with_capacity(pages);
        for _ in 0..pages {
            let page = e.inspect_catalog(&query)?;
            query.after = page.next_cursor.clone();
            query.expected = Some(page.stamp.clone());
            output.push(page);
            if query.after.is_none() {
                break;
            }
        }
        Ok(output)
    })();
    let released = e.release();
    let observed = observed?;
    released?;
    if pages == 1 {
        print(&observed[0])
    } else {
        print(&observed)
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum InspectKind {
    Runs,
    Effects,
    Receipts,
}
#[derive(Clone, Copy, ValueEnum)]
pub(super) enum CatalogInspectKind {
    Definitions,
    Activations,
}
impl From<CatalogInspectKind> for CatalogKind {
    fn from(kind: CatalogInspectKind) -> Self {
        match kind {
            CatalogInspectKind::Definitions => Self::Definitions,
            CatalogInspectKind::Activations => Self::Activations,
        }
    }
}
impl From<InspectKind> for InspectionKind {
    fn from(value: InspectKind) -> Self {
        match value {
            InspectKind::Runs => Self::Runs,
            InspectKind::Effects => Self::Effects,
            InspectKind::Receipts => Self::Receipts,
        }
    }
}
