use super::{StoreArgs, engine, print, read, time};
use anyhow::Result;
use aq_playbooks_runner::RetentionPolicy;
use clap::{Args, Subcommand};
use serde_json::json;
use std::path::PathBuf;

#[derive(Subcommand)]
pub(super) enum Maintenance {
    /// Not needed: every commit already deletes finished work and old
    /// entries, a bounded amount each. Kept for existing scripts: runs one
    /// more such commit (plus a page of unreferenced artifact disposal) and
    /// prints what it did.
    Gc {
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Set explicit retention windows from a JSON policy (the deprecated
    /// `max_tombstones` and `terminal_run_ms` are accepted and ignored).
    Retention {
        policy: PathBuf,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Fence a retired authenticated producer epoch: its events and requests are refused from then on.
    RetireProducer {
        producer: String,
        epoch: u64,
        #[arg(long)]
        actor: String,
        #[arg(long)]
        reason: String,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Lower a retention clock that a far-future host time pinned (every
    /// request with a request time is refused as stale while it is ahead).
    /// Caps expiries and run timers written from the pin, records a stale
    /// floor and an admin receipt. Run with --dry-run first; committing
    /// needs --confirm-clock-rebase.
    RebaseRetentionClock(ClockRebase),
    /// Lower a stale floor an earlier rebase set too high, accepting that
    /// keys forgotten after the new floor may be re-admitted as new by a
    /// retry with a request time in between. Needs --confirm-floor-lowering.
    LowerStaleFloor {
        #[arg(long)]
        to_ms: i64,
        #[arg(long)]
        actor: String,
        #[arg(long)]
        reason: String,
        #[arg(long, required = true)]
        confirm_floor_lowering: bool,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Re-encrypt the namespace with the active key from --protection-keys.
    RotateKeys {
        #[command(flatten)]
        store: StoreArgs,
    },
}

impl Maintenance {
    pub(super) fn execute(self) -> Result<()> {
        match self {
            Self::Gc { limit, store } => {
                let e = engine(&store)?;
                let report = e.collect_garbage(time()?, limit)?;
                e.release()?;
                print(&report)
            }
            Self::Retention { policy, store } => {
                let policy: RetentionPolicy = read(&policy)?;
                let e = engine(&store)?;
                e.set_retention_policy(policy)?;
                e.release()?;
                print(&json!({"retention_updated":true}))
            }
            Self::RetireProducer {
                producer,
                epoch,
                actor,
                reason,
                store,
            } => {
                let e = engine(&store)?;
                e.advance_producer_epoch(&producer, epoch, &actor, &reason, time()?)?;
                e.release()?;
                print(&json!({"producer":producer,"epoch":epoch}))
            }
            Self::RebaseRetentionClock(request) => request.execute(),
            Self::LowerStaleFloor {
                to_ms,
                actor,
                reason,
                confirm_floor_lowering,
                store,
            } => {
                anyhow::ensure!(confirm_floor_lowering, "--confirm-floor-lowering required");
                let e = engine(&store)?;
                let lowered = e.lower_stale_floor(to_ms, &actor, &reason, time()?);
                e.release()?;
                let (from_ms, to_ms) = lowered?;
                print(&json!({"from_ms":from_ms,"to_ms":to_ms}))
            }
            Self::RotateKeys { store } => {
                anyhow::ensure!(
                    store.protection_keys.is_some(),
                    "rotation requires --protection-keys"
                );
                let e = engine(&store)?;
                let revision = e.store.reencrypt_namespace(&e.namespace, &e.owner)?;
                e.release()?;
                print(&json!({"reencrypted":true,"revision":revision}))
            }
        }
    }
}

#[derive(Args)]
pub(super) struct ClockRebase {
    /// The corrected real time (ms): from now to a minute after it.
    #[arg(long)]
    to_ms: i64,
    /// The last correct host time before the pin (ms, in the past): a
    /// retry of a key the engine no longer remembers with a request time
    /// at or before it is refused as stale.
    #[arg(long)]
    stale_floor_ms: i64,
    #[arg(long)]
    actor: String,
    #[arg(long)]
    reason: String,
    /// Print what the rebase would change and commit nothing.
    #[arg(long, conflicts_with = "confirm_clock_rebase")]
    dry_run: bool,
    #[arg(long, required_unless_present = "dry_run")]
    confirm_clock_rebase: bool,
    #[command(flatten)]
    store: StoreArgs,
}
impl ClockRebase {
    fn execute(self) -> Result<()> {
        let Self {
            to_ms,
            stale_floor_ms,
            actor,
            reason,
            dry_run,
            confirm_clock_rebase,
            store,
        } = self;

        anyhow::ensure!(
            dry_run || confirm_clock_rebase,
            "--confirm-clock-rebase or --dry-run required"
        );
        let e = engine(&store)?;
        let report = if dry_run {
            e.rebase_retention_clock_dry_run(to_ms, stale_floor_ms, &actor, &reason, time()?)
        } else {
            e.rebase_retention_clock(to_ms, stale_floor_ms, &actor, &reason, time()?)
        };
        e.release()?;
        let report = report?;
        print(&json!({"dry_run":dry_run,"report":report}))
    }
}
