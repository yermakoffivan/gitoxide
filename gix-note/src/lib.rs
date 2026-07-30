//! Read Git notes from notes trees.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::{BTreeMap, HashMap};

use gix_error::{CorruptionError, ErrorExt, ResultExt, ValidationError, message};
use gix_hash::{ObjectId, oid};
use gix_object::{
    Find, FindExt, Tree, Write,
    bstr::{BStr, BString, ByteSlice},
    tree::{Editor, EntryKind, EntryMode},
};

/// The type-erased error returned by note operations.
pub type Error = gix_error::Exn;

/// The result of changing one note mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Edit {
    /// The root tree containing the changed notes.
    pub tree: ObjectId,
    /// The object ID of the note previously associated with the annotated object.
    ///
    /// This is `Some` when [`replace()`] replaced an existing note or [`remove()`]
    /// removed one. It is `None` when adding a new mapping or when removal
    /// found no matching note. Note IDs are expected to reference blobs, but
    /// their object kind is not verified.
    pub previous: Option<ObjectId>,
}

/// Return the note associated with `object` in the notes tree at `root`.
///
/// Git notes are expected to reference blobs. This function verifies that the
/// notes-tree entry has blob mode, but does not load the referenced object to
/// verify its actual kind.
///
/// Trees are loaded lazily along the progressive two-hex-digit fanout path.
/// Entries that do not conform to Git's notes layout are ignored.
///
/// For repeated lookups, `objects` should have a built-in object cache to
/// accelerate tree retrieval.
pub fn get(root: ObjectId, object: &oid, objects: &impl Find) -> Result<Option<ObjectId>, Error> {
    let hex = object.to_hex().to_string();
    let mut remaining = hex.as_bytes().as_bstr();
    let mut tree_id = root;
    let mut buf = Vec::new();

    loop {
        let tree = objects
            .find_tree(&tree_id, &mut buf)
            .or_raise_erased(|| message!("Could not load notes tree {tree_id}"))?;
        if let Some(entry) = tree.bisect_entry(remaining, false).filter(|entry| entry.mode.is_blob()) {
            return Ok(Some(entry.oid.to_owned()));
        }
        let Some(component) = remaining.get(..2).filter(|_| remaining.len() > 2) else {
            return Ok(None);
        };
        let Some(subtree) = tree
            .bisect_entry(component.into(), true)
            .filter(|entry| entry.mode.is_tree())
        else {
            return Ok(None);
        };
        tree_id = subtree.oid.to_owned();
        remaining = remaining[2..].as_bstr();
    }
}

/// Replace the note for `object`, or add it if absent, returning the new root
/// tree and any previous note.
///
/// The notes tree is rewritten with the same progressive fanout heuristic as
/// Git while retaining entries that are not notes. `note` is expected to
/// reference a blob, but its actual object kind is not verified; the mapping is
/// always written as a blob-mode tree entry. For repeated edits, `objects`
/// should have a built-in object cache to accelerate tree retrieval.
pub fn replace(root: ObjectId, object: ObjectId, note: ObjectId, objects: &(impl Find + Write)) -> Result<Edit, Error> {
    if object.kind() != root.kind() || note.kind() != root.kind() {
        return Err(
            ValidationError::from("Notes, annotated objects, and their root tree must use the same hash kind")
                .raise_erased(),
        );
    }
    edit(root, object, Some(note), objects)
}

/// Remove the note for `object`, returning the new root tree and removed note.
///
/// If there is no such note, the root is returned unchanged. For repeated
/// edits, `objects` should have a built-in object cache to accelerate tree
/// retrieval.
pub fn remove(root: ObjectId, object: ObjectId, objects: &(impl Find + Write)) -> Result<Edit, Error> {
    if object.kind() != root.kind() {
        return Err(
            ValidationError::from("The annotated object and notes root tree must use the same hash kind")
                .raise_erased(),
        );
    }
    edit(root, object, None, objects)
}

fn edit(
    root: ObjectId,
    object: ObjectId,
    note: Option<ObjectId>,
    objects: &(impl Find + Write),
) -> Result<Edit, Error> {
    let mut notes = BTreeMap::new();
    let mut non_notes = Vec::new();
    collect(
        root,
        BString::default(),
        Vec::new(),
        objects,
        &mut notes,
        &mut non_notes,
    )?;
    let previous = match note {
        Some(note) => notes.insert(object, note),
        None => notes.remove(&object),
    };
    if note.is_none() && previous.is_none() {
        return Ok(Edit { tree: root, previous });
    }
    let tree = write(notes, non_notes, root.kind(), objects)?;
    Ok(Edit { tree, previous })
}

#[derive(Clone)]
struct NonNote {
    path: Vec<BString>,
    mode: EntryMode,
    oid: ObjectId,
}

fn collect(
    tree_id: ObjectId,
    hex_prefix: BString,
    path_prefix: Vec<BString>,
    objects: &impl Find,
    notes: &mut BTreeMap<ObjectId, ObjectId>,
    non_notes: &mut Vec<NonNote>,
) -> Result<(), Error> {
    let mut buf = Vec::new();
    let tree = objects
        .find_tree(&tree_id, &mut buf)
        .or_raise_erased(|| message!("Could not load notes tree {tree_id}"))?;
    let hex_len = tree_id.kind().len_in_hex();
    for entry in tree.entries {
        let mut path = path_prefix.clone();
        path.push(entry.filename.to_owned());
        if entry.mode.is_blob() && entry.filename.len() + hex_prefix.len() == hex_len {
            let mut hex = hex_prefix.clone();
            hex.extend_from_slice(entry.filename);
            if let Ok(object) = ObjectId::from_hex(&hex) {
                if notes.insert(object, entry.oid.to_owned()).is_some() {
                    return Err(CorruptionError::from(format!("Multiple notes map to object {object}")).raise_erased());
                }
                continue;
            }
        }
        if entry.mode.is_tree()
            && entry.filename.len() == 2
            && hex_prefix.len() + 2 < hex_len
            && entry.filename.iter().all(u8::is_ascii_hexdigit)
        {
            let mut prefix = hex_prefix.clone();
            prefix.extend_from_slice(entry.filename);
            collect(entry.oid.to_owned(), prefix, path, objects, notes, non_notes)?;
        } else {
            non_notes.push(NonNote {
                path,
                mode: entry.mode,
                oid: entry.oid.to_owned(),
            });
        }
    }
    Ok(())
}

fn write(
    notes: BTreeMap<ObjectId, ObjectId>,
    non_notes: Vec<NonNote>,
    hash: gix_hash::Kind,
    objects: &(impl Find + Write),
) -> Result<ObjectId, Error> {
    let mut editor = Editor::new(Tree { entries: Vec::new() }, objects, hash);
    for entry in non_notes {
        editor
            .upsert(entry.path.iter(), entry.mode.kind(), entry.oid)
            .or_raise_erased(|| message("Could not restore a non-note tree entry"))?;
    }

    let hexes: Vec<_> = notes.keys().map(|id| id.to_hex().to_string()).collect();
    let masks = fanout_masks(&hexes);
    for ((_, note), hex) in notes.into_iter().zip(hexes) {
        let fanout = fanout(&hex, &masks);
        let path = note_path(&hex, fanout);
        editor
            .upsert(path.split_str("/"), EntryKind::Blob, note)
            .or_raise_erased(|| message("Could not add a note tree entry"))?;
    }
    editor
        .write(|tree| objects.write(tree).map_err(gix_error::Error::from_boxed))
        .or_raise_erased(|| message("Could not write the notes tree"))
}

fn fanout_masks(hexes: &[String]) -> HashMap<BString, u16> {
    let mut out = HashMap::new();
    for hex in hexes {
        let bytes = hex.as_bytes();
        for offset in (0..bytes.len().saturating_sub(2)).step_by(2) {
            let Some(nibble) = hex_nibble(bytes[offset]) else {
                continue;
            };
            *out.entry(BString::from(&bytes[..offset])).or_default() |= 1 << nibble;
        }
    }
    out
}

fn fanout(hex: &str, masks: &HashMap<BString, u16>) -> usize {
    let mut fanout = 0;
    while fanout * 2 < hex.len().saturating_sub(2)
        && masks.get(BStr::new(&hex.as_bytes()[..fanout * 2])) == Some(&u16::MAX)
    {
        fanout += 1;
    }
    fanout
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn note_path(hex: &str, fanout: usize) -> BString {
    let mut out = BString::new(Vec::with_capacity(hex.len() + fanout));
    for component in hex.as_bytes()[..fanout * 2].chunks_exact(2) {
        out.extend_from_slice(component);
        out.push(b'/');
    }
    out.extend_from_slice(&hex.as_bytes()[fanout * 2..]);
    out
}
