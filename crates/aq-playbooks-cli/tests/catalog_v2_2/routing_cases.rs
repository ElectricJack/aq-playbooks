#[derive(Clone)]
pub(super) struct Task {
    pub(super) filed_kind: &'static str,
    pub(super) hint: &'static str,
    pub(super) failures: i64,
    /// The judgment's `[kind, risk]`; `None` when it gave no answer.
    pub(super) labels: Option<[&'static str; 2]>,
    pub(super) flags: &'static [&'static str],
}
pub(super) struct Expected {
    pub(super) level: &'static str,
    pub(super) harnesses: &'static [&'static str],
    pub(super) prefer: &'static [&'static str],
    pub(super) hold: bool,
    pub(super) codes: &'static [&'static str],
}
pub(super) const NT: &[&str] = &["narrow", "covered_by_test"];
pub(super) const HARD: &[&str] = &["needs_deep_reasoning"];
const CC: &[&str] = &["claude", "codex"];
const ANY: &[&str] = &[];
pub(super) const fn task(
    kind: &'static str,
    risk: &'static str,
    flags: &'static [&'static str],
) -> Task {
    Task {
        filed_kind: kind,
        hint: "",
        failures: 0,
        labels: Some([kind, risk]),
        flags,
    }
}
const fn expect(
    level: &'static str,
    harnesses: &'static [&'static str],
    codes: &'static [&'static str],
) -> Expected {
    Expected {
        level,
        harnesses,
        prefer: &[],
        hold: false,
        codes,
    }
}

pub(super) const fn hint(task: Task, hint: &'static str) -> Task {
    Task { hint, ..task }
}
pub(super) const fn failed(task: Task, failures: i64) -> Task {
    Task { failures, ..task }
}
const fn unclassified(filed_kind: &'static str) -> Task {
    Task {
        labels: None,
        flags: &[],
        ..task(filed_kind, "", &[])
    }
}
const fn prefer(expected: Expected, prefer: &'static [&'static str]) -> Expected {
    Expected { prefer, ..expected }
}
const fn held(expected: Expected) -> Expected {
    Expected {
        hold: true,
        ..expected
    }
}

/// Each row is a concrete task and the route the owner's routing document
/// (aq-server `docs/specs/0011-routing-policy-default.md`, current text) gives
/// it; the comment quotes the sentence, with its line.
pub(super) const ROWS: &[(&str, Task, Expected)] = &[
    // 16: "Features, bug fixes, refactors, tests and docs are Standard, high effort."
    // 72: "Anything not mentioned here, takes only low risk work." Low risk: anyone.
    (
        "feature, low",
        task("feature", "low", &[]),
        expect("standard-high", ANY, &["level_from_kind"]),
    ),
    // 67: "High and very high risk work goes only to Claude or Codex."
    // 58: "Medium and high risk work is at least Standard, high effort."
    (
        "refactor, high",
        task("refactor", "high", &[]),
        expect(
            "standard-high",
            CC,
            &["level_from_kind", "harnesses_limited"],
        ),
    ),
    // 68: "Medium risk work goes to Claude or Codex."
    (
        "bugfix, medium",
        task("bugfix", "medium", &[]),
        expect(
            "standard-high",
            CC,
            &["level_from_kind", "harnesses_limited"],
        ),
    ),
    // 70: "Low risk work that is narrow and covered by a test should go to a local model first, ..."
    (
        "test, low, narrow and covered",
        task("test", "low", NT),
        prefer(
            expect(
                "standard-high",
                ANY,
                &["level_from_kind", "harness_preferred"],
            ),
            &["opencode"],
        ),
    ),
    (
        "docs, low, narrow and covered",
        task("docs", "low", NT),
        prefer(
            expect(
                "standard-high",
                ANY,
                &["level_from_kind", "harness_preferred"],
            ),
            &["opencode"],
        ),
    ),
    // 18: "Chores and syncs are Simple, high effort."
    (
        "chore, low",
        task("chore", "low", &[]),
        expect("simple-high", ANY, &["level_from_kind"]),
    ),
    // 18 + 58: a medium-risk sync is raised to the risk floor.
    (
        "sync, medium",
        task("sync", "medium", &[]),
        expect(
            "standard-high",
            CC,
            &["level_from_kind", "raised_for_risk", "harnesses_limited"],
        ),
    ),
    // 19: "Specs, designs, implementation plans and reviews of someone else's work are Complex, high effort."
    (
        "spec, medium",
        task("spec", "medium", &[]),
        expect(
            "complex-high",
            CC,
            &["level_from_kind", "harnesses_limited"],
        ),
    ),
    (
        "review, medium",
        task("review", "medium", &[]),
        expect(
            "complex-high",
            CC,
            &["level_from_kind", "harnesses_limited"],
        ),
    ),
    // 73: "Design and planning go to Claude first, and to Codex only when Claude can't take them."
    (
        "design, low, not flagged hard",
        task("design", "low", NT),
        prefer(
            expect(
                "complex-high",
                CC,
                &["level_from_kind", "harness_preferred"],
            ),
            &["claude"],
        ),
    ),
    (
        "plan, high",
        task("plan", "high", &[]),
        prefer(
            expect(
                "complex-high",
                CC,
                &["level_from_kind", "harness_preferred"],
            ),
            &["claude"],
        ),
    ),
    // 21: "Research is Standard, high effort, unless it is clearly harder, in which case it is Complex, high effort."
    (
        "research, low",
        task("research", "low", &[]),
        expect("standard-high", ANY, &["level_from_kind"]),
    ),
    (
        "research, low, clearly harder",
        task("research", "low", &["clearly_harder"]),
        expect("complex-high", ANY, &["level_from_kind"]),
    ),
    // 23: "Deep, high effort, is only for analysis, planning, spec writing, design or
    //      review that needs advanced mathematics or unusually hard reasoning."
    (
        "analysis, medium, flagged hard",
        task("analysis", "medium", HARD),
        expect("deep-high", CC, &["deep_reserved", "harnesses_limited"]),
    ),
    (
        "plan, medium, flagged hard",
        task("plan", "medium", HARD),
        prefer(
            expect("deep-high", CC, &["deep_reserved", "harness_preferred"]),
            &["claude"],
        ),
    ),
    (
        "spec, low, flagged hard",
        task("spec", "low", HARD),
        expect("deep-high", ANY, &["deep_reserved"]),
    ),
    (
        "design, low, flagged hard",
        task("design", "low", HARD),
        prefer(
            expect("deep-high", CC, &["deep_reserved", "harness_preferred"]),
            &["claude"],
        ),
    ),
    (
        "review, high, flagged hard",
        task("review", "high", HARD),
        expect("deep-high", CC, &["deep_reserved", "harnesses_limited"]),
    ),
    // 23: research is not among the Deep kinds; 21 gives it Complex only when clearly harder.
    (
        "research, low, flagged hard",
        task("research", "low", HARD),
        expect("standard-high", ANY, &["level_from_kind"]),
    ),
    // 27: "Analysis that doesn't need Deep is Standard, high effort."
    (
        "analysis, low",
        task("analysis", "low", &[]),
        expect("standard-high", ANY, &["level_from_kind"]),
    ),
    // 24: "A big task is not a Deep task, and neither is a risky one."
    (
        "review, very high, not flagged",
        task("review", "very_high", &[]),
        expect(
            "complex-high",
            CC,
            &["level_from_kind", "harnesses_limited"],
        ),
    ),
    // 75: "Art and visual design go to Codex, and wait for it rather than going elsewhere."
    (
        "art, medium",
        task("art", "medium", &[]),
        held(expect(
            "standard-high",
            &["codex"],
            &["level_from_kind", "held_for_harness"],
        )),
    ),
    (
        "art, low, narrow and covered",
        task("art", "low", NT),
        held(expect(
            "standard-high",
            &["codex"],
            &["level_from_kind", "held_for_harness"],
        )),
    ),
    // 78: "Repairs to a failed integration or build never go to a local model."
    (
        "repair, low, narrow and covered",
        task("repair", "low", NT),
        expect(
            "standard-high",
            &["claude", "codex", "gemini"],
            &["level_from_kind", "harness_excluded"],
        ),
    ),
    (
        "repair, medium, narrow and covered",
        task("repair", "medium", NT),
        expect(
            "standard-high",
            CC,
            &["level_from_kind", "harnesses_limited", "harness_excluded"],
        ),
    ),
    // 58: "Very high risk work is at least Complex, medium effort, ..."
    (
        "feature, very high",
        task("feature", "very_high", &[]),
        expect(
            "complex-medium",
            CC,
            &["level_from_kind", "raised_for_risk", "harnesses_limited"],
        ),
    ),
    // 59: "... unless it is narrow and covered by a test, in which case Standard, high effort is enough."
    (
        "feature, very high, narrow and covered",
        task("feature", "very_high", NT),
        expect(
            "standard-high",
            CC,
            &["level_from_kind", "risk_relaxed", "harnesses_limited"],
        ),
    ),
    // 68: "It may go to a local model only when the task is narrow, covered by a test, and Standard."
    (
        "refactor, medium, narrow and covered",
        task("refactor", "medium", NT),
        expect(
            "standard-high",
            &["claude", "codex", "opencode"],
            &["level_from_kind", "harnesses_limited"],
        ),
    ),
    (
        "refactor, medium, narrow and covered, asked for Complex",
        hint(task("refactor", "medium", NT), "complex-high"),
        expect("complex-high", CC, &["hint_respected", "harnesses_limited"]),
    ),
    // 36: "A bug fix that has already failed once for a real reason moves up to Complex, high effort."
    (
        "bugfix, high, no failure",
        task("bugfix", "high", &[]),
        expect(
            "standard-high",
            CC,
            &["level_from_kind", "harnesses_limited"],
        ),
    ),
    (
        "bugfix, high, one failure",
        failed(task("bugfix", "high", &[]), 1),
        expect(
            "complex-high",
            CC,
            &[
                "level_from_kind",
                "escalated_after_failure",
                "harnesses_limited",
            ],
        ),
    ),
    (
        "bugfix, high, two failures",
        failed(task("bugfix", "high", &[]), 2),
        expect(
            "complex-high",
            CC,
            &[
                "level_from_kind",
                "escalated_after_failure",
                "harnesses_limited",
            ],
        ),
    ),
    // Failures only escalate bug fixes.
    (
        "feature, low, one failure",
        failed(task("feature", "low", &[]), 1),
        expect("standard-high", ANY, &["level_from_kind"]),
    ),
    // 77: "Never spend Deep on bug fixes, escalated or not." The recorded reason says so.
    (
        "bugfix, asked for Deep, two failures, flagged hard",
        failed(hint(task("bugfix", "high", HARD), "deep-high"), 2),
        expect("complex-high", CC, &["policy", "harnesses_limited"]),
    ),
    // 29: "If whoever filed the task asked for a level, respect it, ..."
    (
        "feature, asked for Standard low",
        hint(task("feature", "low", &[]), "standard-low"),
        expect("standard-low", ANY, &["hint_respected"]),
    ),
    (
        "chore, asked for Simple low",
        hint(task("chore", "low", &[]), "simple-low"),
        expect("simple-low", ANY, &["hint_respected"]),
    ),
    // 58: the risk floor still applies to a level asked for; the hint did not decide it.
    (
        "bugfix, medium, asked for Simple low",
        hint(task("bugfix", "medium", &[]), "simple-low"),
        expect(
            "standard-high",
            CC,
            &["raised_for_risk", "harnesses_limited"],
        ),
    ),
    // 30: "... but don't go above Standard for tests, docs, chores and syncs, ..."
    (
        "docs, asked for Complex",
        hint(task("docs", "low", &[]), "complex-high"),
        expect("standard-high", ANY, &["hint_capped"]),
    ),
    (
        "sync, asked for Deep",
        hint(task("sync", "low", &[]), "deep-high"),
        expect("standard-high", ANY, &["hint_capped"]),
    ),
    // 30: "... or above Complex for ordinary implementation work." The cap is the reason.
    (
        "feature, asked for Deep",
        hint(task("feature", "low", &[]), "deep-high"),
        expect("complex-high", ANY, &["hint_capped"]),
    ),
    // 31: "Asking for Deep is not enough to get it: ... otherwise it gets Complex, high effort."
    (
        "review, asked for Deep, not flagged",
        hint(task("review", "low", &[]), "deep-high"),
        expect("complex-high", ANY, &["deep_reserved"]),
    ),
    (
        "analysis, asked for Deep, not flagged",
        hint(task("analysis", "low", &[]), "deep-high"),
        expect("complex-high", ANY, &["deep_reserved"]),
    ),
    (
        "analysis, asked for Deep low, flagged hard",
        hint(task("analysis", "low", HARD), "deep-low"),
        expect("deep-low", ANY, &["hint_respected"]),
    ),
    // 33: "If the level asked for isn't available, use the nearest available level below it."
    // It is then treated like any level asked for: it may be below the kind's level.
    (
        "feature, asked for Complex max (not offered)",
        hint(task("feature", "low", &[]), "complex-max"),
        expect("complex-high", ANY, &["policy"]),
    ),
    (
        "spec, asked for Standard max (not offered)",
        hint(task("spec", "low", &[]), "standard-max"),
        expect("standard-high", ANY, &["policy"]),
    ),
    (
        "chore, asked for Simple medium (not offered)",
        hint(task("chore", "low", &[]), "simple-medium"),
        expect("simple-low", ANY, &["policy"]),
    ),
    (
        "analysis, asked for Deep medium (not offered), flagged hard",
        hint(task("analysis", "low", HARD), "deep-medium"),
        expect("deep-low", ANY, &["policy"]),
    ),
    // 34: "The risk and failure rules still apply to it, as to any level asked for."
    (
        "docs, medium, asked for Simple medium (not offered)",
        hint(task("docs", "medium", &[]), "simple-medium"),
        expect(
            "standard-high",
            CC,
            &["policy", "raised_for_risk", "harnesses_limited"],
        ),
    ),
    (
        "bugfix, high, asked for Simple medium (not offered), one failure",
        failed(hint(task("bugfix", "high", &[]), "simple-medium"), 1),
        expect(
            "complex-high",
            CC,
            &["policy", "escalated_after_failure", "harnesses_limited"],
        ),
    ),
    // 30 with 33: a kind cap applies before the nearest available level is taken.
    (
        "docs, asked for Complex max (not offered)",
        hint(task("docs", "low", &[]), "complex-max"),
        expect("standard-high", ANY, &["hint_capped"]),
    ),
    // 86: "An unknown risk counts as medium, and you say that it was assumed."
    // 89: "Keep the task on Claude or Codex." (no local model, though narrow and covered)
    (
        "feature, unknown risk, narrow and covered",
        task("feature", "unknown", NT),
        expect(
            "standard-high",
            CC,
            &["level_from_kind", "risk_assumed", "harnesses_limited"],
        ),
    ),
    // 88: "An unknown kind gets at least Standard, high effort."
    (
        "unknown kind, low, narrow and covered",
        Task {
            labels: Some(["unknown", "low"]),
            ..task("", "low", NT)
        },
        expect("standard-high", CC, &["level_from_kind", "policy"]),
    ),
    // 88: the unknown-kind floor applies over a low level asked for, as the risk floors do.
    (
        "unknown kind, low, asked for Simple low",
        Task {
            labels: Some(["unknown", "low"]),
            ..hint(task("", "low", &[]), "simple-low")
        },
        expect("standard-high", CC, &["policy"]),
    ),
    // 88: "A risk that is known still applies in full."
    (
        "unknown kind, very high",
        Task {
            labels: Some(["unknown", "very_high"]),
            ..task("", "very_high", &[])
        },
        expect(
            "complex-medium",
            CC,
            &[
                "level_from_kind",
                "raised_for_risk",
                "policy",
                "harnesses_limited",
            ],
        ),
    ),
    (
        "unknown kind, unknown risk",
        Task {
            labels: Some(["unknown", "unknown"]),
            ..task("", "unknown", &[])
        },
        expect(
            "standard-high",
            CC,
            &[
                "level_from_kind",
                "risk_assumed",
                "policy",
                "harnesses_limited",
            ],
        ),
    ),
    // 87: "Use what is known." The task could not be classified at all: the kind
    // whoever filed it gave, the level asked for and the failures still apply.
    (
        "unclassified, filed as a bug fix that failed once",
        failed(unclassified("bugfix"), 1),
        expect(
            "complex-high",
            CC,
            &[
                "classification_failed",
                "level_from_kind",
                "escalated_after_failure",
                "risk_assumed",
                "harnesses_limited",
            ],
        ),
    ),
    (
        "unclassified, filed as a chore",
        unclassified("chore"),
        expect(
            "standard-high",
            CC,
            &[
                "classification_failed",
                "level_from_kind",
                "raised_for_risk",
                "risk_assumed",
                "harnesses_limited",
            ],
        ),
    ),
    (
        "unclassified, no kind given",
        unclassified(""),
        expect(
            "standard-high",
            CC,
            &[
                "classification_failed",
                "level_from_kind",
                "risk_assumed",
                "policy",
                "harnesses_limited",
            ],
        ),
    ),
    // 70 + host intersection: a local model first, but none is offered at Complex.
    (
        "spec, low, narrow and covered",
        task("spec", "low", NT),
        prefer(
            expect(
                "complex-high",
                ANY,
                &["level_from_kind", "harness_preferred"],
            ),
            &["opencode"],
        ),
    ),
];
