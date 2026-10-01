mod common;

use std::collections::BTreeSet;

use common::*;
use sapphire_framework_sync::testing::MapSource;
use sapphire_framework_sync::{Conflict, SkipReason, conflict_path};

fn based(a: &mut Node, b: &mut Node) {
    write(a, "a.txt", "base");
    scan(a);
    sync(a, b);
}

fn contents(node: &Node) -> BTreeSet<String> {
    tree(node).into_values().collect()
}

#[test]
fn concurrent_edits_keep_both_versions() {
    let (mut a, mut b) = (node(1), node(2));
    based(&mut a, &mut b);
    write(&a, "a.txt", "from a");
    write(&b, "a.txt", "from b");
    scan(&mut a);
    scan(&mut b);
    let mut conflicts = Vec::new();
    for _ in 0..2 {
        conflicts.extend(push(&a, &mut b).conflicts);
        conflicts.extend(push(&b, &mut a).conflicts);
    }

    assert_eq!(tree(&a), tree(&b));
    assert_eq!(tree(&a).len(), 2);
    assert_eq!(
        contents(&a),
        BTreeSet::from(["from a".to_string(), "from b".to_string()])
    );
    let copy = tree(&a).into_keys().find(|p| p != "a.txt").unwrap();
    assert!(
        copy.starts_with("a.conflict-") && copy.ends_with(".txt"),
        "{copy}"
    );
    assert!(
        conflicts.contains(&Conflict {
            path: "a.txt".to_string(),
            copy_path: copy.clone(),
        }),
        "the copy written to the tree was reported: {conflicts:?}"
    );

    // An edit on top of the merged state supersedes both siblings; no new copies.
    write(&a, "a.txt", "resolved");
    scan(&mut a);
    sync(&mut a, &mut b);
    assert_eq!(read(&b, "a.txt").as_deref(), Some("resolved"));
    assert_eq!(b.replica.state("a.txt").unwrap().unwrap().versions.len(), 1);
    assert_eq!(tree(&b).len(), 2);
}

#[test]
fn an_edit_beats_a_concurrent_delete_without_a_copy() {
    let (mut a, mut b) = (node(1), node(2));
    based(&mut a, &mut b);
    remove(&a, "a.txt");
    write(&b, "a.txt", "edited");
    scan(&mut a);
    scan(&mut b);
    sync(&mut a, &mut b);
    assert_eq!(tree(&a), tree(&b));
    assert_eq!(read(&a, "a.txt").as_deref(), Some("edited"));
    assert_eq!(tree(&a).len(), 1);
}

#[test]
fn identical_concurrent_edits_make_no_copy() {
    let (mut a, mut b) = (node(1), node(2));
    based(&mut a, &mut b);
    write(&a, "a.txt", "same");
    write(&b, "a.txt", "same");
    scan(&mut a);
    scan(&mut b);
    sync(&mut a, &mut b);
    assert_eq!(tree(&a), tree(&b));
    assert_eq!(tree(&a).len(), 1);
}

#[test]
fn three_way_concurrency_converges_with_two_copies() {
    let (mut a, mut b, mut c) = (node(1), node(2), node(3));
    based(&mut a, &mut b);
    sync(&mut b, &mut c);
    write(&a, "a.txt", "A");
    write(&b, "a.txt", "B");
    write(&c, "a.txt", "C");
    scan(&mut a);
    scan(&mut b);
    scan(&mut c);
    for _ in 0..3 {
        sync(&mut a, &mut b);
        sync(&mut b, &mut c);
        sync(&mut a, &mut c);
    }
    assert_eq!(tree(&a), tree(&b));
    assert_eq!(tree(&b), tree(&c));
    assert_eq!(
        contents(&a),
        BTreeSet::from(["A".to_string(), "B".to_string(), "C".to_string()])
    );
    assert_eq!(tree(&a).len(), 3);
}

#[test]
fn a_deleted_copy_is_not_resurrected() {
    let (mut a, mut b) = (node(1), node(2));
    based(&mut a, &mut b);
    write(&a, "a.txt", "from a");
    write(&b, "a.txt", "from b");
    scan(&mut a);
    scan(&mut b);
    sync(&mut a, &mut b);
    sync(&mut a, &mut b);
    let copy = tree(&a).into_keys().find(|p| p != "a.txt").unwrap();
    remove(&a, &copy);
    scan(&mut a);
    sync(&mut a, &mut b);
    scan(&mut b);
    sync(&mut a, &mut b);
    assert_eq!(read(&a, &copy), None);
    assert_eq!(read(&b, &copy), None);
}

#[test]
fn a_loser_without_content_is_not_superseded_by_a_later_local_edit() {
    let (mut a, mut b) = (node(1), node(2));
    based(&mut a, &mut b);
    write(&a, "a.txt", "from a");
    scan(&mut a);
    b.clock.set(9_000_000);
    write(&b, "a.txt", "from b");
    scan(&mut b);

    // b merges a's version but cannot get its bytes, so it cannot make the copy.
    let a_vv = a.replica.vv().clone();
    let updates = a.replica.delta_for(b.replica.vv()).unwrap();
    b.replica.apply(&updates, &MapSource::default()).unwrap();
    b.replica.commit_session(&a_vv).unwrap();
    assert_eq!(tree(&b).len(), 1);

    // A later edit on b must not silently supersede a's version...
    write(&b, "a.txt", "b again");
    scan(&mut b);
    // ...so a, which has the bytes, makes the copy when it learns b won.
    sync(&mut b, &mut a);
    sync(&mut b, &mut a);
    assert_eq!(read(&a, "a.txt").as_deref(), Some("b again"));
    assert!(contents(&a).contains("from a"));
    assert!(contents(&b).contains("from a"));
    assert_eq!(tree(&a), tree(&b));
}

#[test]
fn an_ignored_copy_path_does_not_pin_the_loser_forever() {
    let (mut a, mut b) = (node(1), node(2));
    write(&a, ".sapphireignore", "*.conflict-*\n");
    based(&mut a, &mut b);
    write(&a, "a.txt", "from a");
    scan(&mut a);
    b.clock.set(9_000_000);
    write(&b, "a.txt", "from b");
    scan(&mut b);
    sync(&mut a, &mut b);
    sync(&mut a, &mut b);
    assert!(
        tree(&a).keys().all(|p| !p.contains(".conflict-")),
        "no copy written into an ignored path"
    );
    assert!(tree(&b).keys().all(|p| !p.contains(".conflict-")));

    // A later edit on b supersedes the loser: the user opted out of copies for these names.
    write(&b, "a.txt", "b again");
    scan(&mut b);
    sync(&mut a, &mut b);
    assert_eq!(a.replica.state("a.txt").unwrap().unwrap().versions.len(), 1);
    assert_eq!(b.replica.state("a.txt").unwrap().unwrap().versions.len(), 1);
    assert!(
        scan(&mut b)
            .skipped
            .iter()
            .all(|s| !s.path.contains(".conflict-")),
        "no per-scan noise"
    );
}

#[test]
fn concurrent_ignore_file_edits_keep_both_versions() {
    let (mut a, mut b) = (node(1), node(2));
    write(&a, ".sapphireignore", "*.tmp\n");
    scan(&mut a);
    sync(&mut a, &mut b);

    write(&a, ".sapphireignore", "*.tmp\n*.log\n");
    write(&b, ".sapphireignore", "*.tmp\n*.bak\n");
    scan(&mut a);
    scan(&mut b);
    sync(&mut a, &mut b);
    sync(&mut a, &mut b);

    assert_eq!(tree(&a), tree(&b));
    assert_eq!(
        contents(&a),
        BTreeSet::from(["*.tmp\n*.log\n".to_string(), "*.tmp\n*.bak\n".to_string()]),
        "the loser's ignore file survived as a copy"
    );
    let copy = tree(&a)
        .into_keys()
        .find(|p| p != ".sapphireignore")
        .unwrap();
    assert!(copy.starts_with(".sapphireignore.conflict-"), "{copy}");
}

#[test]
fn an_unrelated_file_at_the_copy_path_does_not_become_the_copy() {
    let (mut a, mut b) = (node(1), node(2));
    based(&mut a, &mut b);
    write(&a, "a.txt", "from a");
    scan(&mut a);
    b.clock.set(9_000_000);
    write(&b, "a.txt", "from b");
    scan(&mut b);

    // b merges a's version without its bytes, so no copy is written yet.
    let a_vv = a.replica.vv().clone();
    let updates = a.replica.delta_for(b.replica.vv()).unwrap();
    b.replica.apply(&updates, &MapSource::default()).unwrap();
    b.replica.commit_session(&a_vv).unwrap();

    let state = b.replica.state("a.txt").unwrap().unwrap();
    let winner = state.winner().dot;
    let loser = state.versions.iter().find(|v| v.dot != winner).unwrap();
    let copy = conflict_path("a.txt", &loser.dot);

    // Something unrelated is sitting at the copy path when the bytes arrive.
    write(&b, &copy, "not the loser at all");
    let report = b.replica.fetch_missing(&a.replica).unwrap();
    assert_eq!(
        read(&b, &copy).as_deref(),
        Some("not the loser at all"),
        "the unrelated file must not be overwritten"
    );
    assert!(
        report
            .skipped
            .iter()
            .any(|s| s.path == copy && s.reason == SkipReason::Occupied),
        "{report:?}"
    );
}

/// The live ordering: a peer's edit is committed to the store, and *then* the file the store
/// committed is written to disk (which is what the live session does, and what the watcher
/// fires on). The scan that follows must not re-record known content as a fresh local edit.
#[test]
fn a_received_write_scanned_after_the_store_commit_is_not_a_local_edit() {
    let (mut a, mut b) = (node(1), node(2));
    based(&mut a, &mut b);
    b.clock.set(9_000_000);
    write(&b, "a.txt", "v2");
    scan(&mut b);

    // a joins b's version, but the bytes are not available, so the store commits the new
    // version while `disk.hash` still names a's old file.
    let b_vv = b.replica.vv().clone();
    let updates = b.replica.delta_for(a.replica.vv()).unwrap();
    a.replica.apply(&updates, &MapSource::default()).unwrap();
    a.replica.commit_session(&b_vv).unwrap();

    // The live session's write: the file now holds the version the store already knows.
    write(&a, "a.txt", "v2");
    let report = scan(&mut a);

    assert_eq!(read(&a, "a.txt").as_deref(), Some("v2"));
    let state = a.replica.state("a.txt").unwrap().unwrap();
    assert_eq!(
        state.versions.len(),
        1,
        "known content must not be recorded as a second, concurrent version"
    );
    assert!(report.conflicts.is_empty(), "{report:?}");
    assert!(
        tree(&a).keys().all(|p| !p.contains(".conflict-")),
        "no conflict copy for known content: {:?}",
        tree(&a).keys().collect::<Vec<_>>()
    );

    // And no conflict copy reaches b either.
    sync(&mut a, &mut b);
    assert_eq!(tree(&b).len(), 1);
}

/// The second symptom of the same root cause: the invented dot pollutes the delete's
/// context, so the delete fails to propagate.
#[test]
fn a_delete_after_a_received_edit_propagates() {
    let (mut a, mut b) = (node(1), node(2));
    based(&mut a, &mut b);
    b.clock.set(9_000_000);
    write(&b, "a.txt", "v2");
    scan(&mut b);

    let b_vv = b.replica.vv().clone();
    let updates = b.replica.delta_for(a.replica.vv()).unwrap();
    a.replica.apply(&updates, &MapSource::default()).unwrap();
    a.replica.commit_session(&b_vv).unwrap();

    write(&a, "a.txt", "v2");
    scan(&mut a);

    remove(&a, "a.txt");
    scan(&mut a);
    sync(&mut a, &mut b);
    sync(&mut a, &mut b);

    assert_eq!(read(&b, "a.txt"), None, "the delete must reach b");
    assert_eq!(tree(&a), tree(&b));
    assert!(tree(&a).is_empty());
}
