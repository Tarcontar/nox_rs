//! Undo and redo.
//!
//! Every edit goes through [`Document::edit`], which keeps a snapshot of the
//! timeline from before the edit. Snapshots are cheap compared to media
//! (a timeline with thousands of clips is a few hundred kilobytes) and make
//! undo exact by construction, with no inverse operations to get wrong.

use crate::timeline::Timeline;

/// How many edits can be undone.
const UNDO_LIMIT: usize = 500;

struct Step {
    label: String,
    timeline: Timeline,
    revision: u64,
    /// Set by `edit_merging`: later edits with the same key join this step.
    merge_key: Option<String>,
}

/// A timeline with its undo history and saved state.
pub struct Document {
    timeline: Timeline,
    /// Identifies the current state; every edit gets a new one.
    revision: u64,
    next_revision: u64,
    saved_revision: Option<u64>,
    undo: Vec<Step>,
    redo: Vec<Step>,
}

impl Document {
    /// A document for a timeline that is considered saved (e.g. just
    /// loaded); see [`Document::mark_unsaved`] for new ones.
    pub fn new(timeline: Timeline) -> Self {
        Self {
            timeline,
            revision: 0,
            next_revision: 1,
            saved_revision: Some(0),
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// Applies `edit` as one undoable step called `label`. If it fails, the
    /// timeline is left unchanged. Edits that change nothing are not
    /// recorded.
    pub fn edit<T, E>(
        &mut self,
        label: impl Into<String>,
        edit: impl FnOnce(&mut Timeline) -> Result<T, E>,
    ) -> Result<T, E> {
        self.record(label.into(), None, edit)
    }

    /// Like `edit`, but joins the previous step if that was made with the
    /// same `merge_key` and nothing came in between: dragging a slider makes
    /// one undo step, not one per value. If the merged edits end where they
    /// started, the step is dropped.
    pub fn edit_merging<T, E>(
        &mut self,
        label: impl Into<String>,
        merge_key: &str,
        edit: impl FnOnce(&mut Timeline) -> Result<T, E>,
    ) -> Result<T, E> {
        let mergeable = self.redo.is_empty()
            && self
                .undo
                .last()
                .is_some_and(|s| s.merge_key.as_deref() == Some(merge_key));
        if !mergeable {
            return self.record(label.into(), Some(merge_key.to_owned()), edit);
        }
        let before = self.timeline.clone();
        match edit(&mut self.timeline) {
            Ok(value) => {
                let step = self.undo.last().expect("mergeable step");
                if self.timeline == step.timeline {
                    // Back where the step started: nothing to undo.
                    let step = self.undo.pop().expect("mergeable step");
                    self.revision = step.revision;
                } else if self.timeline != before {
                    self.revision = self.next_revision;
                    self.next_revision += 1;
                }
                Ok(value)
            }
            Err(err) => {
                self.timeline = before;
                Err(err)
            }
        }
    }

    fn record<T, E>(
        &mut self,
        label: String,
        merge_key: Option<String>,
        edit: impl FnOnce(&mut Timeline) -> Result<T, E>,
    ) -> Result<T, E> {
        let before = self.timeline.clone();
        match edit(&mut self.timeline) {
            Ok(value) => {
                if self.timeline != before {
                    self.undo.push(Step {
                        label,
                        timeline: before,
                        revision: self.revision,
                        merge_key,
                    });
                    if self.undo.len() > UNDO_LIMIT {
                        self.undo.remove(0);
                    }
                    self.redo.clear();
                    self.revision = self.next_revision;
                    self.next_revision += 1;
                }
                Ok(value)
            }
            Err(err) => {
                self.timeline = before;
                Err(err)
            }
        }
    }

    /// Reverts the last edit and returns its label.
    pub fn undo(&mut self) -> Option<String> {
        let step = self.undo.pop()?;
        let label = step.label.clone();
        let current = self.swap(step);
        self.redo.push(current);
        Some(label)
    }

    /// Reapplies the last undone edit and returns its label.
    pub fn redo(&mut self) -> Option<String> {
        let step = self.redo.pop()?;
        let label = step.label.clone();
        let current = self.swap(step);
        self.undo.push(current);
        Some(label)
    }

    /// Makes `step` current and returns the current state as a step.
    fn swap(&mut self, step: Step) -> Step {
        let current = Step {
            label: step.label,
            timeline: std::mem::replace(&mut self.timeline, step.timeline),
            revision: self.revision,
            // Undone and redone steps don't merge with new edits.
            merge_key: None,
        };
        self.revision = step.revision;
        current
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|s| s.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|s| s.label.as_str())
    }

    /// Whether the timeline differs from the last saved state.
    pub fn is_modified(&self) -> bool {
        self.saved_revision != Some(self.revision)
    }

    pub fn mark_saved(&mut self) {
        self.saved_revision = Some(self.revision);
    }

    /// Marks the document as never saved.
    pub fn mark_unsaved(&mut self) {
        self.saved_revision = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Rational;

    fn rename(doc: &mut Document, name: &str) {
        doc.edit("Rename", |t| {
            t.name = name.into();
            Ok::<_, ()>(())
        })
        .unwrap();
    }

    #[test]
    fn undo_redo_and_modified_state() {
        let mut doc = Document::new(Timeline::new("a", Rational::new(25, 1)));
        assert!(!doc.is_modified());
        rename(&mut doc, "b");
        rename(&mut doc, "c");
        assert!(doc.is_modified());
        assert_eq!(doc.undo(), Some("Rename".into()));
        assert_eq!(doc.timeline().name, "b");
        doc.mark_saved();
        assert_eq!(doc.undo(), Some("Rename".into()));
        assert_eq!(doc.timeline().name, "a");
        assert!(doc.is_modified());
        assert_eq!(doc.undo(), None);
        assert_eq!(doc.redo(), Some("Rename".into()));
        assert!(!doc.is_modified());
        // A new edit drops the redo history.
        rename(&mut doc, "d");
        assert_eq!(doc.redo(), None);
        assert_eq!(doc.undo_label(), Some("Rename"));
    }

    fn rename_merging(doc: &mut Document, name: &str, key: &str) {
        doc.edit_merging("Rename", key, |t| {
            t.name = name.into();
            Ok::<_, ()>(())
        })
        .unwrap();
    }

    #[test]
    fn merging_edits_make_one_step() {
        let mut doc = Document::new(Timeline::new("a", Rational::new(25, 1)));
        for name in ["b", "c", "d"] {
            rename_merging(&mut doc, name, "name");
        }
        assert!(doc.is_modified());
        assert_eq!(doc.undo(), Some("Rename".into()));
        assert_eq!(doc.timeline().name, "a");
        assert_eq!(doc.undo(), None);

        // Another key, a plain edit or an undo in between starts a new step.
        rename_merging(&mut doc, "b", "name");
        rename_merging(&mut doc, "c", "other");
        rename(&mut doc, "d");
        rename_merging(&mut doc, "e", "other");
        assert_eq!(doc.undo(), Some("Rename".into()));
        assert_eq!(doc.timeline().name, "d");
        rename_merging(&mut doc, "f", "other");
        assert_eq!(doc.undo(), Some("Rename".into()));
        assert_eq!(doc.timeline().name, "d");
    }

    #[test]
    fn merging_back_to_the_start_drops_the_step() {
        let mut doc = Document::new(Timeline::new("a", Rational::new(25, 1)));
        rename_merging(&mut doc, "b", "name");
        rename_merging(&mut doc, "a", "name");
        assert_eq!(doc.undo_label(), None);
        assert!(!doc.is_modified());
    }

    #[test]
    fn failed_and_empty_edits_are_not_recorded() {
        let mut doc = Document::new(Timeline::new("a", Rational::new(25, 1)));
        let result = doc.edit("Fail", |t| {
            t.name = "broken".into();
            Err::<(), _>("nope")
        });
        assert_eq!(result, Err("nope"));
        assert_eq!(doc.timeline().name, "a");
        rename(&mut doc, "a");
        assert_eq!(doc.undo_label(), None);
        assert!(!doc.is_modified());
    }
}
