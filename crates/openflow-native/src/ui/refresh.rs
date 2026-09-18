//! Generation checks shared by pages that read slow services off the UI thread.
//! A hidden page does no work; an older request can never overwrite a newer
//! query or edits made while the read was outstanding.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A deferred credential read must not overwrite an edit, and an untouched
/// field must not save its initial empty placeholder before the read finishes.
#[derive(Default)]
pub struct DeferredField {
    revision: Cell<u64>,
    writable: Cell<bool>,
    dirty: Cell<bool>,
}

impl DeferredField {
    pub fn revision(&self) -> u64 {
        self.revision.get()
    }

    pub fn edited(&self) {
        self.revision.set(self.revision.get().wrapping_add(1));
        self.writable.set(true);
        self.dirty.set(true);
    }

    pub fn loaded(&self, revision: u64) -> bool {
        if revision != self.revision.get() || self.dirty.get() {
            return false;
        }
        self.writable.set(true);
        true
    }

    pub fn writable(&self) -> bool {
        self.writable.get()
    }

    pub fn committed(&self) {
        self.revision.set(self.revision.get().wrapping_add(1));
        self.dirty.set(false);
    }
}

#[derive(Default)]
pub struct RefreshGate {
    visible: AtomicBool,
    generation: AtomicU64,
}

impl RefreshGate {
    pub fn show(&self) {
        self.visible.store(true, Ordering::Release);
    }

    pub fn hide(&self) {
        self.visible.store(false, Ordering::Release);
        self.invalidate();
    }

    pub fn visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }

    pub fn invalidate(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn accepts(&self, generation: u64) -> bool {
        self.visible() && self.generation.load(Ordering::Acquire) == generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_search_cannot_replace_the_new_one() {
        let gate = RefreshGate::default();
        gate.show();
        let old = gate.invalidate();
        let latest = gate.invalidate();
        assert!(!gate.accepts(old));
        assert!(gate.accepts(latest));
    }

    #[test]
    fn hide_and_reopen_do_not_accept_the_previous_opening() {
        let gate = RefreshGate::default();
        gate.show();
        let previous = gate.invalidate();
        gate.hide();
        assert!(!gate.accepts(previous));
        gate.show();
        assert!(!gate.accepts(previous));
        assert!(gate.accepts(gate.invalidate()));
    }

    #[test]
    fn credential_reads_preserve_edits_and_never_save_a_loading_placeholder() {
        let key = DeferredField::default();
        let pending = key.revision();
        assert!(
            !key.writable(),
            "focusing a loading field must not clear a saved key"
        );
        key.edited();
        assert!(
            key.writable(),
            "a user can intentionally replace a key before loading"
        );
        assert!(
            !key.loaded(pending),
            "the old read must not replace the edit"
        );
        let during_edit = key.revision();
        assert!(
            !key.loaded(during_edit),
            "a refresh during editing must preserve unsaved text"
        );
        key.committed();
        assert!(
            !key.loaded(during_edit),
            "a pre-save read must not restore the old saved key"
        );
        assert!(key.loaded(key.revision()));
        let untouched = DeferredField::default();
        assert!(untouched.loaded(untouched.revision()));
        assert!(untouched.writable());
    }
}
