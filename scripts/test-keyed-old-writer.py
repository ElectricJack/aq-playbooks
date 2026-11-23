#!/usr/bin/env python3
"""Probe old0.1.0 from its exact Git archive against a disposable new writer.
Requires AQ_PLAYBOOKS_TEST_DATABASE_URL to name an empty aq_pb_keyed_* database
on scripts/test-db loopback55432. No live sibling checkout or network discovery.
"""

import hashlib
import json

from frozen_writer import FrozenWriter, checked_archive, run_example


WRITER = FrozenWriter(
    "pb-old-writer-probe",
    "0.1.0",
    r"""
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::{Engine,Storage};
use std::io::{self,Write};
fn main(){
 let url=std::env::var("AQ_PLAYBOOKS_TEST_DATABASE_URL").unwrap();
 let engine=Engine::new(PostgresStorage::connect(&url).unwrap(),"legacy-probe","old-owner").unwrap();
 engine.release().unwrap();println!("OLD_CONNECTION_READY");io::stdout().flush().unwrap();
 let mut line=String::new();io::stdin().read_line(&mut line).unwrap();
 assert!(engine.store.acquire("legacy-probe","old-owner").is_err());println!("WARM_OLD_REFUSED");
 assert!(PostgresStorage::connect(&url).is_err());println!("COLD_OLD_REFUSED");
}
""",
)


def main():
    archive, archive_hash = checked_archive(
        "ad516d7cc5f5bded424aa364f7185db2049a1ac21b15b767c279e906ce829c9e",
        "/aq_pb_keyed_",
    )
    with WRITER.start(archive, "pb-keyed-old-") as owner:
        owner.wait_ready("OLD_CONNECTION_READY", "old helper readiness timeout")
        receipt = run_example("keyed_receipt_probe", "legacy-probe")
        assert receipt["key"] == "retained-key"
        owner.expect_refusals(["WARM_OLD_REFUSED", "COLD_OLD_REFUSED"])
        print(
            json.dumps(
                {
                    "result": "passed",
                    "old_source_revision": "42f944996eafad5af07d9ec1c72d3e3f2630c03b",
                    "old_archive_sha256": archive_hash,
                    "old_lock_sha256": hashlib.sha256(
                        owner.lockfile.read_bytes()
                    ).hexdigest(),
                    "warm_old_connection": "refused",
                    "cold_old_constructor": "refused",
                    "new_receipt": receipt,
                },
                sort_keys=True,
            )
        )


if __name__ == "__main__":
    main()
