#!/usr/bin/env python3
"""Actual frozen0.1.2 warm CAS/cold constructor refusal at catalog schema4.

Requires an empty aq_pb_catalog_* database on the existing disposable loopback
55432 PostgreSQL. No canonical configuration, remote source or publication.
"""

import json
import os

from frozen_writer import FrozenWriter, checked_archive, run_example


WRITER = FrozenWriter(
    "pb-catalog-old-writer-probe",
    "0.1.2",
    r"""
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::{Engine,Storage};
use std::io::{self,Write};
fn main(){
 let url=std::env::var("AQ_PLAYBOOKS_TEST_DATABASE_URL").unwrap();
 let e=Engine::new(PostgresStorage::connect(&url).unwrap(),"catalog-legacy-probe","old-owner").unwrap();
 let mut state=e.store.load(&e.namespace).unwrap();state.state.sequence=99;
 e.store.compare_and_swap(&e.namespace,&e.owner,state.revision,&state.state).unwrap();
 println!("OLD_OWNER_READY");io::stdout().flush().unwrap();
 let mut line=String::new();io::stdin().read_line(&mut line).unwrap();
 let mut state=e.store.load(&e.namespace).unwrap();state.state.sequence=100;
 assert!(e.store.compare_and_swap(&e.namespace,&e.owner,state.revision,&state.state).is_err());
 println!("WARM_OLD_CAS_REFUSED");
 assert!(PostgresStorage::connect(&url).is_err());println!("COLD_OLD_CONSTRUCTOR_REFUSED");
 // Drop the old physical connection, rather than requiring a legacy SQL
 // release operation to write a newer qualified namespace format.
}
""",
)


def main():
    archive, archive_hash = checked_archive(
        "bd1246e8e972ee1627e94598945f398812c89044106b463151a696511412ca39",
        "/aq_pb_catalog_",
    )
    assert os.environ.get("CARGO_TARGET_DIR"), (
        "root-assigned shared build cache required"
    )
    with WRITER.start(archive, "pb-catalog-old-") as owner:
        owner.wait_ready("OLD_OWNER_READY", "old owner readiness timeout")
        migrated = run_example(
            "catalog_compatibility_probe", "migrate", "catalog-legacy-probe"
        )
        assert migrated["sequence"] == 99
        owner.expect_refusals(["WARM_OLD_CAS_REFUSED", "COLD_OLD_CONSTRUCTOR_REFUSED"])
        page = run_example(
            "catalog_compatibility_probe", "inspect", "catalog-legacy-probe"
        )
        assert page["api_version"] == "playbooks-catalog-1" and page["items"] == []
        print(
            json.dumps(
                {
                    "passed": True,
                    "old_archive_sha256": archive_hash,
                    "old_version": "0.1.2",
                    "new_version": "0.1.3",
                    "warm_old_cas": "refused_by_database",
                    "cold_old_constructor": "refused",
                    "legacy_sequence": 99,
                    "new_owner_catalog": page,
                },
                sort_keys=True,
            )
        )


if __name__ == "__main__":
    main()
