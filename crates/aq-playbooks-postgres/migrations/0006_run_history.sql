-- Spec 0003 (live state only): a settled cohort leaves the State in the commit
-- that settles it, and that same commit inserts its public inspection items
-- here, one row per run, effect or receipt. Rows hold public projections only
-- (never encrypted), are inserted once and never updated, and are deleted by
-- the namespace owner by finish time.
CREATE TABLE aq_playbooks.run_history (
    namespace text NOT NULL REFERENCES aq_playbooks.namespaces(namespace),
    kind text NOT NULL CHECK(kind IN ('runs','effects','receipts')),
    item_key text COLLATE "C" NOT NULL,
    run_id text NOT NULL,
    root_id text NOT NULL,
    sequence bigint NOT NULL CHECK(sequence>=0),
    finished_at_ms bigint NOT NULL,
    public_value jsonb NOT NULL,
    PRIMARY KEY(namespace,kind,item_key)
);
CREATE INDEX run_history_by_run ON aq_playbooks.run_history(namespace,kind,run_id,item_key);
CREATE INDEX run_history_by_finish ON aq_playbooks.run_history(namespace,finished_at_ms);

CREATE FUNCTION aq_playbooks.reject_run_history_update() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'run history rows are immutable';
END;
$$;
CREATE FUNCTION aq_playbooks.check_run_history_writer() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE n text;
BEGIN
    IF TG_OP='DELETE' THEN n:=OLD.namespace; ELSE n:=NEW.namespace; END IF;
    IF current_setting('aq_playbooks.writer_namespace',true) IS DISTINCT FROM n THEN
        RAISE EXCEPTION 'qualified run history namespace writer required';
    END IF;
    IF TG_OP='DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER run_history_immutable BEFORE UPDATE ON aq_playbooks.run_history
    FOR EACH ROW EXECUTE FUNCTION aq_playbooks.reject_run_history_update();
CREATE TRIGGER run_history_writer BEFORE INSERT OR DELETE ON aq_playbooks.run_history
    FOR EACH ROW EXECUTE FUNCTION aq_playbooks.check_run_history_writer();
CREATE TRIGGER run_history_no_truncate BEFORE TRUNCATE ON aq_playbooks.run_history
    FOR EACH STATEMENT EXECUTE FUNCTION aq_playbooks.reject_artifact_mutation();
