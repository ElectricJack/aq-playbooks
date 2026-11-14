//! Documents shipped to the authoring agent with its workspace: the guide
//! ([`GUIDE`], written into the workspace as `GUIDE.md`), the language
//! reference ([`LANGUAGE`], `LANGUAGE.md`), the stored format ([`FORMAT`],
//! `FORMAT.md`) and the worked examples ([`example_files`], under
//! `examples/`). Every command, draft and case the two documents show is run
//! by the tests (`tests/docs.rs` here, and `authoring_examples.rs` in the
//! CLI), so they cannot drift from the tools.
//!
//! [`example_files`] holds the worked example workspaces in this crate's
//! `authoring-examples` directory: complete workspaces an agent can read, and
//! run the authoring tools in, to see how a document becomes a playbook.
//! They live inside the crate, so that a copy of `crates/` builds it.
//! `aq-playbooks examples --output DIR` writes them out.

/// How to turn a document into a playbook: the workflow, when to ask or give
/// up, the rules for a draft, the case format and the limits.
pub const GUIDE: &str = include_str!("../../../skills/aq-playbook-author/SKILL.md");
/// Every field, operation, expression, limit and diagnostic code of a draft.
pub const LANGUAGE: &str = include_str!("../LANGUAGE.md");
/// The portable format of a stored artifact, and the rules a new draft is
/// held to.
pub const FORMAT: &str = include_str!("../../aq-playbooks-model/FORMAT.md");

/// Every file of the worked examples, as `(path, content)`. A path is
/// relative to `authoring-examples`, such as `"notify-when-ready/draft.json"`;
/// its first component names the example.
pub fn example_files() -> Vec<(&'static str, &'static str)> {
    macro_rules! files {
        ($($path:literal),* $(,)?) => {
            vec![$(($path, include_str!(concat!("../authoring-examples/", $path)))),*]
        };
    }
    files![
        "escalate-on-judgment/cases/assumed-route-refused.json",
        "escalate-on-judgment/cases/assumed-route-stale.json",
        "escalate-on-judgment/cases/failed-three-times.json",
        "escalate-on-judgment/cases/failed-twice.json",
        "escalate-on-judgment/cases/high-risk.json",
        "escalate-on-judgment/cases/judgment-fails.json",
        "escalate-on-judgment/cases/judgment-never-answers.json",
        "escalate-on-judgment/cases/low-risk.json",
        "escalate-on-judgment/cases/medium-risk-small-and-tested.json",
        "escalate-on-judgment/cases/medium-risk.json",
        "escalate-on-judgment/cases/nothing-offers-the-level.json",
        "escalate-on-judgment/cases/owner-chose.json",
        "escalate-on-judgment/cases/risk-unknown.json",
        "escalate-on-judgment/cases/task-changed.json",
        "escalate-on-judgment/cases/task-gone.json",
        "escalate-on-judgment/cases/unroutable-refused.json",
        "escalate-on-judgment/cases/unroutable-stale.json",
        "escalate-on-judgment/catalog.json",
        "escalate-on-judgment/draft.json",
        "escalate-on-judgment/facts.json",
        "escalate-on-judgment/previous.artifact.json",
        "escalate-on-judgment/record.json",
        "escalate-on-judgment/source.md",
        "escalate-on-judgment/stuck/deep-level.json",
        "escalate-on-judgment/stuck/deep-level.md",
        "notify-when-ready/cases/changed-and-still-bare.json",
        "notify-when-ready/cases/described-meanwhile.json",
        "notify-when-ready/cases/finished-during-the-wait.json",
        "notify-when-ready/cases/message-refused.json",
        "notify-when-ready/cases/still-no-description.json",
        "notify-when-ready/cases/task-cancelled.json",
        "notify-when-ready/cases/task-finished.json",
        "notify-when-ready/cases/task-gone.json",
        "notify-when-ready/cases/task-not-readable.json",
        "notify-when-ready/catalog.json",
        "notify-when-ready/draft.json",
        "notify-when-ready/facts.json",
        "notify-when-ready/previous.artifact.json",
        "notify-when-ready/record.json",
        "notify-when-ready/source.md",
        "notify-when-ready/stuck/how-long.json",
        "notify-when-ready/stuck/how-long.md",
        "notify-when-ready/stuck/phone-call.json",
        "notify-when-ready/stuck/phone-call.md",
    ]
}
