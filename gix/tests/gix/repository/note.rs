#[test]
fn query_and_mutate_a_configured_notes_ref() -> crate::Result {
    let (mut repo, _tmp) = crate::util::basic_rw_repo()?;
    let mut config = repo.config_snapshot_mut();
    config.set_value(&gix::config::tree::Core::NOTES_REF, "refs/notes/review")?;
    config.commit()?;

    assert!(
        repo.try_find_reference("refs/notes/review")?.is_none(),
        "configuring a notes reference does not create it"
    );
    let target = repo.write_blob(b"annotated")?.detach();
    let mut notes = repo.notes()?;
    assert_eq!(
        notes.default_ref().map(ToString::to_string).as_deref(),
        Some("refs/notes/review"),
        "core.notesRef configures the default notes reference"
    );
    assert_eq!(
        notes.refs().map(ToString::to_string).collect::<Vec<_>>(),
        ["refs/notes/review"],
        "the default reference is initially the only selected notes reference"
    );
    assert!(notes.get(target)?.is_empty(), "the target initially has no notes");

    assert_eq!(
        notes.replace("review", target, b"first")?,
        None,
        "adding the first note does not replace an existing note"
    );
    assert!(
        repo.try_find_reference("refs/notes/review")?.is_some(),
        "adding a note creates its previously absent notes reference"
    );
    let found = notes.get(target)?;
    assert_eq!(found.len(), 1, "the target has exactly one note after insertion");
    assert_eq!(
        found[0].reference.to_string(),
        "refs/notes/review",
        "the note is found through the configured notes reference"
    );
    assert_eq!(found[0].blob.data, b"first", "the inserted note data is returned");
    drop(found);

    let previous = notes
        .replace("review", target, b"second")?
        .expect("the first note is replaced");
    assert_eq!(
        repo.find_blob(previous)?.data,
        b"first",
        "replacement returns the previous note object"
    );
    assert_eq!(
        notes.remove("review", target)?,
        Some(repo.write_blob(b"second")?.detach()),
        "removal returns the object ID of the replacement note"
    );
    assert!(notes.get(target)?.is_empty(), "the target has no notes after removal");
    assert!(
        repo.try_find_reference("refs/notes/review")?.is_some(),
        "removing the last note preserves the notes reference"
    );
    Ok(())
}

#[test]
fn custom_commit_message_is_used_for_mutations() -> crate::Result {
    let (repo, _tmp) = crate::util::basic_rw_repo()?;
    let annotated_blob_id = repo.write_blob(b"annotated")?.detach();
    let mut notes = repo.notes()?.with_commit_message("custom notes update");

    notes.replace("review", annotated_blob_id, b"note")?;
    {
        let commit = repo.find_reference("refs/notes/review")?.peel_to_commit()?;
        assert_eq!(
            commit.message_raw()?,
            "custom notes update",
            "replacement uses the configured commit message"
        );
    }

    notes.remove("review", annotated_blob_id)?;
    let commit = repo.find_reference("refs/notes/review")?.peel_to_commit()?;
    assert_eq!(
        commit.message_raw()?,
        "custom notes update",
        "removal uses the configured commit message"
    );
    Ok(())
}

#[test]
fn query_and_mutate_multiple_notes_refs() -> crate::Result {
    let (repo, _tmp) = crate::util::basic_rw_repo()?;
    let target = repo.write_blob(b"annotated")?.detach();
    let notes_refs = ["refs/notes/review", "refs/notes/security"];
    let mut notes = repo.notes()?.with_refs(notes_refs)?;
    assert_eq!(
        notes.refs().map(ToString::to_string).collect::<Vec<_>>(),
        notes_refs,
        "explicitly selected notes references are available in lookup order"
    );

    for name in notes_refs {
        assert!(
            repo.try_find_reference(name)?.is_none(),
            "selecting {name} does not create it"
        );
    }

    assert_eq!(
        notes.replace("review", target, b"review note")?,
        None,
        "adding the review note creates a new mapping"
    );
    assert_eq!(
        notes.replace("notes/security", target, b"security note")?,
        None,
        "adding the security note creates an independent mapping"
    );

    for name in notes_refs {
        assert!(
            repo.try_find_reference(name)?.is_some(),
            "writing a note auto-creates {name}"
        );
    }

    let unmatched = repo.notes()?.with_refs(["/refs/notes/*", "refs/notes/*/"])?;
    assert_eq!(
        unmatched.refs().count(),
        0,
        "reference names have neither a leading nor trailing slash"
    );

    notes = notes.with_refs(["refs/notes/revie?", "refs/notes/[s]ecurity"])?;
    assert_eq!(
        notes.refs().map(ToString::to_string).collect::<Vec<_>>(),
        notes_refs,
        "question-mark and bracket globs expand without requiring an asterisk"
    );

    let found = notes.get(target)?;
    assert_eq!(found.len(), 2, "one note is returned from each selected reference");
    assert_eq!(
        found[0].reference.to_string(),
        "refs/notes/review",
        "the first note follows the selected reference order"
    );
    assert_eq!(found[0].blob.data, b"review note", "the review note data is returned");
    assert_eq!(
        found[1].reference.to_string(),
        "refs/notes/security",
        "the second note follows the selected reference order"
    );
    assert_eq!(
        found[1].blob.data, b"security note",
        "the security note data is returned"
    );
    Ok(())
}
