-- Qualified catalog writers publish State and its bounded metadata together.
-- Earlier already-open writers must not leave this projection stale.
ALTER TABLE aq_playbooks.namespaces DROP CONSTRAINT namespaces_writer_format_check;
ALTER TABLE aq_playbooks.namespaces
    ADD COLUMN catalog_projected_revision bigint,
    ADD COLUMN catalog_metadata_revision bigint,
    ADD COLUMN catalog_supported boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT catalog_projection_complete CHECK (
        (catalog_projected_revision IS NULL AND catalog_metadata_revision IS NULL AND NOT catalog_supported)
        OR (catalog_projected_revision IS NOT NULL AND catalog_metadata_revision IS NOT NULL
            AND catalog_projected_revision=revision AND catalog_projected_revision>0
            AND catalog_metadata_revision>0 AND catalog_metadata_revision<=catalog_projected_revision)
    );

CREATE OR REPLACE FUNCTION aq_playbooks.check_namespace_writer() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'catalog namespace retirement is not implemented'; END IF;
    IF NEW.writer_format<>2
       OR current_setting('aq_playbooks.writer_namespace',true) IS DISTINCT FROM NEW.namespace
       OR current_setting('aq_playbooks.writer_format',true) IS DISTINCT FROM '2' THEN
        RAISE EXCEPTION 'qualified current catalog namespace writer required';
    END IF;
    RETURN NEW;
END;
$$;
DO $$
DECLARE n record;
BEGIN
    FOR n IN SELECT namespace FROM aq_playbooks.namespaces LOOP
        PERFORM set_config('aq_playbooks.writer_namespace',n.namespace,true);
        PERFORM set_config('aq_playbooks.writer_format','2',true);
        UPDATE aq_playbooks.namespaces SET writer_format=2 WHERE namespace=n.namespace;
    END LOOP;
END;
$$;
ALTER TABLE aq_playbooks.namespaces ALTER COLUMN writer_format SET DEFAULT 2;
ALTER TABLE aq_playbooks.namespaces ADD CONSTRAINT namespaces_writer_format_check CHECK(writer_format=2);

CREATE TABLE aq_playbooks.catalog_rows (
    namespace text NOT NULL REFERENCES aq_playbooks.namespaces(namespace),
    kind text NOT NULL CHECK(kind IN ('definitions','activations')),
    item_key text COLLATE "C" NOT NULL CHECK(item_key ~ '^sha256:[0-9a-f]{64}$'),
    row_revision bigint NOT NULL CHECK(row_revision>0),
    plain_bytes bytea CHECK(octet_length(plain_bytes) BETWEEN 1 AND 131072),
    protection_key_id text,
    protection_nonce bytea,
    encrypted_bytes bytea CHECK(octet_length(encrypted_bytes) BETWEEN 17 AND 131088),
    PRIMARY KEY(namespace,kind,item_key),
    CHECK((plain_bytes IS NOT NULL AND protection_key_id IS NULL AND protection_nonce IS NULL AND encrypted_bytes IS NULL)
       OR (plain_bytes IS NULL AND protection_key_id IS NOT NULL AND protection_nonce IS NOT NULL AND octet_length(protection_nonce)=24 AND encrypted_bytes IS NOT NULL))
);
CREATE FUNCTION aq_playbooks.check_catalog_writer() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE n text;
BEGIN
    IF TG_OP='DELETE' THEN n:=OLD.namespace; ELSE n:=NEW.namespace; END IF;
    IF current_setting('aq_playbooks.writer_namespace',true) IS DISTINCT FROM n
       OR current_setting('aq_playbooks.writer_format',true) IS DISTINCT FROM '2'
       OR NOT EXISTS(SELECT 1 FROM aq_playbooks.namespaces WHERE namespace=n AND writer_format=2 AND owner_backend_pid=pg_backend_pid() AND owner IS NOT NULL) THEN
        RAISE EXCEPTION 'qualified current catalog owner writer required';
    END IF;
    IF TG_OP='DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER catalog_owner_writer BEFORE INSERT OR UPDATE OR DELETE ON aq_playbooks.catalog_rows
    FOR EACH ROW EXECUTE FUNCTION aq_playbooks.check_catalog_writer();
