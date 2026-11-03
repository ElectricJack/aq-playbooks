# D10 review revision 2 publication evidence

Task: `keen-falcon-49`, revising `rev-wise-rapids` revision 1.
Source baseline: `7acb983` (`origin/main` at revision preparation).

## Published review

- Review: `rev-wise-rapids`, revision **2**, kind `plan`.
- Content SHA-256: `1f6e90dac38056570367f358385d5a05e9b079a1ccd06de6e00dcdb90d1e89f9`.
- Canonical vault mirror: `projects/aq-playbooks/plans/2026-11-02-d10-bounded-transition-representation-quota-proof-and-tdd-pl.md`.
- Submitted through `aq review submit --review-id rev-wise-rapids`, with a disposition
  and `--resolves` for each of the seven comment IDs below.
- Canonical readback: `aq review show --review-id rev-wise-rapids --revision 2 --comments`.
  Published content/hash matched the UTF-8 local draft exactly. All seven comments
  report `resolved_in_revision=2`; proposed responses remain unaccepted by the decider.
- State at readback: `in_review`. This publication neither approves the representation
  nor qualifies a Rust implementation. The complete plan is stored in AQ, not duplicated here.

## Feedback completion evidence

| Acceptance item | Revision 2 changes and evidence |
| --- | --- |
| `cmt-ccba53c37f51` | **Canonical authenticated tree** defines U64 ready/event/receipt comparators and I64 timer order separately from canonical JSON string order. The object-length proof is permutation invariant; full materialization re-sorts canonically. Tasks 2/3 add 9/10, signed/mixed-width, extrema, range and permutation cases. Checked model `src/lib.rs:682-706` and runner `src/indexes.rs:82-112,163,221,250,316`. |
| `cmt-49c544589212` | **Parallel writers with one owner / Exact format-3 worker fence and its authority change** specifies both 0005 trigger branches, lock/context/base-revision predicates, rooted full-CAS changes and owner-only release/admin operations. Review Focus and Task 4 explicitly expose the trusted-role authority relaxation and negative SQL tests. Checked `0004_catalog_inspection.sql:24-41,67-85` and postgres `src/lib.rs:383,419,497-513`. |
| `cmt-b4ff6028de27` | **Protection and migration** specifies namespace/path/kind/format/key-slot/digest row identity, stable container paths, random-nonce plaintext conflict verification and atomic fenced deletion of unreachable old-key rows. Tasks 3/5 cover two paths, same-key reinsertion and recreation after rotation/key retirement. Checked prototype `examples/rooted/tree.rs:554-570`, `src/protection.rs:84-137` and `0002_indexes_and_protection.sql`. |
| `cmt-039b8bd087c9` | Global Constraints and Task 6 use `origin/main`. `git branch -r` confirms `origin/HEAD -> origin/main` and no `origin/dev`. |
| `cmt-c40596b837d2` | Root AAD excludes owner/incarnation. Task 5 `replacement_owner_preserves_root_envelope` verifies ownership acquisition without resealing, including a different active key with the required decryption key available. Checked postgres `src/protection.rs:63-72` and `src/lib.rs:acquire`. |
| `cmt-b59b8ee490fb` | Pool contract requires dedicated OS threads, no entered Tokio runtime before client calls, runtime-context constructor refusal with recoverable client ownership, and direct Tokio/lockfile work. Task 4 names worker/constructor context tests. Checked locked postgres 0.19 client `connection.block_on` calls and current crate dependencies. |
| `cmt-2f6b740acccc` | Task 6 identifies the operating_envelope example's `Measured<S>` wrapper. It requires forwarding both support and transition methods, explicit capability support plus a real untimed incremental dispatch before each timed cell, and refusal on omitted forwarding. Checked `examples/operating_envelope.rs:38-81`. |
| `overall_note` | Published a newer revision with all seven dispositions. All six implementation tasks, 26 TDD steps and original Global Constraints remain, with the integration-base correction. Representation/authority/integrity acceptance stays with the decider; implementation and measurement remain pending. |

## Verification and limits

[validation.json](evidence/D10-review-r2/validation.json) records document checks:
all seven IDs covered, six task headings and all 26 checkbox steps preserved, and
Global Constraints identical except the requested branch correction. A separate
worked arithmetic example checked the stated object-length formula across all
120 permutations of five entries, including numeric key widths and escaped/UTF-8
values. This is an arithmetic sanity check, not a replacement for Rust quota tests.

No production Rust/schema/API changes were made. New Rust tests, PostgreSQL
qualification and the PB-02 performance matrix were not run because this task only
revises the plan. Tasks 2–6 retain those implementation acceptance obligations.
