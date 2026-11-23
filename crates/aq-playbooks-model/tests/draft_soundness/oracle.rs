use super::*;

/// Whether the draft must be refused: the oracle. It follows the one fact
/// that matters, whether the value is Missing, through the combinators.
fn must_refuse(base: &Base, wraps: &[Wrap], split: usize) -> bool {
    if base.sound {
        return false;
    }
    if base.needed_inside {
        return true;
    }
    let mut missing = base.carrier_missing;
    for (i, wrap) in wraps.iter().enumerate() {
        if i == split {
            missing = true;
        }
        match wrap.missing() {
            Missing::Carries => {}
            Missing::Needs if missing => return true,
            Missing::Needs => {}
            Missing::Tests => return false,
        }
    }
    // Read at last where its value is needed.
    true
}
/// The reasons of every refused read: "…this step: <reasons>. Read it only…".
fn reasons(issues: &[ValidationIssue]) -> BTreeSet<String> {
    issues
        .iter()
        .filter(|i| i.code == "expression_type")
        .filter_map(|i| i.message.split_once("this step: "))
        .filter_map(|(_, rest)| rest.split_once(". Read it only"))
        .flat_map(|(reasons, _)| reasons.split("; ").map(str::to_string))
        .collect()
}

pub(super) const ACCEPTED: &str = ": an unsound read is accepted";
pub(super) const OTHER_REASONS: &str = ": refused for ";
pub(super) const SOUND_REFUSED: &str = ": a sound read is refused: ";
pub(super) const TYPING_REFUSES: &str = ": ordinary typing refuses: ";

#[derive(Default)]
pub(super) struct Tally {
    pub(super) cases: usize,
    pub(super) must_refuse: usize,
    pub(super) must_accept: usize,
    pub(super) typed: usize,
    pub(super) typing_stricter: usize,
    pub(super) failures: Vec<String>,
}
impl Tally {
    pub(super) fn add(&mut self, other: Tally) {
        self.cases += other.cases;
        self.must_refuse += other.must_refuse;
        self.must_accept += other.must_accept;
        self.typed += other.typed;
        self.typing_stricter += other.typing_stricter;
        self.failures.extend(other.failures);
    }
}

fn direct_read(base: &Base, site: Site, content: &mut ArtifactContent) -> BTreeSet<String> {
    let mut built = Built::new(base.carrier.clone(), true, site);
    built.project(base.path);
    built.install(site, content);
    let issues = content.draft_issues(&[]);
    assert_eq!(
        content.validate_structure(),
        vec![],
        "{}: ordinary typing accepts the direct read",
        base.name
    );
    assert_eq!(
        issues.is_empty(),
        base.sound,
        "{}: the direct read: {issues:?}",
        base.name
    );
    reasons(&issues)
}

fn sequences(depth: usize, first: Option<Wrap>) -> Vec<Vec<Wrap>> {
    let Some(first) = first else {
        return vec![vec![]];
    };
    let mut last = vec![vec![first]];
    let mut all = last.clone();
    for _ in 1..depth {
        let mut next = vec![];
        for sequence in &last {
            for wrap in WRAPS {
                let mut longer = sequence.clone();
                longer.push(wrap);
                next.push(longer);
            }
        }
        all.extend(next.iter().cloned());
        last = next;
    }
    all
}

struct Composition<'a> {
    base: &'a Base,
    site: Site,
    wraps: &'a [Wrap],
    split: usize,
}
impl Composition<'_> {
    fn description(&self) -> String {
        format!(
            "{} ({:?}): {:?}, path read after {}",
            self.base.name, self.site, self.wraps, self.split
        )
    }

    fn build(&self) -> Option<Built> {
        let mut built = Built::new(self.base.carrier.clone(), false, self.site);
        for (i, wrap) in self.wraps.iter().enumerate() {
            if i == self.split {
                built.project(self.base.path);
            }
            if !built.wrap(*wrap, i) {
                return None;
            }
        }
        if self.split == self.wraps.len() {
            built.project(self.base.path);
        }
        Some(built)
    }

    /// Check all single wrappers and sample deeper compositions, where ordinary
    /// typing costs about ten times the draft check.
    fn check_typing(&self, content: &ArtifactContent, tally: &mut Tally) -> bool {
        let sampled = match self.wraps.len() {
            0 | 1 => true,
            2 => tally.cases % 11 == 0,
            _ => tally.cases % 397 == 0,
        };
        if !sampled {
            return true;
        }
        tally.typed += 1;
        let ordinary = content.validate_structure();
        if ordinary.is_empty() {
            return true;
        }
        // Ordinary typing gives an item the type its collection has anywhere,
        // so a join's answer carried as an item may already type as nullable.
        if self.base.loose_item && self.wraps.contains(&Wrap::ForEachItem) {
            tally.typing_stricter += 1;
            return true;
        }
        tally.failures.push(format!(
            "{}{TYPING_REFUSES}{ordinary:?}",
            self.description()
        ));
        false
    }

    fn check_issues(
        &self,
        issues: &[ValidationIssue],
        direct: &BTreeSet<String>,
        tally: &mut Tally,
    ) {
        if !must_refuse(self.base, self.wraps, self.split) {
            tally.must_accept += 1;
            if !issues.is_empty() {
                tally
                    .failures
                    .push(format!("{}{SOUND_REFUSED}{issues:?}", self.description()));
            }
            return;
        }
        tally.must_refuse += 1;
        if issues.iter().all(|i| i.code != "expression_type") {
            tally
                .failures
                .push(format!("{}{ACCEPTED}", self.description()));
            return;
        }
        let found = reasons(issues);
        let subset = self.base.two_causes && found.is_subset(direct);
        if &found != direct && !subset {
            tally.failures.push(format!(
                "{}{OTHER_REASONS}{found:?}, the direct read for {direct:?}",
                self.description()
            ));
        }
    }

    fn check(&self, content: &mut ArtifactContent, direct: &BTreeSet<String>, tally: &mut Tally) {
        let Some(built) = self.build() else { return };
        built.install(self.site, content);
        tally.cases += 1;
        if self.check_typing(content, tally) {
            self.check_issues(&content.draft_issues(&[]), direct, tally);
        }
    }
}

/// Every composition of up to `depth` combinators that starts with `first`,
/// with the path read after each possible value-preserving prefix.
pub(super) fn check_base(base: &Base, depth: usize, first: Option<Wrap>, site: Site) -> Tally {
    let mut tally = Tally::default();
    let mut content = base.content.clone();
    let direct = direct_read(base, site, &mut content);
    for wraps in sequences(depth, first) {
        // A trigger has no step before the expression.
        if site == Site::Trigger
            && wraps
                .iter()
                .any(|w| matches!(w, Wrap::LaterStep | Wrap::ForEachItem))
        {
            continue;
        }
        let prefix = if base.path.is_empty() {
            0
        } else {
            wraps.iter().take_while(|w| w.same_value()).count()
        };
        for split in 0..=prefix {
            Composition {
                base,
                site,
                wraps: &wraps,
                split,
            }
            .check(&mut content, &direct, &mut tally);
        }
    }
    tally
}
