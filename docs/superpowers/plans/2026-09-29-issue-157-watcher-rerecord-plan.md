# Issue #157: Watcher re-record fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> If your harness has no such skill, execute the tasks in order, one at a time, running the
> listed commands and committing at the end of each task.

- Date: 2026-09-29
- Branch: `fix/issue-157-watcher-re_record` (cut from `origin/feat/p2p-sync-iroh` @ `3881d70`)
- Issue: sapphire-framework #157 「sync: liveセッションの受信書き込みがwatcher再スキャンでローカル編集として再記録され、競合コピーと削除伝播失敗を起こす」
- Spec: `docs/superpowers/specs/2026-09-15-p2p-sync-iroh-design.md` §2.4/§2.5 (the local-write and
  external-edit rules this fix makes precise).

**Goal:** A file whose on-disk bytes match a version the store already knows must never be
re-recorded as a fresh local edit when a watcher-driven scan (or `fetch_missing`) revisits it
while `disk.hash` is stale. It is a materialization of known content: settle it, do not
invent a new dot. This removes both symptoms of issue #157 — the spurious conflict copy after
a single sequential edit, and the failed delete propagation — at the same root cause.

**Ruling (agreed with the maintainer, 2026-09-29).** The fix is in
`crates/sapphire-framework-sync/src/replica.rs::reconcile_path`, at the
`file_hash != disk.hash` fall-through:

1. Before constructing a new entry, check whether `file_hash` equals the content hash of **any
   version already in the stored state** (including the winner and any loser).
2. If it does, the file is a re-appearance of known content — a materialization the store has
   already committed (e.g. one a live session wrote, which fired the watcher). Do **not** call
   `next_entry` (no new dot, no new `disk.seen` pollution): refresh the stamp fields as the
   `file_hash == disk.hash` branch does, then run `ensure_conflict_copies` and `settle` on the
   stored state and return — the same shape as the existing `file_hash == disk.hash` branch.
3. Everything else stays: genuine local edits (content no version knows) still get a new dot
   with `context = disk.seen`; the tombstone/pause guard and all skip guards are unchanged.
   The issue's second symptom (delete propagation) is collateral of the first: once no spurious
   `(a, n)` dot is invented, the tombstone's context stays right and deletion propagates — no
   separate change to tombstone handling.

**Deliverables in full:**

- The `reconcile_path` fix in `crates/sapphire-framework-sync/src/replica.rs` (with its doc
  comment updated: `reconcile_path` now "records an edit of content the store does not
  already know, or settles the state when the file holds a known version").
- Regression tests in `crates/sapphire-framework-sync/tests/replica_conflict.rs` reproducing
  the live ordering at the replica layer (Task 1).
- The live-stack end-to-end sequence (write → remote edit → local delete over two hosts with a
  live session) as a server test proving the end-to-end symptom is gone (Task 2).

**Tech stack:** Rust, `cargo test`, redb store, tokio (server tests).

## Global Constraints

- Commits: conventional commits, imperative subject, one commit per task, matching `git log`
  style (`fix(sync): … (issue #157)`).
- `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace --all-features` must pass at the end of every task.
- Doc comments are load-bearing: comments describing the old two-branch behavior must describe
  the new three-way behavior.

## Task 1: Core fix + replica-layer regression tests

- [ ] In `crates/sapphire-framework-sync/src/replica.rs`, in `reconcile_path`, after the
      `file_hash == disk.hash` branch and before `let content = match file_hash`, add the
      known-content branch: when `file_hash` is `Some(h)` and `state` has a version whose
      `content.hash()` is `Some(h)`, treat it exactly like the `file_hash == disk.hash` case —
      refresh stamp fields when they differ or are racy, `ensure_conflict_copies`, `settle`,
      return. Update the method doc comment accordingly.
- [ ] Add regression tests to `crates/sapphire-framework-sync/tests/replica_conflict.rs` that
      reproduce the live ordering at the replica layer:
      - `a_received_write_scanned_after_the_store_commit_is_not_a_local_edit`: a writes
        `a.txt = v1` and scans; sync; b writes `a.txt = v2`, scans; **a applies b's delta with
        an empty source** (`MapSource::default()`, so the update is joined but not
        materialized), `commit_session`s, **then** the file is written on a's disk (a's live
        session's write), **then** a scans. Assert: a's disk holds v2, `state("a.txt").versions`
        has length 1, no `conflicts` in any report, no `.conflict-` path appears in either
      tree.
      - `a_delete_after_a_received_edit_propagates`: same ordering as above, then a removes the
        file, scans, syncs both ways once each; assert the file is gone from **both** trees and
        both trees are equal.
      Both tests must fail before the fix (versions.len()==2 / leftover file) and pass after.
- [ ] `cargo test -p sapphire-framework-sync` passes; full workspace tests and clippy pass.

## Task 2: Live-stack end-to-end test (server)

- [ ] Add a test to `crates/sapphire-framework-server/tests/converge.rs` (or `live.rs` if it
      fits the live-session framing better — reviewer decides by file conventions already
      there): two synced hosts; a writes `note.md` via the IPC write path; b edits it (direct
      `std::fs::write` + wait for propagation back to a, as existing tests wait); a deletes it
      (`std::fs::remove_file` + wait); assert the file is **gone from b** within the existing
      `await_file`-style polling discipline (poll for absence with a deadline; a helper may be
      added to `tests/common/mod.rs` if none fits). This test must fail (timeout/timeout assert)
      on the pre-fix tree and pass after Task 1.
- [ ] The existing convergence/live suites (`tests/converge.rs`, `tests/live.rs`,
      `crates/sapphire-framework-session/tests/*`) all pass unchanged.

## Verification (whole branch)

- `cargo test --workspace --all-features` — all green (57+ suites as on the base branch).
- The sapphire-sync `feat/sync-app` harness (Task 4, `cli/tests/propagate.rs` / `probe.rs`)
  re-runs green against this branch afterwards — outside this repo, done by the maintainer.
