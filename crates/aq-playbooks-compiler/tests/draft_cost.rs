//! A generated test of the cost bound: whatever a draft under the bounds
//! holds, checking it, and verifying what it seals into, stay within a fixed
//! budget of time and memory.
//!
//! Seeded, so every run checks the same drafts. Each draft is built to stress
//! one of the ways cost grows (a large type shared through many references,
//! names that double, large tables, many steps with much in scope, loops) at
//! a random size around the bounds: some fit and seal, the rest are refused.
use aq_playbooks_compiler::draft::*;
use aq_playbooks_model::*;
use serde_json::{Map, Value, json};
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Counts the bytes in use, to measure the peak of one call.
struct Counting;
static IN_USE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let now = IN_USE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(now, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        IN_USE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;
/// One measurement at a time: the counts are the whole process's.
static MEASURING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What one call cost.
#[derive(Clone, Copy, Default, Debug)]
struct Cost {
    time: Duration,
    /// Peak bytes in use above what was in use when it started.
    bytes: usize,
}
impl Cost {
    fn max(self, other: Cost) -> Cost {
        Cost {
            time: self.time.max(other.time),
            bytes: self.bytes.max(other.bytes),
        }
    }
}
fn measured<T>(f: impl FnOnce() -> T) -> (T, Cost) {
    let before = IN_USE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let start = Instant::now();
    let result = f();
    let time = start.elapsed();
    let bytes = PEAK.load(Ordering::Relaxed).saturating_sub(before);
    (result, Cost { time, bytes })
}

/// The budget, in a release build: checking a draft under the bounds, and
/// verifying the artifact it seals into (as every load does).
const CHECK_BUDGET: Cost = Cost {
    time: Duration::from_millis(2_000),
    bytes: 256 << 20,
};
const VERIFY_BUDGET: Cost = Cost {
    time: Duration::from_millis(1_500),
    bytes: 256 << 20,
};

#[path = "draft_cost/generate.rs"]
mod generate;
use generate::{Rng, decide, draft, reference, ret, wide};
/// The reviewer's draft (A2-R3-I2): a 4,972-node `input` type shared through
/// 1,936 references in one expression, 198 KB, under every earlier bound.
fn shared_input() -> Workspace {
    let mut rng = Rng(1);
    let mut ws = draft(&mut rng);
    let mut outer = Map::new();
    for i in 0..44 {
        let mut inner = Map::new();
        for j in 0..44 {
            inner.insert(format!("g{j}"), reference("input"));
        }
        outer.insert(format!("f{i}"), json!({"op": "object", "fields": inner}));
    }
    ws.draft["entries"]["main"]["input"] = wide(70, 70);
    ws.draft["steps"] = json!({
        "s0": decide(json!({"op": "object", "fields": outer}), "done"),
        "done": ret("done"),
        "failed": ret("failed"),
    });
    ws
}

#[test]
fn a_large_type_shared_through_many_references_is_refused() {
    let _alone = MEASURING.lock().unwrap_or_else(|e| e.into_inner());
    let ws = shared_input();
    let bytes = serde_json::to_string(&ws.draft).unwrap().len();
    assert!(bytes < MAX_DRAFT_JSON_BYTES, "{bytes}");
    let (diagnostics, cost) = measured(|| check(&ws));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(
        (d.code.as_str(), d.path.as_str()),
        ("expression_limit", "draft.json:steps.s0")
    );
    // The message names what is responsible and the numbers: the type of
    // `input` is 70 objects of 70 fields, the object around them and `task`.
    let input = 70 * 71 + 2;
    let references = 44 * 44;
    let all = input * references;
    for text in [
        // The references, the 45 objects that hold them, and `exists`.
        format!("steps.s0.predicate: expands to {} nodes", all + 46),
        "an expression may expand to at most 200000".to_string(),
        "Most of it is `input`".to_string(),
        format!("up to {input} nodes, read {references} times, {all} in all"),
    ] {
        assert!(d.message.contains(&text), "{text}: {}", d.message);
    }
    println!("the shared-input draft ({bytes} bytes) is refused in {cost:?}");
    if !cfg!(debug_assertions) {
        assert!(cost.time < Duration::from_millis(100), "{cost:?}");
    }
}

#[path = "draft_cost/bounds.rs"]
mod bounds;
use bounds::at_the_bounds;

/// Literal rows that each have a different field, read through a `map` item
/// in many places: every read has to follow every row's shape.
fn many_shapes(rows: usize, reads: usize) -> Workspace {
    let table: Vec<Value> = (0..rows).map(|i| json!({format!("a{i}"): 1})).collect();
    let read = json!({"op": "exists", "value": {"op": "ref", "root": "r", "path": ["a0"]}});
    let mut ws = draft(&mut Rng(7));
    ws.draft["steps"] = json!({
        "s0": decide(json!({"op": "map", "item": "r", "max_items": rows,
            "collection": {"op": "literal", "value": table},
            "value": {"op": "array", "items": vec![read; reads]}}), "done"),
        "done": ret("done"),
    });
    ws
}

#[test]
fn reading_a_value_of_many_shapes_many_times_is_refused_before_the_work_is_done() {
    let _alone = MEASURING.lock().unwrap_or_else(|e| e.into_inner());
    // 30 000 shapes read 6 000 times would be 180 million steps.
    let ws = many_shapes(30_000, 6_000);
    let bytes = serde_json::to_string(&ws.draft).unwrap().len();
    assert!(bytes < MAX_DRAFT_JSON_BYTES, "{bytes}");
    let (diagnostics, cost) = measured(|| check(&ws));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].code, "draft_limit", "{diagnostics:#?}");
    assert!(
        diagnostics[0].message.contains("many different shapes"),
        "{diagnostics:#?}"
    );
    println!("the many-shapes draft ({bytes} bytes) is refused in {cost:?}");
    assert!(cost.bytes < CHECK_BUDGET.bytes, "{cost:?}");
    if !cfg!(debug_assertions) {
        assert!(cost.time < CHECK_BUDGET.time, "{cost:?}");
    }
    // A table of a thousand such rows read in a hundred places is checked
    // as usual (the reads here only test with `exists`, so it checks clean).
    let diagnostics = check(&many_shapes(1_000, 100));
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
}

#[test]
fn checking_and_verifying_any_draft_under_the_bounds_stays_within_budget() {
    let _alone = MEASURING.lock().unwrap_or_else(|e| e.into_inner());
    // Fewer in a debug build, where only the memory budget is held.
    let drafts = if cfg!(debug_assertions) { 12 } else { 400 };
    let mut rng = Rng(0x00C0_FFEE_D00D_F00D);
    let mut totals = Measurements::default();
    let directed = at_the_bounds();
    let aimed = directed.len();
    let generated = (0..drafts).map(|_| (None, draft(&mut rng)));
    let all = directed
        .into_iter()
        .map(|(name, ws)| (Some(name), ws))
        .chain(generated);
    for (n, (name, ws)) in all.enumerate() {
        let sample = Sample {
            n,
            name,
            bytes: serde_json::to_string(&ws.draft).unwrap().len(),
        };
        let (diagnostics, cost) = measured(|| check(&ws));
        sample.validate(&diagnostics);
        if diagnostics.is_empty() {
            sample.verify(&ws, cost, &mut totals);
        } else {
            sample.refused(cost, &mut totals);
        }
    }
    let Measurements {
        sealed,
        refused,
        too_long,
        worst_check,
        worst_refusal,
        worst_verify,
        largest,
    } = totals;
    println!(
        "cost: {aimed} drafts at the bounds and {drafts} generated: {sealed} sealed (the largest {largest} bytes), {refused} refused \
         ({too_long} for their length); worst check {worst_check:?}, worst refusal \
         {worst_refusal:?}, worst verify {worst_verify:?}"
    );
    // Both sides of the bounds are exercised.
    assert!(
        (sealed - aimed) * 4 >= drafts && refused * 4 >= drafts,
        "{sealed} sealed, {refused} refused"
    );
}

#[derive(Default)]
struct Measurements {
    sealed: usize,
    refused: usize,
    too_long: usize,
    worst_check: Cost,
    worst_refusal: Cost,
    worst_verify: Cost,
    largest: usize,
}

struct Sample {
    n: usize,
    name: Option<&'static str>,
    bytes: usize,
}
impl Sample {
    fn validate(&self, diagnostics: &[aq_playbooks_compiler::Diagnostic]) {
        let n = self.n;
        // A draft aimed just under a bound seals.
        assert!(
            self.name.is_none() || diagnostics.is_empty(),
            "{n}: {diagnostics:#?}"
        );
        // Refused for its size alone, never because the check gave up.
        if let Some(d) = diagnostics.iter().find(|d| {
            !["expression_limit", "draft_limit"].contains(&d.code.as_str())
                || d.message.contains("too many ways to check")
                || d.message.contains("levels deep")
        }) {
            panic!("draft {n}: the generator built a draft refused for another reason: {d:#?}");
        }
    }

    fn refused(&self, cost: Cost, totals: &mut Measurements) {
        let Self { n, bytes, .. } = *self;
        totals.refused += 1;
        totals.too_long += usize::from(bytes > MAX_DRAFT_JSON_BYTES);
        totals.worst_refusal = totals.worst_refusal.max(cost);
        assert!(
            cost.bytes < CHECK_BUDGET.bytes,
            "draft {n} ({bytes} bytes): refusing it took {cost:?}"
        );
    }

    fn verify(&self, ws: &Workspace, cost: Cost, totals: &mut Measurements) {
        let Self { n, bytes, name } = *self;
        totals.sealed += 1;
        totals.largest = totals.largest.max(bytes);
        totals.worst_check = totals.worst_check.max(cost);
        assert!(
            cost.bytes < CHECK_BUDGET.bytes,
            "draft {n} ({bytes} bytes): checking it took {cost:?}"
        );
        let artifact = seal(ws).unwrap_or_else(|d| panic!("draft {n}: {d:#?}"));
        let stored = canonical_bytes(&artifact).unwrap();
        let (loaded, verify) = measured(|| Artifact::from_canonical_bytes(&stored));
        loaded.unwrap_or_else(|e| panic!("draft {n}: {e}"));
        totals.worst_verify = totals.worst_verify.max(verify);
        if let Some(name) = name {
            println!("at {name} ({bytes} bytes): check {cost:?}, verify {verify:?}");
        }
        assert!(
            verify.bytes < VERIFY_BUDGET.bytes,
            "draft {n} ({bytes} bytes): verifying it took {verify:?}"
        );
        if !cfg!(debug_assertions) {
            assert!(
                cost.time < CHECK_BUDGET.time,
                "draft {n} ({bytes} bytes): checking it took {cost:?}"
            );
            assert!(
                verify.time < VERIFY_BUDGET.time,
                "draft {n} ({bytes} bytes): verifying it took {verify:?}"
            );
        }
    }
}
