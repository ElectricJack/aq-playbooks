use super::bounds::past_the_bound;
use super::*;

#[test]
fn every_draft_snippet_checks_as_shown() {
    let shown: usize = DOCUMENTS
        .into_iter()
        .map(|(name, doc)| checked_blocks(name, &blocks(doc)))
        .sum();
    assert!(shown >= 15, "only {shown} draft snippets were checked");
}

fn checked_blocks(name: &str, found: &[Block]) -> usize {
    let mut shown = 0;
    for (i, block) in found.iter().enumerate() {
        let at = format!("{name} line {}", block.line);
        match role(block) {
            ("json", "draft") => {
                let next = found
                    .get(i + 1)
                    .filter(|b| role(b) == ("json", "diagnostic"));
                check_draft(block, next, &at);
                shown += 1;
            }
            ("json", "diagnostic") => {
                let follows = i
                    .checked_sub(1)
                    .and_then(|index| found.get(index))
                    .is_some_and(|prev| role(prev) == ("json", "draft"));
                shown += usize::from(check_diagnostic(block, follows, &at));
            }
            ("json", "") => panic!("{at}: give every json block a role"),
            _ => {}
        }
    }
    shown
}

fn check_draft(block: &Block, next: Option<&Block>, at: &str) {
    let example = block
        .info
        .get(2)
        .unwrap_or_else(|| panic!("{at}: name the example"));
    let snippet: Value = serde_json::from_str(&block.body).unwrap_or_else(|e| panic!("{at}: {e}"));
    let mut ws = workspace(example);
    patch(&mut ws.draft, &snippet);
    let got = diagnostics(&ws);
    match next {
        Some(want) => {
            let want: Value = serde_json::from_str(&want.body).unwrap();
            assert!(
                got.iter().any(|d| fits(&want, d)),
                "{at}: no diagnostic is {want:#}; the check says {got:#?}"
            );
        }
        None => assert!(got.is_empty(), "{at}: shown as right, but {got:#?}"),
    }
}

fn check_diagnostic(block: &Block, follows: bool, at: &str) -> bool {
    let want: Value = serde_json::from_str(&block.body).unwrap_or_else(|e| panic!("{at}: {e}"));
    let Some(limit) = option(block, "limit") else {
        assert!(follows, "{at}: a diagnostic follows its draft");
        return false;
    };
    let got = diagnostics(&past_the_bound(limit));
    assert!(
        got.iter().any(|d| fits(&want, d)),
        "{at}: past the {limit} bound the check says {got:#?}"
    );
    true
}
