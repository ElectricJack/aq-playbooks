//! Report draft cost bounds and missing-value diagnostics after the derivation.
use super::*;
fn count(size: u64) -> String {
    if size == u64::MAX {
        "too many".into()
    } else {
        size.to_string()
    }
}
impl ExprSize {
    /// "`R` stands for 4100 nodes and is read 40 times".
    fn most(&self) -> String {
        let Some((name, charge)) = &self.most else {
            return String::new();
        };
        format!(
            " Most of it is `{name}` (counted at the size of the type it reads): up to {} nodes, \
             read {} time{}, {} in all.",
            count(charge.each),
            charge.times,
            if charge.times == 1 { "" } else { "s" },
            count(charge.total)
        )
    }
}

/// How much of the cost bounds a draft uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSize {
    /// The largest expression's count, against `MAX_DRAFT_EXPANSION`.
    pub largest_expression: u64,
    /// Where that expression is, as a diagnostic names it.
    pub largest_at: String,
    /// All expressions together, against `MAX_DRAFT_EXPANSION_TOTAL`.
    pub expressions: u64,
    /// Type nodes in scope over all steps, against `MAX_DRAFT_SCOPE`.
    pub scope: u64,
}
fn scope_total(derived: &Derived) -> u64 {
    derived.scope.iter().fold(0u64, |n, (_, steps, each)| {
        n.saturating_add(steps.saturating_mul(*each))
    })
}

impl ArtifactContent {
    /// What the draft counts against each cost bound.
    pub fn draft_size(&self) -> DraftSize {
        let sources = Sources::new(self);
        let derived = Check::new(self, &sources).derived();
        let largest = derived.sizes.iter().max_by_key(|s| s.size);
        DraftSize {
            largest_expression: largest.map_or(0, |s| s.size),
            largest_at: largest
                .map(|s| format!("{}.{}", s.path, s.at))
                .unwrap_or_default(),
            expressions: derived.total,
            scope: scope_total(&derived),
        }
    }
    /// Bounds a new draft is held to before anything costly runs on it: the
    /// expanded size of each expression and of all together, and the types
    /// in scope over all steps. Typing and the read check follow every copy
    /// of a value, so such a draft is refused before either runs.
    pub fn draft_expansion_issues(&self) -> Vec<ValidationIssue> {
        let sources = Sources::new(self);
        let derived = Check::new(self, &sources).derived();
        derived.expansion_issues()
    }
    /// Issues only a new draft is held to. `reported` are the issues
    /// `validate_structure` already found; a read it already refused is not
    /// reported again.
    pub fn draft_issues(&self, reported: &[ValidationIssue]) -> Vec<ValidationIssue> {
        let mut issues = vec![];
        for (name, step) in &self.playbook.steps {
            self.check_join_members(name, step, &mut issues);
        }
        let sources = Sources::new(self);
        let derived = Check::new(self, &sources).derived();
        issues.extend(derived.limits);
        for (path, place, message) in derived.reads {
            let refused = reported
                .iter()
                .chain(&issues)
                .any(|i| i.path == path && i.message.starts_with(&format!("{path}.{place}:")));
            if !refused {
                issue(
                    &mut issues,
                    "expression_type",
                    &path,
                    format!("{path}.{place}: {message}"),
                );
            }
        }
        issues
    }
    fn check_join_members(&self, name: &str, step: &Step, issues: &mut Vec<ValidationIssue>) {
        let Operation::Join {
            members,
            member_keys,
            ..
        } = &step.op
        else {
            return;
        };
        for (i, member) in members.iter().enumerate() {
            if let Some(error) = self.handle_member_path(member) {
                let key = member_keys
                    .get(i)
                    .map(|k| format!(" (key {k})"))
                    .unwrap_or_default();
                issue(
                    issues,
                    "join_member_type",
                    &format!("steps.{name}"),
                    format!("members[{i}]{key}: {error}"),
                );
            }
        }
    }
}
const COUNTED: &str = "with every reference counted at the size of the type it reads";

impl Derived {
    fn expansion_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = self.limits.clone();
        if issues.is_empty() {
            self.individual_expansions(&mut issues);
        }
        if issues.is_empty() {
            self.total_expansion(&mut issues);
        }
        if issues.is_empty() {
            self.scope_expansion(&mut issues);
        }
        issues
    }
    fn individual_expansions(&self, issues: &mut Vec<ValidationIssue>) {
        for sized in &self.sizes {
            if sized.size > MAX_DRAFT_EXPANSION {
                issue(
                    issues,
                    "expression_limit",
                    &sized.path,
                    format!(
                        "{}.{}: expands to {} nodes {COUNTED}; an expression may expand to at \
                         most {MAX_DRAFT_EXPANSION}.{} Build each value from a name once, not \
                         twice at every level, and read the one field you need rather than a \
                         whole binding or table.",
                        sized.path,
                        sized.at,
                        count(sized.size),
                        sized.most()
                    ),
                );
            }
        }
    }
    fn total_expansion(&self, issues: &mut Vec<ValidationIssue>) {
        let total = self.total;
        if total > MAX_DRAFT_EXPANSION_TOTAL {
            let largest = self.sizes.iter().max_by_key(|s| s.size);
            let largest = largest
                .map(|s| {
                    format!(
                        " The largest is {}.{}, {} nodes.{}",
                        s.path,
                        s.at,
                        count(s.size),
                        s.most()
                    )
                })
                .unwrap_or_default();
            issue(
                issues,
                "expression_limit",
                "playbook",
                format!(
                    "the draft's expressions expand to {} nodes in all {COUNTED} (an expression \
                     counts once for each entry that reaches its step); a draft may expand to at \
                     most {MAX_DRAFT_EXPANSION_TOTAL}.{largest} Build each value from a name \
                     once, or shorten the draft.",
                    count(total)
                ),
            );
        }
    }
    fn scope_expansion(&self, issues: &mut Vec<ValidationIssue>) {
        let in_scope = scope_total(self);
        if in_scope > MAX_DRAFT_SCOPE {
            let entries: Vec<String> = self
                .scope
                .iter()
                .map(|(entry, steps, each)| {
                    format!("entry `{entry}` reaches {steps} steps with {each} at each")
                })
                .take(8)
                .collect();
            issue(
                issues,
                "expression_limit",
                "playbook",
                format!(
                    "the draft's steps have {} type nodes in scope in all (for each step an \
                     entry reaches: the type of the entry's `input`, the captured definitions, \
                     and the result type of every step that binds one); a draft may have at most \
                     {MAX_DRAFT_SCOPE}: {}. Use fewer steps, fewer steps that bind a result, a \
                     smaller `input` type, or fewer entries over the same steps.",
                    count(in_scope),
                    entries.join("; ")
                ),
            );
        }
    }
}
