//! Access Git notes.

use std::borrow::Cow;

/// Low-level operations on Git notes trees.
pub use gix_note as plumbing;

use gix_error::{ErrorExt, ResultExt, message};

use crate::{
    Blob, Repository,
    bstr::{BStr, BString, ByteSlice, ByteVec},
    config::tree::{Core, Key, Notes},
    refs::{FullName, FullNameRef, PartialName, PartialNameRef, transaction::PreviousValue},
};

/// A note and the reference from which it originated.
pub struct Note<'platform, 'repo> {
    /// The source notes reference, such as `refs/notes/commits`.
    pub reference: &'platform FullNameRef,
    /// The note blob in any format.
    pub blob: Blob<'repo>,
}

/// Cached access to one or more notes references.
pub struct Platform<'repo> {
    // TODO(ST): make this owned once there is a cache, so it's easier to use reliably.
    pub(crate) repo: &'repo Repository,
    pub(crate) default_ref: Option<FullName>,
    roots: Vec<Root>,
    commit_message: Option<String>,
}

impl<'repo> Platform<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Result<Self, crate::Error> {
        let config = repo.config_snapshot();
        let value = config
            .string(Core::NOTES_REF)
            .unwrap_or_else(|| Core::NOTES_REF.default_value_or_panic().into());
        let default_ref = if value.is_empty() {
            None
        } else {
            Some(
                FullName::try_from(value)
                    .or_raise(|| message("core.notesRef must be a fully qualified reference name"))?,
            )
        };
        let mut refs = default_ref.iter().cloned().collect::<Vec<_>>();
        for value in config.plumbing().strings(Notes::DISPLAY_REF).unwrap_or_default() {
            let display_refs = Notes::DISPLAY_REF
                .try_into_display_refs(value)
                .or_raise(|| message("Could not parse notes display references"))?;
            for pattern in display_refs {
                add_refs(repo, pattern.as_bstr(), &mut refs)?;
            }
        }
        Ok(Platform {
            repo,
            default_ref,
            roots: refs.into_iter().map(Root::new).collect(),
            commit_message: None,
        })
    }
}

/// Builder.
impl Platform<'_> {
    /// Replace the references searched by [`Self::get()`] with `refs`.
    ///
    /// Each item is either a fully qualified literal reference such as `refs/notes/review`, or a
    /// glob pattern. An item containing `*`, `?`, or `[` is treated as a glob and expanded against
    /// existing references. A literal reference may be absent and is then treated as containing no
    /// notes. Duplicate references are ignored and the resulting order is preserved.
    pub fn with_refs(mut self, refs: impl IntoIterator<Item = impl Into<BString>>) -> Result<Self, crate::Error> {
        let mut selected = Vec::new();
        for pattern in refs {
            let pattern = pattern.into();
            add_refs(self.repo, pattern.as_bstr(), &mut selected)?;
        }
        self.roots = selected.into_iter().map(Root::new).collect();
        Ok(self)
    }

    /// Override the commit message used by subsequent note replacements and removals.
    ///
    /// Without an override, each operation uses its own gitoxide-specific default message.
    pub fn with_commit_message(mut self, message: impl Into<String>) -> Self {
        self.commit_message = Some(message.into());
        self
    }
}

impl<'repo> Platform<'repo> {
    /// Return the default notes reference selected by `core.notesRef`, if enabled.
    ///
    /// This is distinct from non-default display references searched by [`Self::get()`],
    /// which may be selected through `notes.displayRef`, `GIT_NOTES_DISPLAY_REF`, or
    /// [`Self::with_refs()`]. If `core.notesRef` is unset, the default is
    /// `refs/notes/commits`; an empty value disables the default reference.
    pub fn default_ref(&self) -> Option<&gix_ref::FullNameRef> {
        self.default_ref.as_ref().map(AsRef::as_ref)
    }

    /// Return the selected notes references in lookup order.
    ///
    /// This includes the enabled default reference and any display references selected through
    /// configuration, the environment, or [`Self::with_refs()`]. A selected literal reference
    /// may not exist yet, in which case it simply contains no notes.
    pub fn refs(&self) -> impl Iterator<Item = &gix_ref::FullNameRef> {
        self.roots.iter().map(|root| root.reference.as_ref())
    }

    /// Return all notes associated with `object` in configured display order.
    ///
    /// A notes reference associates at most one note with an object, but the selected
    /// default and non-default display references may each annotate the same object.
    /// Consequently, this may return one [`Note`] per selected reference;
    /// [`Note::reference`] identifies where each note originated.
    pub fn get<'platform>(
        &'platform mut self,
        object: impl Into<gix_hash::ObjectId>,
    ) -> Result<Vec<Note<'platform, 'repo>>, crate::Error> {
        let object = object.into();
        let mut out = Vec::new();
        for selected in &mut self.roots {
            let Some(root) = selected.tree_id(self.repo)? else {
                continue;
            };
            if let Some(note) =
                gix_note::get(root, &object, &self.repo).or_raise(|| message!("Could not find notes for {object}"))?
            {
                let reference = selected.reference.as_ref();
                let blob = self
                    .repo
                    .find_blob(note)
                    .or_raise(|| message!("Could not load note {note} from {reference}"))?;
                out.push(Note { reference, blob });
            }
        }
        Ok(out)
    }

    /// Replace a note in `notes_ref`, assigned to `object`, or add it if absent, returning the previous note id.
    ///
    /// * `notes_ref` identifies the notes reference to update. It may be given as `review`,
    ///   `notes/review`, or the fully qualified `refs/notes/review`. If it does not exist,
    ///   it is created.
    /// * `object` identifies the Git object to annotate.
    /// * `data` is stored verbatim in a new blob and used as the note.
    pub fn replace<N>(
        &mut self,
        notes_ref: N,
        object: impl Into<gix_hash::ObjectId>,
        data: impl AsRef<[u8]>,
    ) -> Result<Option<gix_hash::ObjectId>, crate::Error>
    where
        N: TryInto<PartialName>,
        N::Error: std::error::Error + Send + Sync + 'static,
    {
        let notes_ref = notes_ref
            .try_into()
            .or_raise(|| message("The notes reference name is invalid"))?;
        let notes_ref = expand_notes_ref(notes_ref.as_ref())?;
        let (root_tree_id, parent_commit_id) = self.lookup_edit_root(notes_ref.as_ref())?;
        let object = object.into();
        let note = self
            .repo
            .write_blob(data)
            .or_raise(|| message!("Could not write note for {object}"))?
            .detach();
        let edit = gix_note::replace(root_tree_id, object, note, &self.repo)
            .or_raise(|| message!("Could not replace note for {object}"))?;
        self.commit_edit(notes_ref.as_ref(), parent_commit_id, edit, "Notes replaced by gitoxide")?;
        Ok(edit.previous)
    }

    /// Remove a note in `notes_ref`, returning the removed note id.
    ///
    /// * `notes_ref` identifies the notes reference to update. It may be given as `review`,
    ///   `notes/review`, or the fully qualified `refs/notes/review`. If it does not exist,
    ///   removal is a no-op and the reference remains absent. Removing the last note from an
    ///   existing reference retains it, pointing to a new commit with an empty tree.
    /// * `object` identifies the Git object whose note should be removed.
    pub fn remove<N>(
        &mut self,
        notes_ref: N,
        object: impl Into<gix_hash::ObjectId>,
    ) -> Result<Option<gix_hash::ObjectId>, crate::Error>
    where
        N: TryInto<PartialName>,
        N::Error: std::error::Error + Send + Sync + 'static,
    {
        let notes_ref = notes_ref
            .try_into()
            .or_raise(|| message("The notes reference name is invalid"))?;
        let notes_ref = expand_notes_ref(notes_ref.as_ref())?;
        let (root_tree_id, parent_commit_id) = self.lookup_edit_root(notes_ref.as_ref())?;
        let object = object.into();
        let edit = gix_note::remove(root_tree_id, object, &self.repo)
            .or_raise(|| message!("Could not remove note for {object}"))?;
        if edit.previous.is_some() {
            self.commit_edit(notes_ref.as_ref(), parent_commit_id, edit, "Notes removed by gitoxide")?;
        }
        Ok(edit.previous)
    }

    /// Return the notes root tree ID and the current notes commit ID to use as parent as `(root_tree_id, maybe_parent_commit_id)`.
    /// If `notes_ref` can't be found, return the empty tree ID and `None` as parent.
    fn lookup_edit_root(
        &self,
        notes_ref: &gix_ref::FullNameRef,
    ) -> Result<(gix_hash::ObjectId, Option<gix_hash::ObjectId>), crate::Error> {
        match self
            .repo
            .try_find_reference(notes_ref)
            .or_raise(|| message!("Could not find notes reference {notes_ref}"))?
        {
            Some(mut reference) => {
                let parent = reference
                    .try_id()
                    .ok_or_else(|| message!("Notes reference {notes_ref} must be direct").raise())?
                    .detach();
                let root = reference
                    .peel_to_tree()
                    .or_raise(|| message!("Could not peel notes reference {notes_ref} to a tree"))?
                    .id;
                Ok((root, Some(parent)))
            }
            None => Ok((gix_hash::ObjectId::empty_tree(self.repo.object_hash()), None)),
        }
    }

    /// Commit `edit` to `notes_ref` and refresh its cached root.
    ///
    /// Create the reference if `parent` is `None`; otherwise update it only if it still points to `parent`.
    fn commit_edit(
        &mut self,
        notes_ref: &gix_ref::FullNameRef,
        parent_commit: Option<gix_hash::ObjectId>,
        edit: gix_note::Edit,
        default_message: &str,
    ) -> Result<(), crate::Error> {
        let message = self.commit_message.as_deref().unwrap_or(default_message);
        let commit = self
            .repo
            .new_commit(message, edit.tree, parent_commit)
            .or_raise(|| message!("Could not create commit for {notes_ref}"))?;
        let expected = parent_commit.map_or(PreviousValue::MustNotExist, |id| {
            PreviousValue::MustExistAndMatch(gix_ref::Target::Object(id))
        });
        self.repo
            .reference(notes_ref, commit.id, expected, format!("notes: {message}"))
            .or_raise(|| message!("Could not update notes reference {notes_ref}"))?;
        for root in &mut self.roots {
            if root.reference.as_ref() == notes_ref {
                root.tree_id = Some(Some(edit.tree));
            }
        }
        Ok(())
    }
}

/// A selected notes reference and its lazily resolved root tree.
struct Root {
    /// The selected notes reference, such as `refs/notes/commits`.
    reference: FullName,
    /// `None` means unresolved, `Some(None)` means the reference is absent, and
    /// `Some(Some(id))` caches its resolved root tree.
    tree_id: Option<Option<gix_hash::ObjectId>>,
}

impl Root {
    fn new(reference: FullName) -> Self {
        Root {
            reference,
            tree_id: None,
        }
    }

    fn tree_id(&mut self, repo: &Repository) -> Result<Option<gix_hash::ObjectId>, crate::Error> {
        if let Some(root) = self.tree_id {
            return Ok(root);
        }

        let name = &self.reference;
        let root = match repo
            .try_find_reference(name.as_ref())
            .or_raise(|| message!("Could not find notes reference {name}"))?
        {
            Some(mut reference) => Some(
                reference
                    .peel_to_tree()
                    .or_raise(|| message!("Could not peel notes reference {name} to a tree"))?
                    .id,
            ),
            None => None,
        };
        self.tree_id = Some(root);
        Ok(root)
    }
}

fn add_refs(repo: &Repository, pattern: &BStr, out: &mut Vec<FullName>) -> Result<(), crate::Error> {
    let mut push_unique = |reference| {
        if !out.contains(&reference) {
            out.push(reference);
        }
    };
    let parsed = gix_glob::Pattern::from_bytes_without_negation(pattern)
        .ok_or_else(|| message("Notes display references must not be empty").raise())?;
    if parsed
        .mode
        .intersects(gix_glob::pattern::Mode::ABSOLUTE | gix_glob::pattern::Mode::MUST_BE_DIR)
    {
        return Ok(());
    }
    if parsed.has_wildcard() {
        let platform = repo
            .references()
            .or_raise(|| message!("Could not iterate notes references matching {pattern}"))?;
        let references = platform
            .all()
            .or_raise(|| message!("Could not iterate notes references matching {pattern}"))?;
        for reference in references {
            let reference = reference
                .map_err(gix_error::Error::from_boxed)
                .or_raise(|| message!("Could not read notes reference matching {pattern}"))?;
            if parsed.matches(reference.name().as_bstr(), gix_glob::wildmatch::Mode::empty()) {
                push_unique(reference.inner.name);
            }
        }
    } else {
        push_unique(
            FullName::try_from(pattern)
                .or_raise(|| message!("Notes display reference {pattern} is not fully qualified"))?,
        );
    }
    Ok(())
}

fn expand_notes_ref(name: &PartialNameRef) -> Result<Cow<'_, FullNameRef>, gix_error::Exn<gix_ref::name::Error>> {
    let name = name.as_bstr();
    if name.starts_with_str("refs/notes/") {
        return Ok(Cow::Borrowed(name.try_into()?));
    }

    let mut name = name.to_owned();
    if name.starts_with_str("notes/") {
        name.insert_str(0, "refs/");
    } else {
        name.insert_str(0, "refs/notes/");
    }
    Ok(Cow::Owned(name.try_into()?))
}
