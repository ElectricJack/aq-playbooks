-- Spec 0003, owner direction "rolling bounds": history is a rolling log of the
-- most recent `policy.history_keep` finished runs per namespace. Each commit
-- that writes history deletes the oldest runs beyond that number (all of a
-- run's items, its run row last), a bounded number per commit.
--
-- Finding the oldest runs reads this partial index in order (no sort, no
-- scan of the table): it holds one entry per finished run.
CREATE INDEX run_history_runs_by_finish ON aq_playbooks.run_history(namespace,finished_at_ms,item_key) WHERE kind='runs';
-- The number of finished runs each namespace's history holds, maintained by
-- the same transactions that insert, trim and delete history rows, so the
-- trim never counts rows.
CREATE TABLE aq_playbooks.run_history_counts (
    namespace text PRIMARY KEY REFERENCES aq_playbooks.namespaces(namespace),
    runs bigint NOT NULL CHECK(runs>=0)
);
-- Namespaces with history written before this migration (one count, once).
INSERT INTO aq_playbooks.run_history_counts(namespace,runs)
    SELECT namespace,count(*) FROM aq_playbooks.run_history WHERE kind='runs' GROUP BY namespace;
-- Written only by the namespace's leased writer, as run_history itself.
CREATE TRIGGER run_history_counts_writer BEFORE INSERT OR UPDATE OR DELETE ON aq_playbooks.run_history_counts
    FOR EACH ROW EXECUTE FUNCTION aq_playbooks.check_run_history_writer();
CREATE TRIGGER run_history_counts_no_truncate BEFORE TRUNCATE ON aq_playbooks.run_history_counts
    FOR EACH STATEMENT EXECUTE FUNCTION aq_playbooks.reject_run_history_truncate();
