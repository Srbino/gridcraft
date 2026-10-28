//! Bounded host revision pulses include pending native editor drafts.

use std::sync::Arc;

use gridcraft_engine::{core::CellRef, model::Workbook};
use gridcraft_ui_egui::SheetApp;
use sha2::{Digest, Sha256};

#[derive(PartialEq)]
struct Fingerprint {
    active: Option<(u64, u64, usize)>,
    cell: Option<(u64, usize, CellRef, [u8; 32])>,
    text_box: Option<(u32, [u8; 32])>,
}

fn changed_cell(app: &SheetApp) -> bool {
    app.cell_draft().is_some_and(|edit| edit.original.as_ref().is_none_or(|original| original != &edit.text))
}

/// Draft buffers are native editor state, not yet part of `DocState::wb`.
pub fn dirty(app: &SheetApp) -> bool {
    app.session.documents().iter().any(|doc| doc.is_dirty()) || app.has_unsaved_drafts()
}

/// Bounded activity state: one shared workbook identity and fixed-size draft fingerprints.
/// Holding the Arc forces copy-on-write for serialized view changes too; it also
/// prevents pointer reuse from making a different workbook look unchanged.
/// The counter is an opaque equality token, not a JavaScript number or a lease.
#[derive(Default)]
pub struct Revisions {
    previous: Option<Fingerprint>,
    workbook: Option<Arc<Workbook>>,
    serial: u64,
}

impl Revisions {
    pub fn observe(&mut self, app: &SheetApp) -> bool {
        let fingerprint = Fingerprint {
            // ActiveSheet is serialized AtomicUsize interior state: snapshot
            // its value, since Arc identity alone cannot observe a sheet switch.
            active: app.session.active().map(|doc| (doc.uid, doc.revision, doc.wb.active_sheet())),
            cell: app
                .editor
                .as_ref()
                .filter(|_| changed_cell(app))
                .map(|edit| (edit.doc, edit.sheet, edit.cell, Sha256::digest(edit.text.as_bytes()).into())),
            text_box: app
                .text_box_editor
                .as_ref()
                .filter(|edit| edit.is_modified())
                .map(|edit| (edit.id, Sha256::digest(edit.text.as_bytes()).into())),
        };
        let workbook = app.session.active().map(|doc| &doc.wb);
        let same_workbook = match (&self.workbook, workbook) {
            (Some(previous), Some(current)) => Arc::ptr_eq(previous, current),
            (None, None) => true,
            _ => false,
        };
        if self.previous.as_ref() == Some(&fingerprint) && same_workbook {
            return false;
        }
        self.previous = Some(fingerprint);
        self.workbook = workbook.cloned();
        self.serial = self.serial.wrapping_add(1);
        true
    }

    pub fn sequence(&self) -> u64 {
        self.serial
    }

    pub fn revision(&self) -> String {
        self.sequence().to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gridcraft_engine::Session;
    use gridcraft_ui_egui::{Services, SheetApp};
    use serde_json::json;

    fn app() -> SheetApp {
        let mut session = Session::new();
        session.new_workbook();
        let mut app = SheetApp::new(session, Services::default());
        app.run("cell.set", json!({"cell":"A1", "input":"first"})).unwrap();
        app
    }

    #[test]
    fn pending_cell_survives_older_ack_and_every_text_edit_pulses_without_enter() {
        let mut app = app();
        let mut revisions = super::Revisions::default();
        revisions.observe(&app);
        let mut saves = crate::managed_save::ManagedSaves::default();
        let first = crate::native_document::export(&mut app).unwrap();
        let first_revision = revisions.revision();
        saves.remember("first", &first, &app, &first_revision).unwrap();
        let engine_revision = app.session.active().unwrap().revision;
        app.begin_edit(Some("second".into()), false);
        assert!(revisions.observe(&app));
        let pending = revisions.revision();
        assert_ne!(first_revision, pending);
        assert!(!revisions.observe(&app));
        app.editor.as_mut().unwrap().text.push('!');
        assert!(revisions.observe(&app));
        assert_ne!(pending, revisions.revision());
        assert_eq!(engine_revision, app.session.active().unwrap().revision);
        assert!(saves.acknowledge(&mut app, "first", &first.document, &first_revision));
        assert!(!app.session.active().unwrap().is_dirty());
        assert!(super::dirty(&app), "a pending cell is unsaved even after the older workbook ACK");
        let second = crate::native_document::export(&mut app).unwrap();
        assert!(revisions.observe(&app), "export flushes the pending native cell before its revision is returned");
        let second_revision = revisions.revision();
        saves.remember("second", &second, &app, &second_revision).unwrap();
        assert!(saves.acknowledge(&mut app, "second", &second.document, &second_revision));
        assert!(!super::dirty(&app));
    }

    #[test]
    fn unchanged_editor_is_clean_and_invalid_draft_stays_dirty_until_cancelled() {
        let mut app = app();
        let first = crate::native_document::export(&mut app).unwrap();
        let mut saves = crate::managed_save::ManagedSaves::default();
        saves.remember("first", &first, &app, &first.revision).unwrap();
        assert!(saves.acknowledge(&mut app, "first", &first.document, &first.revision));
        app.begin_edit(None, false);
        assert!(!super::dirty(&app));
        app.editor.as_mut().unwrap().text = "=1+".into();
        assert!(super::dirty(&app));
        assert!(crate::native_document::export(&mut app).is_err());
        assert!(super::dirty(&app));
        app.cancel_edit();
        assert!(!super::dirty(&app));
    }

    #[test]
    fn pending_text_box_survives_ack_and_flushes_through_the_native_engine() {
        let mut app = app();
        let id = app.run("insert.textBox", json!({"at":"B2","text":"original"})).unwrap()["shape"].as_u64().unwrap() as u32;
        let first = crate::native_document::export(&mut app).unwrap();
        let mut saves = crate::managed_save::ManagedSaves::default();
        saves.remember("first", &first, &app, &first.revision).unwrap();
        assert!(saves.acknowledge(&mut app, "first", &first.document, &first.revision));
        assert!(app.begin_text_box_edit(id));
        assert!(!super::dirty(&app));
        let mut revisions = super::Revisions::default();
        revisions.observe(&app);
        let before = revisions.revision();
        saves.remember("older", &first, &app, &before).unwrap();
        app.text_box_editor.as_mut().unwrap().text = "pending text".into();
        assert!(revisions.observe(&app));
        assert_ne!(before, revisions.revision());
        assert!(saves.acknowledge(&mut app, "older", &first.document, &before));
        assert!(!app.session.active().unwrap().is_dirty());
        assert!(super::dirty(&app));
        let snapshot = crate::native_document::export(&mut app).unwrap();
        let (wb, _) = gridcraft_engine::io::open_bytes("snapshot.json", &snapshot.bytes).unwrap();
        assert_eq!(wb.active().unwrap().shapes.iter().find(|shape| shape.id == id).unwrap().text, "pending text");
        assert_eq!(app.text_box_editor.as_ref().unwrap().text, "pending text", "a native checkpoint keeps the ongoing text-box edit");
    }

    #[test]
    fn serialized_view_changes_get_a_fresh_wire_revision_but_idle_navigation_does_not() {
        let mut app = app();
        let mut revisions = super::Revisions::default();
        let first = crate::native_document::export(&mut app).unwrap();
        revisions.observe(&app);
        let before = revisions.revision();
        let noop = crate::native_document::export(&mut app).unwrap();
        assert!(!revisions.observe(&app));
        assert_eq!(first.bytes, noop.bytes);
        app.session.run("selection.set", json!({"cell":"A2"})).unwrap();
        assert!(!revisions.observe(&app), "view-only navigation does not create an edit pulse");
        let second = crate::native_document::export(&mut app).unwrap();
        assert_ne!(first.bytes, second.bytes, "actual native view_active changed");
        assert!(revisions.observe(&app), "different serialized bytes need a new wire identity");
        assert_ne!(before, revisions.revision());
        let stable = crate::native_document::export(&mut app).unwrap();
        assert_eq!(second.bytes, stable.bytes);
        assert!(!revisions.observe(&app));
    }

    #[test]
    fn all_serialized_view_fields_refresh_identity_and_noop_exports_stay_stable() {
        let mut app = app();
        app.run("home.insertSheet", json!({"name":"Second"})).unwrap();
        let mut revisions = super::Revisions::default();
        let first = crate::native_document::export(&mut app).unwrap();
        revisions.observe(&app);
        let mut previous = (first.bytes, revisions.revision());
        for (command, arguments) in
            [("sheet.activate", json!({"sheet":0})), ("view.zoom", json!({"percent":150})), ("view.gridlines", json!({"on":false}))]
        {
            app.run(command, arguments).unwrap();
            let exported = crate::native_document::export(&mut app).unwrap();
            revisions.observe(&app);
            assert_ne!(previous.0, exported.bytes, "{command} changes actual native bytes");
            assert_ne!(previous.1, revisions.revision(), "{command} needs fresh identity");
            previous = (exported.bytes, revisions.revision());
        }
        app.view_mut().unwrap().scroll.y = 160.0;
        assert!(!revisions.observe(&app), "an unmaterialized scroll is not a content edit pulse");
        let scrolled = crate::native_document::export(&mut app).unwrap();
        assert_ne!(previous.0, scrolled.bytes);
        assert!(revisions.observe(&app), "store_view materializes native view_top_left");
        let stable = crate::native_document::export(&mut app).unwrap();
        assert_eq!(scrolled.bytes, stable.bytes);
        assert!(!revisions.observe(&app));
    }

    #[test]
    fn active_sheet_atomic_changes_identity_even_when_workbook_arc_and_content_revision_match() {
        let mut app = app();
        app.run("home.insertSheet", json!({"name":"Second"})).unwrap();
        let mut revisions = super::Revisions::default();
        revisions.observe(&app);
        let before = revisions.revision();
        let workbook = app.session.active().unwrap().wb.clone();
        let content_revision = app.session.active().unwrap().revision;
        app.session.run("sheet.activate", json!({"sheet":0})).unwrap();
        assert!(Arc::ptr_eq(&workbook, &app.session.active().unwrap().wb));
        assert_eq!(content_revision, app.session.active().unwrap().revision);
        assert!(revisions.observe(&app));
        assert_ne!(before, revisions.revision());
        assert!(!revisions.observe(&app));
    }

    #[test]
    fn older_ack_after_sheet_switch_cannot_clean_a_later_cell_edit_or_held_readonly_draft() {
        let mut app = app();
        app.run("home.insertSheet", json!({"name":"Second"})).unwrap();
        let mut revisions = super::Revisions::default();
        let original = crate::native_document::export(&mut app).unwrap();
        revisions.observe(&app);
        let mut saves = crate::managed_save::ManagedSaves::default();
        let before = revisions.revision();
        saves.remember("original", &original, &app, &before).unwrap();
        app.session.run("sheet.activate", json!({"sheet":0})).unwrap();
        revisions.observe(&app);
        app.run("cell.set", json!({"cell":"A2","input":"new content"})).unwrap();
        assert!(saves.acknowledge(&mut app, "original", &original.document, &before));
        assert!(super::dirty(&app), "old snapshot cannot clean later content");
        app.begin_edit(Some("parked readonly text".into()), false);
        app.set_read_only(true);
        let workbook = app.session.active().unwrap().wb.clone();
        assert!(crate::native_document::export(&mut app).is_err());
        assert!(Arc::ptr_eq(&workbook, &app.session.active().unwrap().wb));
        assert_eq!(app.cell_draft().unwrap().text, "parked readonly text");
        assert!(super::dirty(&app));
    }

    #[test]
    fn draft_counter_wraps_with_a_bounded_opaque_string() {
        let mut app = app();
        let mut revisions = super::Revisions::default();
        revisions.observe(&app);
        revisions.serial = u64::MAX;
        app.begin_edit(Some("another".into()), false);
        assert!(revisions.observe(&app));
        assert_eq!(revisions.revision(), "0");
    }
}
