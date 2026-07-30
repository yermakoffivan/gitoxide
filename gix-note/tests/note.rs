use gix_hash::{Kind, ObjectId, oid};
use gix_object::{
    FindExt, Tree, Write,
    bstr::{BString, ByteSlice},
    tree::{Entry, EntryKind},
};

type ObjectDb = gix_odb::memory::Proxy<gix_object::find::Never>;

#[test]
fn reads_notes_without_fanout() -> gix_testtools::Result {
    assert_note_at_fanout(0)
}

#[test]
fn reads_notes_with_one_fanout_level() -> gix_testtools::Result {
    assert_note_at_fanout(1)
}

#[test]
fn reads_notes_with_two_fanout_levels() -> gix_testtools::Result {
    assert_note_at_fanout(2)
}

#[test]
fn reads_notes_with_three_fanout_levels() -> gix_testtools::Result {
    assert_note_at_fanout(3)
}

fn assert_note_at_fanout(fanout: usize) -> gix_testtools::Result {
    let kind = gix_testtools::object_hash();
    let objects = ObjectDb::new(gix_object::find::Never, kind);
    let annotated = gix_object::compute_hash(kind, gix_object::Kind::Blob, b"annotated")?;
    let note = objects.write_buf(gix_object::Kind::Blob, b"note")?;
    let root = notes_tree(&objects, &annotated, note, fanout)?;

    assert_eq!(
        gix_note::get(root, &annotated, &objects)?,
        Some(note),
        "the note is found with {fanout} fanout levels"
    );
    Ok(())
}

fn notes_tree(objects: &ObjectDb, annotated: &oid, note: ObjectId, fanout: usize) -> gix_testtools::Result<ObjectId> {
    let hex = annotated.to_hex().to_string();
    let mut tree = objects.write(&Tree {
        entries: vec![Entry {
            mode: EntryKind::Blob.into(),
            filename: BString::from(&hex[fanout * 2..]),
            oid: note,
        }],
    })?;

    for level in (0..fanout).rev() {
        tree = objects.write(&Tree {
            entries: vec![Entry {
                mode: EntryKind::Tree.into(),
                filename: BString::from(&hex[level * 2..][..2]),
                oid: tree,
            }],
        })?;
    }
    Ok(tree)
}

#[test]
fn ignores_entries_that_are_not_notes() -> gix_testtools::Result {
    let kind = gix_testtools::object_hash();
    let objects = gix_odb::memory::Proxy::new(gix_object::find::Never, kind);
    let annotated = gix_object::compute_hash(kind, gix_object::Kind::Blob, b"annotated")?;
    let hex = annotated.to_hex().to_string();
    let root = objects.write(&Tree {
        entries: vec![Entry {
            mode: EntryKind::Tree.into(),
            filename: BString::from(hex),
            oid: kind.empty_tree(),
        }],
    })?;

    assert_eq!(
        gix_note::get(root, &annotated, &objects)?,
        None,
        "the canonical empty-tree ID in a tree-mode entry at the full object-ID path is not a blob note"
    );
    Ok(())
}

#[test]
fn mutations_rebalance_at_the_sixteenth_prefix_and_preserve_non_notes() -> gix_testtools::Result {
    let kind = gix_testtools::object_hash();
    let objects = ObjectDb::new(gix_object::find::Never, kind);
    let unrelated = objects.write_buf(gix_object::Kind::Blob, b"keep")?;
    let note = objects.write_buf(gix_object::Kind::Blob, b"note")?;
    let mut root = objects.write(&Tree {
        entries: vec![Entry {
            mode: EntryKind::Blob.into(),
            filename: "README".into(),
            oid: unrelated,
        }],
    })?;
    let mut annotated = Vec::new();
    for nibble in b"0123456789abcde" {
        let object = object_id_with_nibble(kind, 0, *nibble)?;
        annotated.push(object);
        let outcome = gix_note::replace(root, object, note, &objects)?;
        assert_eq!(outcome.previous, None, "each distinct object receives a new note");
        root = outcome.tree;
    }

    let mut buf = Vec::new();
    let tree = objects.find_tree(&root, &mut buf)?;
    assert_eq!(
        tree.entries.iter().filter(|entry| entry.mode.is_tree()).count(),
        0,
        "fifteen represented leading nibbles keep notes in the flat layout"
    );
    assert_eq!(
        tree.entries.iter().filter(|entry| entry.mode.is_blob()).count(),
        16,
        "the flat tree contains fifteen notes and README"
    );

    let sixteenth = object_id_with_nibble(kind, 0, b'f')?;
    annotated.push(sixteenth);
    let outcome = gix_note::replace(root, sixteenth, note, &objects)?;
    assert_eq!(outcome.previous, None, "the sixteenth prefix also receives a new note");
    root = outcome.tree;

    let mut buf = Vec::new();
    let tree = objects.find_tree(&root, &mut buf)?;
    assert_eq!(
        tree.entries.iter().filter(|entry| entry.mode.is_tree()).count(),
        16,
        "covering all first nibbles causes Git's first fanout level"
    );
    assert!(
        tree.entries
            .iter()
            .any(|entry| entry.filename == "README" && entry.oid == unrelated),
        "non-note entries survive rebalancing"
    );

    for object in &annotated {
        assert_eq!(
            gix_note::get(root, object, &objects)?,
            Some(note),
            "every note remains readable after fanout"
        );
    }

    let replacement = objects.write_buf(gix_object::Kind::Blob, b"replacement")?;
    let outcome = gix_note::replace(root, annotated[0], replacement, &objects)?;
    assert_eq!(outcome.previous, Some(note), "replacement returns the previous note");
    assert_eq!(
        gix_note::get(outcome.tree, &annotated[0], &objects)?,
        Some(replacement),
        "replacement is visible through lookup"
    );
    let outcome = gix_note::remove(outcome.tree, annotated[0], &objects)?;
    assert_eq!(outcome.previous, Some(replacement), "removal returns the removed note");
    assert_eq!(
        gix_note::get(outcome.tree, &annotated[0], &objects)?,
        None,
        "the removed mapping is no longer visible"
    );
    let mut buf = Vec::new();
    let tree = objects.find_tree(&outcome.tree, &mut buf)?;
    assert_eq!(
        tree.entries.iter().filter(|entry| entry.mode.is_blob()).count(),
        16,
        "dropping one leading nibble collapses the remaining notes to the root beside README"
    );
    Ok(())
}

#[test]
fn edit_lifecycle_handles_empty_trees_replacements_and_no_op_removals() -> gix_testtools::Result {
    let kind = gix_testtools::object_hash();
    let objects = ObjectDb::new(gix_object::find::Never, kind);
    let root = objects.write(&Tree { entries: Vec::new() })?;
    let annotated = gix_object::compute_hash(kind, gix_object::Kind::Blob, b"annotated")?;
    let child = objects.write_buf(gix_object::Kind::Blob, b"tree contents")?;
    let tree_note = objects.write(&Tree {
        entries: vec![Entry {
            mode: EntryKind::Blob.into(),
            filename: "file".into(),
            oid: child,
        }],
    })?;

    assert_eq!(
        gix_note::get(root, &annotated, &objects)?,
        None,
        "an empty notes tree has no mapping"
    );
    let absent = gix_note::remove(root, annotated, &objects)?;
    assert_eq!(absent.previous, None, "removing an absent mapping has no previous note");
    assert_eq!(
        absent.tree, root,
        "removing an absent mapping leaves the root unchanged"
    );

    let added = gix_note::replace(root, annotated, tree_note, &objects)?;
    assert_eq!(added.previous, None, "adding the first mapping has no previous note");
    assert_eq!(
        gix_note::get(added.tree, &annotated, &objects)?,
        Some(tree_note),
        "lookup returns a note even when its object is actually a tree"
    );
    assert_note_layout(&objects, added.tree, &annotated, 0, tree_note)?;

    let replacement = objects.write_buf(gix_object::Kind::Blob, b"replacement")?;
    let replaced = gix_note::replace(added.tree, annotated, replacement, &objects)?;
    assert_eq!(
        replaced.previous,
        Some(tree_note),
        "replacement returns the tree-valued note"
    );
    assert_eq!(
        gix_note::get(replaced.tree, &annotated, &objects)?,
        Some(replacement),
        "lookup observes the replacement"
    );

    let removed = gix_note::remove(replaced.tree, annotated, &objects)?;
    assert_eq!(
        removed.previous,
        Some(replacement),
        "removal returns the replacement note"
    );
    assert_eq!(
        removed.tree,
        kind.empty_tree(),
        "removing the last note produces the empty tree"
    );
    let absent = gix_note::remove(removed.tree, annotated, &objects)?;
    assert_eq!(absent.previous, None, "a repeated removal is a no-op");
    assert_eq!(absent.tree, removed.tree, "a repeated removal retains the empty root");
    Ok(())
}

#[test]
fn mutations_create_and_collapse_mixed_deep_fanout() -> gix_testtools::Result {
    let kind = gix_testtools::object_hash();
    let objects = ObjectDb::new(gix_object::find::Never, kind);
    let note = objects.write_buf(gix_object::Kind::Blob, b"note")?;
    let mut root = objects.write(&Tree { entries: Vec::new() })?;
    let mut annotated = Vec::new();

    for nibble in b"0123456789abcdef" {
        annotated.push(object_id_with_nibble(kind, 0, *nibble)?);
    }
    for nibble in b"123456789abcdef" {
        annotated.push(object_id_with_nibble(kind, 2, *nibble)?);
    }
    for nibble in b"123456789abcdef" {
        annotated.push(object_id_with_nibble(kind, 4, *nibble)?);
    }
    for object in &annotated {
        let outcome = gix_note::replace(root, *object, note, &objects)?;
        assert_eq!(outcome.previous, None, "every generated object is unique");
        root = outcome.tree;
    }

    let one_level = object_id_with_nibble(kind, 0, b'f')?;
    let two_levels = object_id_with_nibble(kind, 2, b'f')?;
    let last_in_level_three = object_id_with_nibble(kind, 4, b'f')?;
    assert_note_layout(&objects, root, &one_level, 1, note)?;
    assert_note_layout(&objects, root, &two_levels, 2, note)?;
    assert_note_layout(&objects, root, &last_in_level_three, 3, note)?;
    for object in &annotated {
        assert_eq!(
            gix_note::get(root, object, &objects)?,
            Some(note),
            "mixed fanout depths remain readable"
        );
    }

    let outcome = gix_note::remove(root, last_in_level_three, &objects)?;
    assert_eq!(outcome.previous, Some(note), "deep removal returns its note");
    assert_eq!(
        gix_note::get(outcome.tree, &last_in_level_three, &objects)?,
        None,
        "the deep mapping is removed, collapsing a level"
    );
    // Removing the sixteenth note from the deepest group drops it below the fanout threshold.
    // Its surviving `e` sibling must therefore move from a three-level path to a two-level path.
    let formerly_three_levels = object_id_with_nibble(kind, 4, b'e')?;
    assert_note_layout(&objects, outcome.tree, &formerly_three_levels, 2, note)?;
    assert_note_layout(&objects, outcome.tree, &two_levels, 2, note)?;
    assert_note_layout(&objects, outcome.tree, &one_level, 1, note)?;
    Ok(())
}

#[test]
fn mutations_preserve_non_notes_at_root_and_below_hex_trees() -> gix_testtools::Result {
    let kind = gix_testtools::object_hash();
    let objects = ObjectDb::new(gix_object::find::Never, kind);
    let payload = objects.write_buf(gix_object::Kind::Blob, b"keep")?;
    let nested = objects.write(&Tree {
        entries: vec![Entry {
            mode: EntryKind::Blob.into(),
            filename: "README".into(),
            oid: payload,
        }],
    })?;
    let existing_object = gix_object::compute_hash(kind, gix_object::Kind::Blob, b"existing")?;
    let full_hex_tree = existing_object.to_hex().to_string();
    let full_non_hex_blob = "g".repeat(kind.len_in_hex());
    let root = objects.write(&Tree {
        entries: vec![
            Entry {
                mode: EntryKind::Tree.into(),
                filename: full_hex_tree.clone().into(),
                oid: kind.empty_tree(),
            },
            Entry {
                mode: EntryKind::BlobExecutable.into(),
                filename: "ab".into(),
                oid: payload,
            },
            Entry {
                mode: EntryKind::Tree.into(),
                filename: "cd".into(),
                oid: nested,
            },
            Entry {
                mode: EntryKind::Blob.into(),
                filename: full_non_hex_blob.clone().into(),
                oid: payload,
            },
            Entry {
                mode: EntryKind::Tree.into(),
                filename: "zz".into(),
                oid: nested,
            },
        ],
    })?;
    let annotated = gix_object::compute_hash(kind, gix_object::Kind::Blob, b"new")?;
    let note = objects.write_buf(gix_object::Kind::Blob, b"note")?;
    let outcome = gix_note::replace(root, annotated, note, &objects)?;

    assert_entry_at_path(&objects, outcome.tree, &["ab"], EntryKind::BlobExecutable, payload)?;
    assert_entry_at_path(&objects, outcome.tree, &["cd", "README"], EntryKind::Blob, payload)?;
    assert_entry_at_path(
        &objects,
        outcome.tree,
        &[full_hex_tree.as_str()],
        EntryKind::Tree,
        kind.empty_tree(),
    )?;
    assert_entry_at_path(
        &objects,
        outcome.tree,
        &[full_non_hex_blob.as_str()],
        EntryKind::Blob,
        payload,
    )?;
    assert_entry_at_path(&objects, outcome.tree, &["zz"], EntryKind::Tree, nested)?;
    assert_eq!(
        gix_note::get(outcome.tree, &annotated, &objects)?,
        Some(note),
        "the new note coexists with all preserved entries"
    );
    Ok(())
}

#[test]
fn mutations_reject_mixed_hash_kinds_before_reading_objects() -> gix_testtools::Result {
    let objects = ObjectDb::new(gix_object::find::Never, Kind::Sha1);
    let root = objects.write(&Tree { entries: Vec::new() })?;
    let sha1 = ObjectId::null(Kind::Sha1);
    let sha256 = ObjectId::null(Kind::Sha256);

    let err =
        gix_note::replace(root, sha256, sha1, &objects).expect_err("the annotated object has the wrong hash kind");
    let err = err.into_error();
    assert!(
        err.is_validation(),
        "an annotated-object hash mismatch is a validation error"
    );
    assert_eq!(
        err.probable_cause().to_string(),
        "Notes, annotated objects, and their root tree must use the same hash kind",
        "replace reports an annotated-object hash mismatch"
    );
    let err = gix_note::replace(root, sha1, sha256, &objects).expect_err("the note has the wrong hash kind");
    let err = err.into_error();
    assert!(err.is_validation(), "a note hash mismatch is a validation error");
    assert_eq!(
        err.probable_cause().to_string(),
        "Notes, annotated objects, and their root tree must use the same hash kind",
        "replace reports a note hash mismatch"
    );
    let err = gix_note::remove(root, sha256, &objects).expect_err("the annotated object has the wrong hash kind");
    let err = err.into_error();
    assert!(
        err.is_validation(),
        "an annotated-object hash mismatch is a validation error"
    );
    assert_eq!(
        err.probable_cause().to_string(),
        "The annotated object and notes root tree must use the same hash kind",
        "remove reports an annotated-object hash mismatch"
    );
    Ok(())
}

#[test]
#[cfg(feature = "sha256")]
fn edits_support_sha256_notes_trees() -> gix_testtools::Result {
    let kind = Kind::Sha256;
    let objects = ObjectDb::new(gix_object::find::Never, kind);
    let root = objects.write(&Tree { entries: Vec::new() })?;
    let annotated = gix_object::compute_hash(kind, gix_object::Kind::Blob, b"annotated")?;
    let note = objects.write_buf(gix_object::Kind::Blob, b"note")?;

    let added = gix_note::replace(root, annotated, note, &objects)?;
    assert_eq!(added.previous, None, "the SHA-256 mapping is new");
    assert_eq!(added.tree.kind(), kind, "the rewritten root retains its hash kind");
    assert_eq!(
        gix_note::get(added.tree, &annotated, &objects)?,
        Some(note),
        "SHA-256 notes can be read after insertion"
    );
    let removed = gix_note::remove(added.tree, annotated, &objects)?;
    assert_eq!(removed.previous, Some(note), "SHA-256 removal returns the note");
    assert_eq!(
        removed.tree,
        kind.empty_tree(),
        "removing the last SHA-256 note yields its empty tree"
    );
    Ok(())
}

#[test]
fn mutations_reject_duplicate_mappings_across_layouts() -> gix_testtools::Result {
    let kind = gix_testtools::object_hash();
    let objects = ObjectDb::new(gix_object::find::Never, kind);
    let annotated = ObjectId::null(kind);
    let hex = annotated.to_hex().to_string();
    let flat_note = objects.write_buf(gix_object::Kind::Blob, b"flat")?;
    let fanout_note = objects.write_buf(gix_object::Kind::Blob, b"fanout")?;
    let subtree = objects.write(&Tree {
        entries: vec![Entry {
            mode: EntryKind::Blob.into(),
            filename: hex[2..].into(),
            oid: fanout_note,
        }],
    })?;
    let root = objects.write(&Tree {
        entries: vec![
            Entry {
                mode: EntryKind::Tree.into(),
                filename: hex[..2].into(),
                oid: subtree,
            },
            Entry {
                mode: EntryKind::Blob.into(),
                filename: hex.into(),
                oid: flat_note,
            },
        ],
    })?;
    let other = gix_object::compute_hash(kind, gix_object::Kind::Blob, b"other")?;

    let err = gix_note::replace(root, other, flat_note, &objects)
        .expect_err("ambiguous existing mappings cannot be rewritten losslessly");
    let err = err.into_error();
    assert!(err.is_corrupted(), "duplicate mappings indicate a corrupt notes tree");
    assert_eq!(
        err.probable_cause().to_string(),
        format!("Multiple notes map to object {annotated}"),
        "mutations diagnose duplicate flat and fanout mappings"
    );
    Ok(())
}

fn object_id_with_nibble(kind: Kind, offset: usize, nibble: u8) -> gix_testtools::Result<ObjectId> {
    let mut hex = vec![b'0'; kind.len_in_hex()];
    hex[offset] = nibble;
    Ok(ObjectId::from_hex(&hex)?)
}

fn assert_note_layout(
    objects: &ObjectDb,
    root: ObjectId,
    annotated: &oid,
    fanout: usize,
    note: ObjectId,
) -> gix_testtools::Result {
    let hex = annotated.to_hex().to_string();
    let mut path = Vec::with_capacity(fanout + 1);
    for level in 0..fanout {
        path.push(&hex[level * 2..][..2]);
    }
    path.push(&hex[fanout * 2..]);
    assert_entry_at_path(objects, root, &path, EntryKind::Blob, note)
}

fn assert_entry_at_path(
    objects: &ObjectDb,
    root: ObjectId,
    path: &[&str],
    expected_kind: EntryKind,
    expected_oid: ObjectId,
) -> gix_testtools::Result {
    let mut tree_id = root;
    let mut buf = Vec::new();
    for (index, component) in path.iter().enumerate() {
        let is_last = index + 1 == path.len();
        let expected_tree = if is_last {
            expected_kind == EntryKind::Tree
        } else {
            true
        };
        let tree = objects.find_tree(&tree_id, &mut buf)?;
        let entry = tree
            .bisect_entry(component.as_bytes().as_bstr(), expected_tree)
            .unwrap_or_else(|| panic!("entry at {} is present", path[..=index].join("/")));
        if is_last {
            assert_eq!(
                entry.mode.kind(),
                expected_kind,
                "entry at {} retains its mode",
                path.join("/")
            );
            assert_eq!(
                entry.oid,
                expected_oid,
                "entry at {} retains its object ID",
                path.join("/")
            );
        } else {
            assert!(
                entry.mode.is_tree(),
                "{} is an intermediate tree",
                path[..=index].join("/")
            );
            tree_id = entry.oid.to_owned();
        }
    }
    Ok(())
}
