//! Documents' texts, replicated (see `docs/client-server.md`): the app
//! edits its own copy of each, in its buffer, and the host keeps another,
//! which the app keeps up to date by sending what changed, as edits the
//! host acks, by version. The host's copy is what saving writes, what a
//! session keeps of unsaved changes, and what workspace search reads of
//! a file open with unsaved changes.
//!
//! Whole texts go between them only on opening: from the host, which
//! keeps what it loaded as its copy (see [`Request::Load`]'s `doc`); or
//! to it, for a document that wasn't loaded, such as an untitled one
//! ([`Tell::Open`]). Reloading a file takes what changed, as editing does.
//!
//! The app's end of a copy is a document's [`Replica`]; the host's, a
//! connection's [`Copies`]: a client that takes over makes its own.
//!
//! [`Request::Load`]: crate::host::Request::Load

use std::collections::{HashMap, HashSet};

use opentui::EditBuffer;
use serde::{Deserialize, Serialize};

use crate::host::{Link, Tell, WeakLink};

const NO_COPY: &str = "The host has no copy of it.";

/// Tells a document's copy on the host from the others on the same link.
pub type DocId = u64;

/// A change to a text: the bytes `start..end` replaced with `text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

impl Edit {
    /// The edit that turns `old` into `new`, if they differ: what's
    /// between what they start and end with in common.
    pub fn between(old: &str, new: &str) -> Option<Edit> {
        let (start, old_end, new_end) = crate::syntax::changed(old, new)?;
        Some(Edit {
            start,
            end: old_end,
            text: new[start..new_end].to_string(),
        })
    }

    /// Makes it to `text`. Returns false, leaving `text` as it was, if it
    /// doesn't fit.
    pub fn apply(&self, text: &mut String) -> bool {
        let fits = self.start <= self.end
            && self.end <= text.len()
            && text.is_char_boundary(self.start)
            && text.is_char_boundary(self.end);
        if fits {
            text.replace_range(self.start..self.end, &self.text);
        }
        fits
    }
}

// --- the app's end ----------------------------------------------------------

/// A document's copy on the host, as the app last sent it. Dropping it
/// lets the host's go.
pub struct Replica {
    id: DocId,
    /// Not keeping it going: once the app's gone, so is its need of it.
    host: WeakLink,
    /// The text the host has, as of `version`.
    sent: String,
    /// The buffer's content epoch `sent` was taken at; `None` if it wasn't
    /// taken from the buffer, as what the host loaded isn't.
    epoch: Option<u64>,
    version: u64,
    /// The latest version the host said it has.
    acked: u64,
}

impl Replica {
    /// One the host's to make of the file it loads for it, as
    /// [`Request::Load`](crate::host::Request::Load)'s `doc`. It's of use
    /// once the host's said what it loaded (see [`Replica::loaded`]).
    pub fn loading(host: &Link) -> Replica {
        Replica {
            id: host.next_doc(),
            host: host.weak(),
            sent: String::new(),
            epoch: None,
            version: 0,
            acked: 0,
        }
    }

    /// Takes in what the host loaded, `text`, which is its copy.
    pub fn loaded(&mut self, text: String) {
        self.sent = text;
    }

    /// One the host makes of `text`, sent whole, the buffer's at content
    /// epoch `epoch`.
    pub fn open(host: &Link, text: String, epoch: Option<u64>) -> Replica {
        let id = host.next_doc();
        host.tell(Tell::Open {
            doc: id,
            text: text.clone(),
        });
        Replica {
            id,
            host: host.weak(),
            sent: text,
            epoch,
            version: 0,
            acked: 0,
        }
    }

    pub fn id(&self) -> DocId {
        self.id
    }

    /// The text the host has, as of [`Replica::version`].
    pub fn sent(&self) -> &str {
        &self.sent
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// The latest version the host said it has.
    #[cfg(test)]
    pub fn acked(&self) -> u64 {
        self.acked
    }

    /// Notes that the host has `version`.
    pub fn ack(&mut self, version: u64) {
        self.acked = self.acked.max(version);
    }

    /// Sends the host what `buffer` changed since it was last sent, if
    /// anything did, as the next version.
    pub fn sync(&mut self, buffer: &EditBuffer) {
        let epoch = buffer.content_epoch();
        if self.epoch == Some(epoch) {
            return;
        }
        let text = buffer.text();
        if let Some(edit) = Edit::between(&self.sent, &text) {
            self.version += 1;
            self.host.tell(Tell::Edit {
                doc: self.id,
                version: self.version,
                edit,
            });
        }
        self.sent = text;
        self.epoch = Some(epoch);
    }
}

impl Drop for Replica {
    fn drop(&mut self) {
        self.host.tell(Tell::Close(self.id));
    }
}

// --- the host's end ---------------------------------------------------------

/// Copies' texts, taken for a call (see [`Copies::take`]).
#[derive(Debug, Default)]
pub struct Texts(HashMap<DocId, Result<String, String>>);

impl Texts {
    /// The text of the copy `id`, or why there's none.
    pub fn get(&self, id: DocId) -> Result<&str, String> {
        match self.0.get(&id) {
            Some(Ok(text)) => Ok(text),
            Some(Err(err)) => Err(err.clone()),
            None => Err(NO_COPY.to_string()),
        }
    }

    /// The text of the copy `id`, or why there's none, taken out.
    pub fn remove(&mut self, id: DocId) -> Result<String, String> {
        self.0
            .remove(&id)
            .unwrap_or_else(|| Err(NO_COPY.to_string()))
    }
}

/// The documents' copies a connection's app keeps on the host, by id.
#[derive(Default)]
pub struct Copies {
    texts: HashMap<DocId, Copy>,
    /// Those closed before they were loaded, not to be kept once they are.
    closed: HashSet<DocId>,
}

struct Copy {
    text: String,
    version: u64,
}

impl Copies {
    /// Keeps `text`, which the host loaded for the copy `id`, unless that
    /// was closed meanwhile; `None` if it couldn't load it.
    pub fn loaded(&mut self, id: DocId, text: Option<&str>) {
        if self.closed.remove(&id) {
            return;
        }
        if let Some(text) = text {
            self.open(id, text.to_string());
        }
    }

    /// Keeps `text` as the copy `id`, at version 0.
    pub fn open(&mut self, id: DocId, text: String) {
        self.texts.insert(id, Copy { text, version: 0 });
    }

    /// Makes `edit` to the copy `id`, as `version`. Returns whether it
    /// did: a copy an edit doesn't fit, or comes out of order to, isn't
    /// the app's any more, and goes.
    pub fn edit(&mut self, id: DocId, version: u64, edit: &Edit) -> bool {
        let Some(copy) = self.texts.get_mut(&id) else {
            return false;
        };
        if copy.version + 1 == version && edit.apply(&mut copy.text) {
            copy.version = version;
            return true;
        }
        eprintln!(
            "cue: version {version} of document {id} doesn't follow from {}",
            copy.version
        );
        self.texts.remove(&id);
        false
    }

    /// Lets the copy `id` go.
    pub fn close(&mut self, id: DocId) {
        if self.texts.remove(&id).is_none() {
            // Still loading.
            self.closed.insert(id);
        }
    }

    /// The texts of the copies `wanted`, each as of a version if one's
    /// given, as they are now, for a call that reads them: taken as it
    /// comes, not as it's answered, which edits that come after it may
    /// have changed.
    pub fn take(&self, wanted: impl IntoIterator<Item = (DocId, Option<u64>)>) -> Texts {
        let texts = wanted.into_iter().map(|(id, version)| {
            let text = self.text(id, version).map(str::to_string);
            (id, text)
        });
        Texts(texts.collect())
    }

    /// The text of the copy `id`, if there is one; and if `version` is
    /// given, as of it.
    pub fn text(&self, id: DocId, version: Option<u64>) -> Result<&str, String> {
        let copy = self.texts.get(&id).ok_or(NO_COPY)?;
        match version {
            Some(version) if version != copy.version => Err(format!(
                "The host's copy is at version {}, not {version}.",
                copy.version
            )),
            _ => Ok(&copy.text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_turns_one_text_into_the_other() {
        let cases = [
            ("hello world", "hello brave world"),
            ("abc", ""),
            ("", "abc"),
            ("aaa", "aa"),
            ("héllo", "hello"),
            ("one\ntwo\n", "one\n2\n"),
        ];
        for (old, new) in cases {
            let edit = Edit::between(old, new).unwrap();
            let mut text = old.to_string();
            assert!(edit.apply(&mut text));
            assert_eq!(text, new, "{old:?} to {new:?}");
        }
        assert_eq!(Edit::between("same", "same"), None);
    }

    #[test]
    fn copies_take_edits_in_order_only() {
        let mut copies = Copies::default();
        copies.open(1, "hello".to_string());
        let edit = Edit {
            start: 5,
            end: 5,
            text: "!".to_string(),
        };
        assert!(copies.edit(1, 1, &edit));
        assert_eq!(copies.text(1, Some(1)), Ok("hello!"));
        assert!(copies.text(1, Some(0)).is_err());
        // One skipped: the copy's no good.
        assert!(!copies.edit(1, 3, &edit));
        assert!(copies.text(1, None).is_err());
    }

    #[test]
    fn a_copy_closed_while_loading_isnt_kept() {
        let mut copies = Copies::default();
        copies.close(2);
        copies.loaded(2, Some("text"));
        assert!(copies.text(2, None).is_err());
        // Loaded again, it is.
        copies.loaded(2, Some("text"));
        assert_eq!(copies.text(2, None), Ok("text"));
    }
}
