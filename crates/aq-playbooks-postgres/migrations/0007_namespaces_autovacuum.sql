-- Spec 0003 (live state only): storage tuning for the one-row namespace
-- record. Tuning only; no schema, data or behaviour change.
--
-- On protected format 2 every transition rewrites the namespace's one wide,
-- encrypted row. Ciphertext does not compress, so each commit writes a new
-- TOAST value the size of the record and leaves the old one dead. The heap
-- row prunes itself; TOAST space is reclaimed only by vacuum, which makes it
-- reusable without shrinking the file. The table's on-disk size is therefore
-- a plateau of roughly
--     commit rate x record size x (autovacuum naptime + vacuum time),
-- not a size that grows with the number of runs.
--
-- Without the cost-based delay a vacuum pass of this table and of its TOAST
-- table finishes in one go, which shortens the vacuum-time term. The naptime
-- is server-wide and is left to the operator. Only this table is changed:
-- the database may be shared with other workloads.
--
-- Setting autovacuum storage parameters needs ownership of the table, which
-- the role applying these migrations has (it created the table in 0001). It
-- takes a SHARE UPDATE EXCLUSIVE lock, so concurrent commits are not
-- blocked. Setting the same values again is a no-op.
ALTER TABLE aq_playbooks.namespaces SET (
    autovacuum_vacuum_cost_delay = 0,
    toast.autovacuum_vacuum_cost_delay = 0
);
