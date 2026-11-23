use super::*;

enum HistoryOrder {
    Finished,
    Sequence,
}
struct HistoryLog<'a>(&'a str);
impl HistoryLog<'_> {
    fn run_count(&self) -> Option<i64> {
        support::client()
            .query_opt(
                "SELECT runs FROM aq_playbooks.run_history_counts WHERE namespace=$1",
                &[&self.0],
            )
            .unwrap()
            .map(|row| row.get(0))
    }

    fn run_ids(&self, order: HistoryOrder) -> Vec<String> {
        let field = match order {
            HistoryOrder::Finished => "finished_at_ms",
            HistoryOrder::Sequence => "sequence",
        };
        let query = format!(
            "SELECT item_key FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='runs' ORDER BY {field},item_key"
        );
        support::client()
            .query(&query, &[&self.0])
            .unwrap()
            .into_iter()
            .map(|row| row.get(0))
            .collect()
    }
    fn assert_finished(&self, expected: &[String], format: &str) {
        assert_eq!(self.run_ids(HistoryOrder::Finished), expected, "{format}");
        assert_eq!(self.run_count(), Some(expected.len() as i64), "{format}");
    }
    fn receipt_count(&self, pattern: &str) -> i64 {
        support::client()
                .query_one(
                    "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='receipts' AND run_id LIKE $2",
                    &[&self.0, &pattern],
                )
                .unwrap()
                .get(0)
    }
    fn maintained_counts(&self) -> (i64, i64, i64) {
        let row = support::client()
                .query_one(
                    "SELECT runs,event_receipts,admin_receipts FROM aq_playbooks.run_history_counts WHERE namespace=$1",
                    &[&self.0],
                )
                .unwrap();
        (row.get(0), row.get(1), row.get(2))
    }
    fn row_counts(&self) -> (i64, i64, i64) {
        (
            self.run_ids(HistoryOrder::Sequence).len() as i64,
            self.receipt_count("event:%"),
            self.receipt_count("admin"),
        )
    }
}

/// On f2 protected and f3: `history_keep` 5 keeps the newest 5 of 20
/// finished runs, holds only those runs' items (and admin receipts), and
/// counts them; deleting history by time keeps the count, so the trim after
/// it keeps the newest 5 again; `history_keep` 0 writes no row.
#[test]
fn history_is_a_rolling_log_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("rolling");
        let e = Engine::new(store, &ns, "w").unwrap();
        let history = HistoryLog(&ns);
        let a = support::artifact("a");
        e.install(a.clone()).unwrap();
        e.activate_at("a", &a.digest, pg_authority(), 0).unwrap();
        e.set_retention_policy(RetentionPolicy {
            history_keep: 5,
            ..RetentionPolicy::default()
        })
        .unwrap();
        let admit = |t: i64| {
            let id = e
                .admit("a", "main", Value::Null, pg_authority(), t)
                .unwrap();
            assert!(e.advance(&id, t).unwrap(), "{format}");
            id
        };
        let runs: Vec<String> = (0..20).map(admit).collect();
        history.assert_finished(&runs[15..], format);
        let foreign: i64 = support::client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1 AND run_id<>ALL($2) AND run_id<>'admin'",
                &[&ns, &runs[15..].to_vec()],
            )
            .unwrap()
            .get(0);
        assert_eq!(foreign, 0, "{format}");
        let listed: Vec<Value> = e
            .inspect(&runs_query())
            .unwrap()
            .items
            .iter()
            .map(|r| r["id"].clone())
            .collect();
        assert_eq!(listed.len(), 5, "{format}");
        // Delete by time, then the trim counts what is left.
        assert!(e.delete_history_before(17).unwrap() > 0, "{format}");
        history.assert_finished(&runs[17..], format);
        let more: Vec<String> = (20..24).map(admit).collect();
        let mut expected = runs[19..].to_vec();
        expected.extend(more);
        history.assert_finished(&expected, format);
        // `history_keep` 0: nothing is written; what is there is trimmed.
        e.set_retention_policy(RetentionPolicy {
            history_keep: 0,
            ..RetentionPolicy::default()
        })
        .unwrap();
        admit(30);
        history.assert_finished(&[], format);
    }
}
/// One commit settling 100 cohorts, on f2 protected, f2 plain and f3: the
/// trim keeps up, history holds the newest 5.
#[test]
fn the_trim_keeps_up_with_one_commit_on_postgres() {
    for (label, store) in [
        ("f2", support::protected()),
        ("f2_plain", support::store()),
        ("f3", support::records()),
    ] {
        let ns = support::namespace(label);
        let history = HistoryLog(&ns);
        store.acquire(&ns, "w").unwrap();
        let mut state = State::default();
        state.retention.policy.history_keep = 5;
        state.settled = (0..100)
            .map(|i| synthetic_cohort(&format!("c{i:03}"), i, &[i as u64 + 1]))
            .collect();
        store.compare_and_swap(&ns, "w", 0, &state).unwrap();
        assert_eq!(
            history.run_ids(HistoryOrder::Finished),
            ["c095", "c096", "c097", "c098", "c099"],
            "{label}"
        );
        assert_eq!(history_rows(&ns), 10, "{label}");
        assert_eq!(history.run_count(), Some(5), "{label}");
    }
}
/// Review I2 and I3 on f2 protected and f3: the trim keeps the newest runs
/// by sequence after a host clock that ran ahead, and receipts owned by no
/// run (blocked event deliveries, admin) are rolling sets of `history_keep`.
#[test]
fn history_trims_by_sequence_and_bounds_receipts_of_no_run_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("rolling_all");
        let e = Engine::new(store, &ns, "w").unwrap();
        let history = HistoryLog(&ns);
        let a = support::artifact("a");
        e.install(a.clone()).unwrap();
        e.activate_at("a", &a.digest, pg_authority(), 0).unwrap();
        activate_blocked_trigger(&e);
        e.set_retention_policy(RetentionPolicy {
            replay_keep: 5,
            history_keep: 3,
            ..RetentionPolicy::default()
        })
        .unwrap();
        let run = |t: i64| {
            let id = e
                .admit("a", "main", Value::Null, pg_authority(), t)
                .unwrap();
            assert!(e.advance(&id, t).unwrap(), "{format}");
            id
        };
        let pinned: Vec<String> = (0..3).map(|_| run(9_000_000_000_000)).collect();
        let later: Vec<String> = (0..2).map(|_| run(1_000)).collect();
        assert_eq!(
            history.run_ids(HistoryOrder::Sequence),
            [pinned[2].clone(), later[0].clone(), later[1].clone()],
            "{format}"
        );
        for i in 0..40 {
            e.admit_event("host", pg_event(&format!("b{i:03}")), 2_000 + i)
                .unwrap();
        }
        for i in 0..10 {
            e.set_activation_controls("trigger", ActivationControls::default(), 3_000 + i)
                .unwrap();
        }
        for group in ["event:%", "admin"] {
            let count = history.receipt_count(group);
            assert!((1..=3).contains(&count), "{format} {group}: {count}");
        }
        // The counts are those of the rows, also after a delete by time.
        assert_eq!(
            history.maintained_counts(),
            history.row_counts(),
            "{format}"
        );
        assert!(e.delete_history_before(2_020).unwrap() > 0, "{format}");
        assert_eq!(
            history.maintained_counts(),
            history.row_counts(),
            "{format}"
        );
        assert!(e.delete_history_before(10_000).unwrap() > 0, "{format}");
        // The run finished at the pinned time (later than 10,000) stays.
        assert_eq!(history.row_counts(), (1, 0, 0), "{format}");
        assert_eq!(
            history.maintained_counts(),
            history.row_counts(),
            "{format}"
        );
    }
}

fn activate_blocked_trigger(e: &Engine<PostgresStorage>) {
    // A trigger whose input is refused at acceptance: every event writes
    // a `destination_blocked` receipt.
    let mut trigger = support::artifact("trigger").content;
    trigger
        .registry
        .events
        .insert("test.signal".into(), pg_event_contract());
    trigger.playbook.triggers.push(aq_playbooks_model::Trigger {
        id: "on-signal".into(),
        event: aq_playbooks_model::ContractRef {
            name: "test.signal".into(),
            fingerprint: pg_event_contract().fingerprint().unwrap(),
        },
        entry: "main".into(),
        input: aq_playbooks_model::Expr::literal(Value::Null),
    });
    trigger.playbook.limits.max_payload_bytes = 1;
    let trigger = aq_playbooks_model::Artifact::seal(trigger).unwrap();
    e.install(trigger.clone()).unwrap();
    e.activate_at("trigger", &trigger.digest, pg_authority(), 0)
        .unwrap();
}
