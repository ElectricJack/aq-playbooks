# Current catalog inspection source candidate 0.1.3

Source commits `9df1113`, `4740651` and `0173673` add the component-owned current metadata
query; runner/PostgreSQL/CLI are 0.1.3 and model/compiler remain 0.1.0.
`source-manifest.json` pins the exact 67-file standalone source closure and
SHA-256 of `source.tar`. This follows the existing exact Git source-archive
profile. It is not a registry crate or a consumer adoption record.

Final qualification ran entirely from the extracted archive, with all 67 source
file hashes verified before and after. The affected runner/PostgreSQL/CLI suite
passed **153 tests** (9.88 s compilation; 10.92 s summed test-suite time). Strict
lint for all affected targets with PostgreSQL tests enabled passed in 6.54 s;
formatting passed. Verbatim extracted logs are retained alongside the source.

The ten actual catalog PostgreSQL cases passed, including bounded queries with
snapshot access denied, ownership loss/takeover, stale continuations, rollback,
committed-reply loss refusal, protected row tampering/rotation, row-identity
fences and missing-row/count guards. Missing row DELETE refuses both reads and
an unchanged CAS, without repair; TRUNCATE and direct counter/header reset are
refused. Unrelated progress preserves both metadata stamps and encrypted row
bytes. Captured private history is absent from responses.

The extracted compatibility probe independently compiled the frozen 0.1.2
archive. Its already-open writer was rejected by schema 4, its fresh constructor
refused the schema, and committed sequence 99 survived successor incarnation 2.
The old archive remains byte-identical (`bd1246…ca39`).

Independent source review identified the missing-row completeness blocker in
`9df1113`/`4740651`. The `0173673` count-based correction received source review
with no additional concrete blocker and then passed the final actual tests.
Earlier development checks in `qualification.json` are historical preliminary
results; final extracted logs qualify the final source. Count/guard claims assume
the qualified triggers are enabled, and exclude a hostile schema owner disabling
guards or replaying an older valid encrypted row. This is not a cryptographic
integrity claim against a malicious DBA.

The public surface is separate `CatalogQuery`/`CatalogPage` plus
`CatalogError`, `Storage::inspect_catalog` (default Unsupported), and
`Engine::inspect_catalog`. Existing request, response and error enum shapes do
not change. The CLI requires explicit `--acquire-idle-namespace`, refuses a
nonexistent/live-owned namespace, and releases its inspection ownership before
printing. Acquisition can perform one initial catalog materialization through
normal State CAS; the bounded read itself never materializes or loads State.
An embedding daemon should delegate through its existing owning Engine.

Queries expose current retained namespace artifacts or represented activation
bindings. They do not expose captured values, schemas, full artifacts, authority
or run history, and do not infer grant validity, activation times or readiness.
Pagination uses exact namespace/kind/incarnation/metadata-revision stamps and
opaque SHA-256 addresses, with at most 25 entries and 256 KiB per response.
Protected rows are encrypted individually and bound to their namespace, kind,
address and row revision. No full-snapshot fallback is permitted.

All database checks use fresh `aq_pb_catalog_*` databases on the existing
loopback-only disposable PostgreSQL. No provider call, worker execution, remote
publication, canonical database/configuration change, AQ consumer pin or browser
promotion occurred. Snapshot write costs and broad fleet performance remain
outside this bounded read qualification.

Reproduce with an explicitly assigned shared Cargo cache, jobs 2, debug 0 and
incremental disabled:

```sh
cargo test --offline --locked -p aq-playbooks-runner -p aq-playbooks-postgres -p aq-playbooks-cli --features postgres-tests
cargo clippy --offline --locked -p aq-playbooks-runner -p aq-playbooks-postgres -p aq-playbooks-cli --all-targets --features postgres-tests -- -D warnings
cargo fmt --all -- --check
python3 scripts/test-catalog-old-writer.py --old-source-archive /path/to/frozen-0.1.2/source.tar
```

The last command requires a separate empty `aq_pb_catalog_*` database and checks
the frozen old archive digest before compiling it. Run it sequentially with other
Cargo work sharing the assigned cache.
