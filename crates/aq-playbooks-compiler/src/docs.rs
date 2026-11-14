//! Documents shipped to the authoring agent with its workspace.
//!
//! [`example_files`] holds the worked example workspaces of
//! `docs/authoring/examples`: complete workspaces an agent can read, and run
//! the authoring tools in, to see how a document becomes a playbook.
//! `aq-playbooks examples --output DIR` writes them out.

/// Every file of the worked examples, as `(path, content)`. A path is
/// relative to `docs/authoring/examples`, such as
/// `"notify-when-ready/draft.json"`; its first component names the example.
pub fn example_files() -> Vec<(&'static str, &'static str)> {
    macro_rules! files {
        ($($path:literal),* $(,)?) => {
            vec![$(($path, include_str!(concat!("../../../docs/authoring/examples/", $path)))),*]
        };
    }
    files![
        "escalate-on-judgment/cases/failed-three-times.json",
        "escalate-on-judgment/cases/high-risk.json",
        "escalate-on-judgment/cases/judgment-fails.json",
        "escalate-on-judgment/cases/judgment-never-answers.json",
        "escalate-on-judgment/cases/low-risk.json",
        "escalate-on-judgment/cases/medium-risk-small-and-tested.json",
        "escalate-on-judgment/cases/medium-risk.json",
        "escalate-on-judgment/cases/owner-chose.json",
        "escalate-on-judgment/cases/risk-unknown.json",
        "escalate-on-judgment/cases/task-changed.json",
        "escalate-on-judgment/cases/task-gone.json",
        "escalate-on-judgment/catalog.json",
        "escalate-on-judgment/draft.json",
        "escalate-on-judgment/facts.json",
        "escalate-on-judgment/previous.artifact.json",
        "escalate-on-judgment/record.json",
        "escalate-on-judgment/source.md",
        "escalate-on-judgment/stuck/deep-level.json",
        "escalate-on-judgment/stuck/deep-level.md",
        "notify-when-ready/cases/message-refused.json",
        "notify-when-ready/cases/owner-chose.json",
        "notify-when-ready/cases/still-waiting.json",
        "notify-when-ready/cases/taken-meanwhile.json",
        "notify-when-ready/cases/task-finished.json",
        "notify-when-ready/cases/task-gone.json",
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
