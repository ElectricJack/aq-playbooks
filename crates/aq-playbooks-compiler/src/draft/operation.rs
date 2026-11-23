//! Draft operations use the model's exact shape, with bare names in the two contract fields.
use super::*;
use serde::{Deserializer, Serializer};
#[derive(Serialize, Deserialize)]
#[serde(
    remote = "Operation",
    expecting = "internally tagged enum DraftOperation",
    rename = "DraftOperation",
    tag = "kind",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(super) enum DraftOperation {
    Invoke {
        #[serde(with = "name")]
        command: ContractRef,
        input: Expr,
        bind: String,
        retry: Option<RetryPolicy>,
    },
    Decide {
        predicate: Expr,
    },
    Wait {
        #[serde(default, with = "optional_name")]
        event: Option<ContractRef>,
        correlation: BTreeMap<String, Expr>,
        minimum_revision: Option<Expr>,
        deadline_ms: u64,
        bind: String,
    },
    ForEach {
        collection: Expr,
        item: String,
        body: String,
        next: String,
        max_items: usize,
        concurrency: u32,
        on_failure: IterationFailure,
    },
    Call {
        dependency: String,
        entry: String,
        input: Expr,
        mode: CallMode,
        bind: String,
    },
    Join {
        members: Vec<Expr>,
        #[serde(default)]
        member_keys: Vec<String>,
        mode: JoinMode,
        accepted_outcomes: BTreeSet<String>,
        deadline_ms: u64,
        remaining: RemainingPolicy,
        bind: String,
    },
    Return {
        outcome: String,
        output: Expr,
    },
}

fn unresolved(name: String) -> ContractRef {
    ContractRef {
        name,
        fingerprint: String::new(),
    }
}

mod name {
    use super::*;
    pub(super) fn serialize<S: Serializer>(
        reference: &ContractRef,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        reference.name.serialize(s)
    }
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ContractRef, D::Error> {
        String::deserialize(d).map(unresolved)
    }
}

mod optional_name {
    use super::*;
    pub(super) fn serialize<S: Serializer>(
        reference: &Option<ContractRef>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        reference.as_ref().map(|r| &r.name).serialize(s)
    }
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<ContractRef>, D::Error> {
        Option::<String>::deserialize(d).map(|v| v.map(unresolved))
    }
}
