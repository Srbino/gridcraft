//! Host checkpoints preserve the active native edit while encoding its current text.

use super::*;
use gridcraft_engine::{GroupToken, core::CellRef, model::Workbook};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeSnapshotOwner {
    Cell(u64, Option<u64>, usize, CellRef),
    TextBox(u64, usize, u32),
}

impl NativeSnapshotOwner {
    pub(crate) fn cell(edit: &editor::EditState) -> Self {
        Self::Cell(edit.doc, edit.window, edit.sheet, edit.cell)
    }
}

pub(crate) struct NativeSnapshotGroup {
    owner: NativeSnapshotOwner,
    token: GroupToken,
    original: Arc<Workbook>,
    original_sheet: usize,
}

impl SheetApp {
    /// Encode an embedding host's native checkpoint without ending text input.
    /// Ordinary File Save/Export keeps its existing commit-and-close behavior.
    pub fn native_snapshot_bytes(&mut self) -> Result<Json, String> {
        if self.is_read_only() && self.has_pending_drafts() {
            return Err("The workbook is read-only; native drafts are retained.".into());
        }
        let owner =
            self.editor.as_ref().map(NativeSnapshotOwner::cell).or_else(|| self.text_box_editor.as_ref().map(text_box::EditState::snapshot_owner));
        let Some(owner) = owner else {
            return self.run("file.saveBytes", json!({"format":"json"}));
        };
        if self.draft_elsewhere() {
            return Err("Finish the other window's edit before taking a native checkpoint.".into());
        }
        if self.native_snapshot_group.as_ref().is_some_and(|group| group.owner != owner) {
            return Err("Another native editing checkpoint is still open.".into());
        }
        let cell = self.editor.clone();
        let text_box = self.text_box_editor.clone();
        let mode = self.session.mode;
        let message = self.message.clone();
        let toast = self.toast.clone();
        let view = self.session.active().map(|doc| (doc.wb.active_sheet(), doc.selection.clone()));
        let before = self.session.active().map(|doc| doc.wb.clone());
        let started_group = self.native_snapshot_group.is_none();
        let group = match self.native_snapshot_group.take() {
            Some(group) => group,
            None => {
                let original = self.session.active().ok_or("No workbook is open.")?.wb.clone();
                let original_sheet = original.active_sheet();
                NativeSnapshotGroup { owner, token: self.session.group_begin("Edit").map_err(|error| error.to_string())?, original, original_sheet }
            }
        };
        // Completion is a suggestion until the user accepts it. A background
        // checkpoint stores only the actual typed text, then restores the hint.
        if let Some(edit) = self.editor.as_mut() {
            edit.completion = None;
        }
        let committed = self.settle_drafts();
        // Point mode can temporarily visit another sheet. Capture the view the
        // user was actually looking at, without moving their caret/selection.
        if let (Some((sheet, selection)), Some(doc)) = (view, self.session.active_mut()) {
            doc.wb.set_active_sheet(sheet);
            doc.selection = selection;
        }
        let result = committed.as_ref().map_err(Clone::clone).and_then(|_| {
            self.store_view_state();
            self.session.run("file.saveBytes", json!({"format":"json"})).map_err(|error| error.to_string())
        });
        self.editor = cell;
        self.text_box_editor = text_box;
        self.session.mode = mode;
        // A background invalid-prefix refusal must not open a modal and steal
        // the remaining keyboard input. Its error is returned to the host.
        self.message = message;
        self.toast = toast;
        if committed.is_err() && started_group && before.as_ref().zip(self.session.active()).is_some_and(|(before, doc)| Arc::ptr_eq(before, &doc.wb))
        {
            // A first invalid prefix changed no workbook. Do not leave an
            // empty undo group open merely because the host attempted capture.
            self.session.group_end(group.token).map_err(|error| error.to_string())?;
        } else {
            self.native_snapshot_group = Some(group);
        }
        if committed.is_ok() {
            if let Some(edit) = self.editor.as_mut() {
                edit.original = Some(edit.text.clone());
            }
            if let Some(edit) = self.text_box_editor.as_mut() {
                edit.checkpoint_text();
            }
        }
        result
    }

    pub(crate) fn finish_native_snapshot_owner(&mut self, owner: NativeSnapshotOwner, cancel: bool) -> Result<(), String> {
        if !self.native_snapshot_group.as_ref().is_some_and(|group| group.owner == owner) {
            return Ok(());
        }
        if self.is_read_only() {
            return Err("The workbook is read-only; its native editing checkpoint is retained.".into());
        }
        if !cancel
            && let NativeSnapshotOwner::Cell(doc, _, sheet, _) = owner
            && self.session.active().is_some_and(|active| active.uid == doc && active.wb.active_sheet() != sheet)
        {
            self.session.run("sheet.activate", json!({"sheet":sheet})).map_err(|error| error.to_string())?;
        }
        // Keep the association if reactivating a deleted owner is refused;
        // Escape still needs this token to roll back the editing gesture.
        let group = self.native_snapshot_group.take().ok_or("Native checkpoint owner is unavailable.")?;
        // ActiveSheet is serialized interior state. The engine group holds the
        // original Arc, so restore its captured index before publishing history
        // (without changing the current workbook on a view-only no-op).
        let same = self.session.documents().iter().find(|doc| doc.uid == group.token.doc()).is_some_and(|doc| Arc::ptr_eq(&doc.wb, &group.original));
        if cancel || !same {
            group.original.set_active_sheet(group.original_sheet);
        }
        if cancel {
            self.session.group_rollback(group.token).map_err(|error| error.to_string())?;
        } else {
            self.session.group_end(group.token).map_err(|error| error.to_string())?;
        }
        self.after_engine();
        Ok(())
    }
}
