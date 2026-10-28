use super::*;

impl HeldDrafts {
    pub(crate) fn change_access(
        cell: &mut Option<editor::EditState>,
        text_box: &mut Option<text_box::EditState>,
        mode: &mut gridcraft_engine::Mode,
        held: &mut Option<Self>,
        read_only: bool,
    ) {
        if read_only {
            if held.is_none() {
                *held = Some(Self { cell: cell.take(), text_box: text_box.take(), mode: *mode });
                *mode = gridcraft_engine::Mode::Ready;
            }
        } else if let Some(drafts) = held.take() {
            *cell = drafts.cell;
            *text_box = drafts.text_box;
            *mode = drafts.mode;
        }
    }

    pub(crate) fn pending(&self) -> bool {
        self.cell.is_some() || self.text_box.is_some()
    }

    pub(crate) fn modified(&self) -> bool {
        self.cell.as_ref().is_some_and(|edit| edit.original.as_ref().is_none_or(|original| original != &edit.text))
            || self.text_box.as_ref().is_some_and(|edit| edit.is_modified())
    }
}

impl SheetApp {
    pub fn is_read_only(&self) -> bool {
        self.session.is_read_only()
    }

    /// Apply ephemeral access without committing or discarding any native draft.
    /// Existing per-window storage retains parked buffers in the same way.
    pub fn set_read_only(&mut self, value: bool) {
        if !value && self.held_drafts.as_ref().is_some_and(|held| held.pending()) && (self.editor.is_some() || self.text_box_editor.is_some()) {
            return; // Retain both scopes and remain fail-closed if an embedder violated the invariant.
        }
        if !value && self.parked.values().any(|window| !window.can_resume_drafts()) {
            return;
        }
        HeldDrafts::change_access(&mut self.editor, &mut self.text_box_editor, &mut self.session.mode, &mut self.held_drafts, value);
        for window in self.parked.values_mut() {
            window.set_draft_access(value);
        }
        if value {
            self.grid.drag = grid::Drag::None;
            self.grid.list_picker = None;
            self.grid.filter_menu = None;
            self.grid.fill_menu = None;
            self.session.draw_tool.clear();
            self.session.format_painter = None;
            self.cut_text = None;
        }
        self.session.set_read_only(value);
    }

    pub fn cell_draft(&self) -> Option<&editor::EditState> {
        self.editor.as_ref().or_else(|| self.held_drafts.as_ref().and_then(|held| held.cell.as_ref()))
    }

    pub fn text_box_draft(&self) -> Option<&text_box::EditState> {
        self.text_box_editor.as_ref().or_else(|| self.held_drafts.as_ref().and_then(|held| held.text_box.as_ref()))
    }

    pub fn has_pending_drafts(&self) -> bool {
        self.cell_draft().is_some() || self.text_box_draft().is_some() || self.parked.values().any(|window| window.has_pending_drafts())
    }

    pub fn has_unsaved_drafts(&self) -> bool {
        self.cell_draft().is_some_and(|edit| edit.original.as_ref().is_none_or(|original| original != &edit.text))
            || self.text_box_draft().is_some_and(|edit| edit.is_modified())
            || self.parked.values().any(|window| window.has_unsaved_drafts())
    }

    /// The embedding host may load a clean workbook while viewing access is active.
    /// It cannot use import to settle or replace unsaved native state.
    pub fn import_host_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<Json, String> {
        if self.has_pending_drafts() || self.session.documents().iter().any(|doc| doc.is_dirty()) {
            return Err("Save or discard unsaved work and finish native drafts before opening another workbook.".into());
        }
        let result = self.session.import_bytes(name, bytes).map_err(|error| error.to_string());
        self.follow_active_window();
        self.after_engine();
        result
    }
}

#[cfg(test)]
mod tests {
    use crate::{Services, SheetApp};
    use gridcraft_engine::{Mode, Session};
    use serde_json::json;
    use std::sync::Arc;

    fn app() -> SheetApp {
        let mut session = Session::new();
        session.new_workbook();
        let mut app = SheetApp::new(session, Services::default());
        app.run("cell.set", json!({"cell":"A1","input":"21"})).unwrap();
        let doc = app.session.active_mut().unwrap();
        doc.saved = doc.wb.clone();
        app
    }

    #[test]
    fn revoke_holds_the_exact_cell_draft_without_committing_and_viewing_still_works() {
        let mut app = app();
        let before = app.session.active().unwrap().wb.clone();
        app.begin_edit(Some("pending cell".into()), false);
        app.editor.as_mut().unwrap().caret = 4;
        let text = app.editor.as_ref().unwrap().text.as_ptr();
        app.set_read_only(true);
        assert!(app.is_read_only());
        assert!(app.editor.is_none());
        assert_eq!(app.cell_draft().unwrap().text.as_ptr(), text);
        assert_eq!(app.cell_draft().unwrap().caret, 4);
        assert_eq!(app.session.mode, Mode::Ready);
        assert!(app.run("home.bold", json!({"range":"A1","on":true})).is_err());
        assert!(app.run("file.saveBytes", json!({"format":"json"})).is_err(), "export must not omit a held draft");
        app.begin_edit(Some("forbidden".into()), false);
        app.run("selection.set", json!({"cell":"A1"})).unwrap();
        assert_eq!(app.run("edit.copy", json!({})).unwrap()["text"], "21\n");
        app.run("edit.find", json!({"what":"21"})).unwrap();
        assert_eq!(app.cell_draft().unwrap().text, "pending cell");
        assert!(Arc::ptr_eq(&before, &app.session.active().unwrap().wb));
        app.set_read_only(false);
        assert_eq!(app.editor.as_ref().unwrap().text.as_ptr(), text);
        assert_eq!(app.editor.as_ref().unwrap().caret, 4);
        assert!(app.commit_edit(0, 0, false, false));
        assert_eq!(app.run("cell.get", json!({"cell":"A1"})).unwrap()["value"], "pending cell");
    }

    #[test]
    fn revoke_holds_the_exact_text_box_draft_and_restores_it_only_after_access_changes() {
        let mut app = app();
        let id = app.run("insert.textBox", json!({"at":"D8","text":"original"})).unwrap()["shape"].as_u64().unwrap() as u32;
        let doc = app.session.active_mut().unwrap();
        doc.saved = doc.wb.clone();
        let before = doc.wb.clone();
        app.begin_text_box_edit(id);
        app.text_box_editor.as_mut().unwrap().text = "pending text box".into();
        let text = app.text_box_editor.as_ref().unwrap().text.as_ptr();
        app.set_read_only(true);
        assert!(app.text_box_editor.is_none());
        assert_eq!(app.text_box_draft().unwrap().text.as_ptr(), text);
        assert!(app.run("shape.setText", json!({"id":id,"text":"forbidden"})).is_err());
        assert!(!app.begin_text_box_edit(id));
        app.open_dialog("find", json!({}));
        assert!(app.dialog.is_some(), "viewing-mode Find must not commit or be blocked by a held text-box draft");
        assert!(app.message.is_none());
        assert!(Arc::ptr_eq(&before, &app.session.active().unwrap().wb));
        app.set_read_only(false);
        assert_eq!(app.text_box_editor.as_ref().unwrap().text.as_ptr(), text);
        app.commit_text_box_edit().unwrap();
        assert_eq!(app.session.active().unwrap().wb.active().unwrap().shapes.iter().find(|shape| shape.id == id).unwrap().text, "pending text box");
    }

    #[test]
    fn trusted_host_import_works_under_guard_but_refuses_held_drafts_before_any_flush() {
        let mut source = Session::new();
        source.new_workbook();
        source.execute("cell.set", json!({"cell":"A1","input":"imported"})).unwrap();
        let encoded = source.execute("file.saveBytes", json!({"format":"json"})).unwrap();
        let bytes = gridcraft_engine::io::base64_decode(encoded["base64"].as_str().unwrap()).unwrap();
        let mut app = app();
        app.set_read_only(true);
        app.import_host_bytes("owned.json", &bytes).unwrap();
        assert!(app.is_read_only());
        assert_eq!(app.run("cell.get", json!({"cell":"A1"})).unwrap()["value"], "imported");
        app.set_read_only(false);
        app.begin_edit(Some("retained".into()), false);
        app.set_read_only(true);
        let before = app.session.active().unwrap().wb.clone();
        assert!(app.import_host_bytes("owned.json", &bytes).is_err());
        assert_eq!(app.cell_draft().unwrap().text, "retained");
        assert!(Arc::ptr_eq(&before, &app.session.active().unwrap().wb));
    }
    #[test]
    fn parked_window_draft_cannot_resurrect_an_edit_widget_during_viewing_access() {
        let mut app = app();
        let first = app.session.active_window().unwrap();
        app.run("view.newWindow", json!({})).unwrap();
        let second = app.session.active_window().unwrap();
        app.enter_window(first);
        app.begin_edit(Some("parked draft".into()), false);
        let text = app.editor.as_ref().unwrap().text.as_ptr();
        app.enter_window(second);
        assert!(app.editor.is_none());
        app.set_read_only(true);
        assert!(app.has_unsaved_drafts());
        app.enter_window(first);
        assert!(app.editor.is_none());
        assert_eq!(app.cell_draft().unwrap().text.as_ptr(), text);
        app.run("selection.set", json!({"cell":"B2"})).unwrap();
        app.set_read_only(false);
        assert_eq!(app.editor.as_ref().unwrap().text.as_ptr(), text);
    }
}
