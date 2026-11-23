use anyhow::{Context, Result};
use aq_playbooks_model::Artifact;
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::Engine;
use clap::{Args, Parser, Subcommand};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
mod authoring;
mod controls;
mod discovery;
mod inspection;
mod lifecycle;
mod maintenance;
mod recovery;

#[derive(Parser)]
#[command(
    name = "aq-playbooks",
    version,
    about = "Author, check, inspect and administer bounded policy workflows"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Authoring(authoring::Authoring),
    #[command(flatten)]
    Discovery(discovery::Discovery),
    #[command(flatten)]
    Lifecycle(lifecycle::Lifecycle),
    #[command(flatten)]
    Inspection(inspection::Inspection),
    #[command(flatten)]
    Control(controls::Control),
    #[command(flatten)]
    Maintenance(maintenance::Maintenance),
    /// Explain pending effects or record an evidence-backed host recovery request.
    Reconcile {
        run: Option<String>,
        #[arg(long)]
        request: Option<PathBuf>,
        #[command(flatten)]
        store: StoreArgs,
    },
}

#[derive(Args)]
struct StoreArgs {
    #[arg(long, env = "AQ_PLAYBOOKS_DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[arg(long)]
    namespace: String,
    /// Owner-only JSON keyring file; keys are never stored in the database.
    #[arg(long)]
    protection_keys: Option<PathBuf>,
}
fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    anyhow::ensure!(bytes.len() <= 16_777_216, "input file exceeds 16 MiB");
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}
fn artifact(path: &Path) -> Result<Artifact> {
    let a: Artifact = read(path)?;
    a.verify()?;
    Ok(a)
}
fn print(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn time() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}
fn connect_store(args: &StoreArgs) -> Result<PostgresStorage> {
    match &args.protection_keys {
        Some(path) => Ok(PostgresStorage::connect_protected(
            &args.database_url,
            aq_playbooks_cli::configuration::read_keyring(path)?,
        )?),
        None => Ok(PostgresStorage::connect(&args.database_url)?),
    }
}
fn engine(args: &StoreArgs) -> Result<Engine<PostgresStorage>> {
    Ok(Engine::new(
        connect_store(args)?,
        &args.namespace,
        format!(
            "cli-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ),
    )?)
}
/// Advance `run` until it makes no progress. A run whose cohort settled has
/// left the State (it is in history), so `NotFound` also ends the loop.
fn advance_until_settled(e: &Engine<PostgresStorage>, run: &str) -> Result<()> {
    for _ in 0..1000 {
        match e.advance(run, time()?) {
            Ok(true) => {}
            Ok(false) | Err(aq_playbooks_runner::Error::NotFound(_)) => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
// Argument errors are JSON on stdout, exit 2, for an agent reading stdout.
fn arguments() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) if !error.use_stderr() => error.exit(),
        Err(error) => {
            println!("{}", json!({"error": error.render().to_string()}));
            std::process::exit(2);
        }
    }
}

fn main() {
    let cli = arguments();
    // Discovery is agent-facing too: its refusals are JSON on stdout.
    let discovery = matches!(
        cli.command,
        Command::Discovery(
            discovery::Discovery::Commands { .. } | discovery::Discovery::Events { .. }
        )
    );
    if let Err(error) = execute(cli) {
        if discovery {
            println!("{}", json!({"error": format!("{error:#}")}));
        } else {
            eprintln!("{error:#}");
        }
        std::process::exit(1);
    }
}
fn execute(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Authoring(command) => authoring::run(command),
        Command::Discovery(command) => command.execute(),
        Command::Lifecycle(command) => command.execute(),
        Command::Inspection(command) => command.execute(),
        Command::Control(command) => command.execute(),
        Command::Maintenance(command) => command.execute(),
        Command::Reconcile {
            run,
            request,
            store,
        } => recovery::reconcile(run, request, store),
    }
}
