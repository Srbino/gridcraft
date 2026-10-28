//! Whole native documents for opt-in embedding hosts.

use gridcraft_ui_egui::SheetApp;

pub struct Snapshot {
    pub bytes: Vec<u8>,
    pub document: String,
    pub revision: String,
}

pub struct Opened {
    pub document: String,
    pub revision: String,
}

/// A correlated host import completes through the real UI/engine command before
/// its identity is returned. Merely enqueueing bytes in the file-picker inbox
/// cannot prove which document was opened.
pub fn open(app: &mut SheetApp, name: &str, bytes: &[u8]) -> Result<Opened, String> {
    // File commands settle drafts. Refuse before that boundary so an error or
    // a replacement cannot silently commit or discard unacknowledged work.
    if crate::draft_state::dirty(app) || app.editor.is_some() || app.text_box_editor.is_some() {
        return Err("Save or discard unsaved work and finish native drafts before opening another workbook.".into());
    }
    let result = app.import_host_bytes(name, bytes)?;
    if let Some(warnings) = result.get("warnings").and_then(serde_json::Value::as_array)
        && !warnings.is_empty()
    {
        return Err(warnings.iter().filter_map(serde_json::Value::as_str).collect::<Vec<_>>().join("\n"));
    }
    let doc = app.session.active().ok_or_else(|| "The engine did not open a workbook.".to_string())?;
    Ok(Opened { document: doc.uid.to_string(), revision: doc.revision.to_string() })
}

/// Encode the complete native workbook through the same command as desktop/CLI.
/// Native checkpoints preserve ongoing cell/text-box editing and its undo gesture.
/// Export is not a durable save and deliberately leaves the dirty baseline alone.
pub fn export(app: &mut SheetApp) -> Result<Snapshot, String> {
    let encoded = app.native_snapshot_bytes()?;
    let bytes = encoded
        .get("base64")
        .and_then(serde_json::Value::as_str)
        .and_then(gridcraft_engine::io::base64_decode)
        .ok_or_else(|| "The engine did not return a native workbook.".to_string())?;
    validate(&bytes)?;
    // A pending draft may have advanced the revision during the file command.
    let doc = app.session.active().ok_or_else(|| "No workbook is open.".to_string())?;
    Ok(Snapshot { bytes, document: doc.uid.to_string(), revision: doc.revision.to_string() })
}

/// Limit native RPC bytes before crossing the JS-to-Rust copy boundary.
/// The private port also bounds file import/export; legacy window import stays unchanged.
pub fn validate_length(length: usize) -> Result<(), String> {
    if length > 32 * 1024 * 1024 {
        return Err("Native validation accepts at most 32 MiB.".into());
    }
    Ok(())
}

/// Decode using the application's engine without changing the open workbook.
pub fn validate(bytes: &[u8]) -> Result<(), String> {
    // The file reader also sniffs XLSX irrespective of the supplied extension.
    // This endpoint promises native JSON, not an arbitrary import.
    serde_json::from_slice::<serde_json::Value>(bytes).map_err(|e| e.to_string())?;
    let (_, warnings) = gridcraft_engine::io::open_bytes("snapshot.json", bytes).map_err(|e| e.to_string())?;
    if !warnings.is_empty() {
        return Err(warnings.join("\n"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use gridcraft_engine::Session;
    use gridcraft_ui_egui::{Services, SheetApp};
    use serde_json::json;

    fn app() -> SheetApp {
        let mut session = Session::new();
        session.new_workbook();
        SheetApp::new(session, Services::default())
    }

    #[test]
    fn host_snapshot_keeps_the_live_cell_editor_caret_and_selection() {
        let mut app = app();
        app.begin_edit(Some("Repeated".into()), false);
        let ed = app.editor.as_mut().unwrap();
        ed.caret = 5;
        ed.anchor = 2;
        let snapshot = super::export(&mut app).unwrap();
        let wb: gridcraft_engine::model::Workbook = serde_json::from_slice(&snapshot.bytes).unwrap();
        assert_eq!(wb.active().unwrap().input_text(gridcraft_engine::core::CellRef::new(0, 0)), "Repeated");
        let ed = app.editor.as_ref().expect("host snapshot must not end ongoing native typing");
        assert_eq!(ed.text, "Repeated");
        assert_eq!((ed.caret, ed.anchor), (5, 2));
        assert!(ed.enter_mode);
    }

    #[test]
    fn host_snapshot_does_not_accept_the_unselected_autocomplete_suggestion() {
        let mut app = app();
        app.begin_edit(Some("Rep".into()), false);
        app.editor.as_mut().unwrap().completion = Some("Repeated suggestion".into());
        let snapshot = super::export(&mut app).unwrap();
        let wb: gridcraft_engine::model::Workbook = serde_json::from_slice(&snapshot.bytes).unwrap();
        assert_eq!(wb.active().unwrap().input_text(gridcraft_engine::core::CellRef::new(0, 0)), "Rep");
        assert_eq!(app.editor.as_ref().unwrap().completion.as_deref(), Some("Repeated suggestion"));
    }

    #[test]
    fn host_snapshot_deleted_owner_refuses_enter_but_escape_can_still_rollback_the_group() {
        let mut app = app();
        app.run("home.insertSheet", json!({"name":"Second"})).unwrap();
        app.run("home.insertSheet", json!({"name":"Third"})).unwrap();
        app.run("cell.set", json!({"cell":"A1","input":"original third"})).unwrap();
        let mut saves = crate::managed_save::ManagedSaves::default();
        checkpoint(&mut app, &mut saves, "baseline-third");
        let undo = app.session.active().unwrap().undo.len();
        app.begin_edit(Some("captured third".into()), false);
        checkpoint(&mut app, &mut saves, "captured-third");
        assert_eq!(app.editor.as_ref().unwrap().sheet, 2);
        app.run("home.deleteSheet", json!({"sheet":2})).unwrap();
        assert_eq!(app.session.active().unwrap().wb.sheets.len(), 2);
        assert!(!app.commit_edit(0, 0, false, false), "the deleted owner cannot be reactivated");
        assert_eq!(app.editor.as_ref().unwrap().text, "captured third");
        assert!(app.session.in_group(app.session.active().unwrap().uid));
        app.cancel_edit();
        assert!(app.editor.is_none());
        assert!(!app.session.in_group(app.session.active().unwrap().uid), "Escape must retain the original group association after refusal");
        let doc = app.session.active().unwrap();
        assert_eq!(doc.wb.sheets.len(), 3);
        assert_eq!(doc.wb.sheets[2].input_text(gridcraft_engine::core::CellRef::new(0, 0)), "original third");
        assert_eq!(doc.undo.len(), undo);
        assert!(crate::draft_state::dirty(&app), "the acknowledged partial edit still differs from the rolled-back value");
    }

    fn checkpoint(app: &mut SheetApp, saves: &mut crate::managed_save::ManagedSaves, id: &str) {
        let snapshot = super::export(app).unwrap();
        let wire = snapshot.revision.clone();
        saves.remember(id, &snapshot, app, &wire).unwrap();
        assert!(saves.acknowledge(app, id, &snapshot.document, &wire));
    }

    fn original_cell() -> (SheetApp, crate::managed_save::ManagedSaves, usize) {
        let mut app = app();
        app.run("cell.set", json!({"cell":"A1","input":"original"})).unwrap();
        let mut saves = crate::managed_save::ManagedSaves::default();
        checkpoint(&mut app, &mut saves, "baseline");
        let undo = app.session.active().unwrap().undo.len();
        (app, saves, undo)
    }

    fn input(app: &SheetApp) -> String {
        app.session.active().unwrap().wb.active().unwrap().input_text(gridcraft_engine::core::CellRef::new(0, 0))
    }

    #[test]
    fn host_snapshot_checkpoints_keep_one_undo_gesture_and_later_undo_stays_dirty_after_ack() {
        let (mut app, mut saves, undo) = original_cell();
        app.begin_edit(Some("Repeated".into()), false);
        checkpoint(&mut app, &mut saves, "first");
        assert!(!crate::draft_state::dirty(&app));
        app.editor.as_mut().unwrap().text.push_str(" complete");
        checkpoint(&mut app, &mut saves, "second");
        assert_eq!(app.session.active().unwrap().undo.len(), undo);
        assert!(app.session.in_group(app.session.active().unwrap().uid));
        assert!(!crate::draft_state::dirty(&app));
        assert!(app.commit_edit(0, 0, false, false));
        assert!(app.editor.is_none());
        assert!(!app.session.in_group(app.session.active().unwrap().uid));
        assert_eq!(app.session.active().unwrap().undo.len(), undo + 1);
        app.run("edit.undo", json!({})).unwrap();
        assert_eq!(input(&app), "original");
        assert!(crate::draft_state::dirty(&app), "server still stores the acknowledged completed text");
        app.run("edit.redo", json!({})).unwrap();
        assert_eq!(input(&app), "Repeated complete");
        assert!(!crate::draft_state::dirty(&app));
    }

    #[test]
    fn host_snapshot_escape_after_ack_restores_original_and_needs_a_new_save() {
        let (mut app, mut saves, undo) = original_cell();
        app.begin_edit(Some("captured prefix".into()), false);
        checkpoint(&mut app, &mut saves, "prefix");
        assert!(!crate::draft_state::dirty(&app));
        app.cancel_edit();
        assert!(app.editor.is_none());
        assert_eq!(input(&app), "original");
        assert_eq!(app.session.active().unwrap().undo.len(), undo);
        assert!(!app.session.in_group(app.session.active().unwrap().uid));
        assert!(crate::draft_state::dirty(&app));
        checkpoint(&mut app, &mut saves, "cancelled-value");
        assert!(!crate::draft_state::dirty(&app));
    }

    #[test]
    fn host_snapshot_invalid_prefix_keeps_caret_without_modal_and_does_not_leak_an_empty_group() {
        let (mut app, mut saves, undo) = original_cell();
        app.begin_edit(Some("=1+".into()), true);
        app.editor.as_mut().unwrap().caret = 2;
        app.editor.as_mut().unwrap().anchor = 1;
        let before = app.session.active().unwrap().wb.clone();
        assert!(super::export(&mut app).is_err());
        assert!(std::sync::Arc::ptr_eq(&before, &app.session.active().unwrap().wb));
        assert_eq!(app.editor.as_ref().unwrap().text, "=1+");
        assert_eq!((app.editor.as_ref().unwrap().caret, app.editor.as_ref().unwrap().anchor), (2, 1));
        assert!(app.message.is_none());
        assert!(!app.session.in_group(app.session.active().unwrap().uid));
        app.editor.as_mut().unwrap().text = "valid checkpoint".into();
        checkpoint(&mut app, &mut saves, "valid");
        app.editor.as_mut().unwrap().text = "=2+".into();
        assert!(super::export(&mut app).is_err());
        assert!(app.session.in_group(app.session.active().unwrap().uid));
        assert_eq!(input(&app), "valid checkpoint");
        assert!(app.message.is_none());
        app.editor.as_mut().unwrap().text = "valid retry".into();
        checkpoint(&mut app, &mut saves, "retry");
        app.cancel_edit();
        assert_eq!(input(&app), "original");
        assert_eq!(app.session.active().unwrap().undo.len(), undo);
        assert!(crate::draft_state::dirty(&app));
    }

    #[test]
    fn host_snapshot_checkpoint_does_not_flush_held_readonly_input() {
        let (mut app, mut saves, _) = original_cell();
        app.begin_edit(Some("checkpoint".into()), false);
        checkpoint(&mut app, &mut saves, "checkpoint");
        app.editor.as_mut().unwrap().text.push_str(" held tail");
        app.set_read_only(true);
        let before = app.session.active().unwrap().wb.clone();
        assert!(super::export(&mut app).is_err());
        assert!(std::sync::Arc::ptr_eq(&before, &app.session.active().unwrap().wb));
        assert_eq!(app.cell_draft().unwrap().text, "checkpoint held tail");
        app.set_read_only(false);
        app.cancel_edit();
        assert_eq!(input(&app), "original");
        assert!(crate::draft_state::dirty(&app));
    }

    #[test]
    fn host_snapshot_repeated_textbox_checkpoints_preserve_edit_and_one_undo_or_cancel() {
        for cancel in [false, true] {
            let mut app = app();
            let id = app.run("insert.textBox", json!({"at":"D8","text":"original box"})).unwrap()["shape"].as_u64().unwrap() as u32;
            let mut saves = crate::managed_save::ManagedSaves::default();
            checkpoint(&mut app, &mut saves, "box-baseline");
            let undo = app.session.active().unwrap().undo.len();
            assert!(app.begin_text_box_edit(id));
            for (id, text) in [("box-one", "prefix"), ("box-two", "prefix completed")] {
                app.text_box_editor.as_mut().unwrap().text = text.into();
                checkpoint(&mut app, &mut saves, id);
                assert_eq!(app.text_box_editor.as_ref().unwrap().text, text);
                assert!(!crate::draft_state::dirty(&app));
                assert_eq!(app.session.active().unwrap().undo.len(), undo);
            }
            let text =
                |app: &SheetApp| app.session.active().unwrap().wb.active().unwrap().shapes.iter().find(|shape| shape.id == id).unwrap().text.clone();
            if cancel {
                app.cancel_text_box_edit();
                assert_eq!(text(&app), "original box");
                assert!(crate::draft_state::dirty(&app));
            } else {
                app.commit_text_box_edit().unwrap();
                assert_eq!(app.session.active().unwrap().undo.len(), undo + 1);
                app.run("edit.undo", json!({})).unwrap();
                assert_eq!(text(&app), "original box");
                assert!(crate::draft_state::dirty(&app));
                app.run("edit.redo", json!({})).unwrap();
                assert_eq!(text(&app), "prefix completed");
            }
            assert!(app.text_box_editor.is_none());
            assert!(!app.session.in_group(app.session.active().unwrap().uid));
        }
    }

    #[test]
    fn host_snapshot_keeps_cross_sheet_point_mode_and_explicit_enter_finishes_on_its_owner() {
        let mut app = app();
        app.run("home.insertSheet", json!({"name":"Second"})).unwrap();
        app.session.run("sheet.activate", json!({"sheet":0})).unwrap();
        app.session.run("selection.set", json!({"cell":"B2"})).unwrap();
        app.begin_edit(Some("=SUM(Second!A1)".into()), true);
        app.editor.as_mut().unwrap().point = Some((5, 14));
        app.editor.as_mut().unwrap().point_cell = Some(gridcraft_engine::core::CellRef::new(0, 0));
        app.editor.as_mut().unwrap().point_anchor = Some((1, gridcraft_engine::core::CellRef::new(0, 0)));
        app.editor.as_mut().unwrap().caret = 14;
        app.editor.as_mut().unwrap().anchor = 5;
        app.session.run("sheet.activate", json!({"sheet":1})).unwrap();
        app.session.run("selection.set", json!({"cell":"C3"})).unwrap();
        app.session.mode = gridcraft_engine::Mode::Point;
        let before = app.session.active().unwrap().selection.clone();
        let snapshot = super::export(&mut app).unwrap();
        let wb: gridcraft_engine::model::Workbook = serde_json::from_slice(&snapshot.bytes).unwrap();
        assert_eq!(wb.active_sheet(), 1);
        assert_eq!(app.session.active().unwrap().wb.active_sheet(), 1);
        assert_eq!(app.session.active().unwrap().selection, before);
        assert_eq!(app.session.mode, gridcraft_engine::Mode::Point);
        let ed = app.editor.as_ref().unwrap();
        assert_eq!((ed.caret, ed.anchor), (14, 5));
        assert_eq!(ed.point, Some((5, 14)));
        assert!(ed.from_formula_bar);
        assert!(app.commit_edit(0, 0, false, false));
        assert_eq!(app.session.active().unwrap().wb.active_sheet(), 0);
        app.run("edit.undo", json!({})).unwrap();
        assert!(app.session.active().unwrap().wb.sheets[0].cell(gridcraft_engine::core::CellRef::new(1, 1)).is_none());
    }

    #[test]
    fn host_snapshot_unchanged_live_edit_keeps_one_revision_and_first_identical_ack() {
        let mut app = app();
        app.begin_edit(Some("unchanged checkpoint".into()), false);
        let first = super::export(&mut app).unwrap();
        let mut revisions = crate::draft_state::Revisions::default();
        revisions.observe(&app);
        let wire = revisions.revision();
        let mut saves = crate::managed_save::ManagedSaves::default();
        saves.remember("first", &first, &app, &wire).unwrap();
        let second = super::export(&mut app).unwrap();
        assert_eq!(first.bytes, second.bytes);
        assert!(!revisions.observe(&app));
        saves.remember("second", &second, &app, &wire).unwrap();
        assert!(saves.acknowledge(&mut app, "first", &first.document, &wire));
        assert!(!crate::draft_state::dirty(&app));
        assert!(app.editor.is_some());
        app.editor.as_mut().unwrap().text.push_str(" later");
        assert!(crate::draft_state::dirty(&app));
        assert!(!saves.acknowledge(&mut app, "second", &first.document, &wire));
        assert!(crate::draft_state::dirty(&app));
    }

    #[test]
    fn ordinary_file_encoding_keeps_its_existing_commit_and_close_behavior() {
        let mut app = app();
        app.begin_edit(Some("ordinary export".into()), false);
        app.run("file.saveBytes", json!({"format":"json"})).unwrap();
        assert!(app.editor.is_none());
        assert!(!app.session.in_group(app.session.active().unwrap().uid));
        assert_eq!(input(&app), "ordinary export");
    }

    #[test]
    fn correlated_open_identity_belongs_to_the_import_not_a_later_file_new() {
        let mut app = app();
        let mut source = Session::new();
        source.new_workbook();
        source.execute("cell.set", json!({"cell":"A1","input":"import A"})).unwrap();
        let encoded = source.execute("file.saveBytes", json!({"format":"json"})).unwrap();
        let bytes = gridcraft_engine::io::base64_decode(encoded["base64"].as_str().unwrap()).unwrap();
        let opened = super::open(&mut app, "owned.json", &bytes).unwrap();
        assert_eq!(opened.document, app.session.active().unwrap().uid.to_string());
        assert_eq!(opened.revision, app.session.active().unwrap().revision.to_string());
        assert_eq!(app.run("cell.get", json!({"cell":"A1"})).unwrap()["value"], "import A");
        app.run("file.new", json!({})).unwrap();
        assert_ne!(opened.document, app.session.active().unwrap().uid.to_string());
        let before = app.session.active().unwrap().uid;
        assert!(super::open(&mut app, "invalid.json", b"invalid JSON").is_err());
        assert_eq!(before, app.session.active().unwrap().uid);
    }

    fn imported_bytes(value: &str) -> Vec<u8> {
        let mut source = Session::new();
        source.new_workbook();
        source.execute("cell.set", json!({"cell":"A1","input":value})).unwrap();
        let encoded = source.execute("file.saveBytes", json!({"format":"json"})).unwrap();
        gridcraft_engine::io::base64_decode(encoded["base64"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn same_import_name_with_new_bytes_opens_a_new_actual_document() {
        let mut app = app();
        let first = super::open(&mut app, "owned.json", &imported_bytes("import A")).unwrap();
        let second = super::open(&mut app, "owned.json", &imported_bytes("import B")).unwrap();
        assert_ne!(first.document, second.document);
        assert_eq!(second.document, app.session.active().unwrap().uid.to_string());
        assert_eq!(app.run("cell.get", json!({"cell":"A1"})).unwrap()["value"], "import B");
    }

    #[test]
    fn owned_import_refuses_dirty_work_without_replacing_or_settling_it() {
        let mut app = app();
        app.run("cell.set", json!({"cell":"A1","input":"unsaved"})).unwrap();
        let before = app.session.active().unwrap().wb.clone();
        let uid = app.session.active().unwrap().uid;
        assert!(super::open(&mut app, "owned.json", &imported_bytes("replacement")).is_err());
        assert_eq!(uid, app.session.active().unwrap().uid);
        assert!(std::sync::Arc::ptr_eq(&before, &app.session.active().unwrap().wb));
        assert!(crate::draft_state::dirty(&app));
    }

    #[test]
    fn owned_import_error_retains_pending_cell_without_committing_it() {
        let mut app = app();
        let before = app.session.active().unwrap().wb.clone();
        app.begin_edit(Some("pending cell".into()), false);
        assert!(super::open(&mut app, "invalid.json", b"invalid JSON").is_err());
        assert_eq!(app.editor.as_ref().unwrap().text, "pending cell");
        assert!(std::sync::Arc::ptr_eq(&before, &app.session.active().unwrap().wb));
        assert!(crate::draft_state::dirty(&app));
    }

    #[test]
    fn owned_import_refuses_pending_text_box_without_committing_it() {
        let mut app = app();
        let id = app.run("insert.textBox", json!({"at":"B2","text":"original"})).unwrap()["shape"].as_u64().unwrap() as u32;
        let doc = app.session.active_mut().unwrap();
        doc.saved = doc.wb.clone();
        let before = doc.wb.clone();
        assert!(app.begin_text_box_edit(id));
        app.text_box_editor.as_mut().unwrap().text = "pending text box".into();
        assert!(super::open(&mut app, "owned.json", &imported_bytes("replacement")).is_err());
        assert_eq!(app.text_box_editor.as_ref().unwrap().text, "pending text box");
        assert!(std::sync::Arc::ptr_eq(&before, &app.session.active().unwrap().wb));
        assert!(crate::draft_state::dirty(&app));
    }

    #[test]
    fn native_export_checkpoints_the_cell_draft_without_ending_input_or_claiming_durable_save() {
        let mut app = app();
        app.run("cell.set", json!({"cell":"A1", "input":"21"})).unwrap();
        app.run("cell.set", json!({"cell":"B1", "input":"=A1*2"})).unwrap();
        app.run("home.bold", json!({"range":"A1", "on":true})).unwrap();
        app.run("home.numberFormat", json!({"range":"B1", "code":"0.00"})).unwrap();
        app.run("home.mergeCells", json!({"range":"A4:C4"})).unwrap();
        app.run("home.rowHeight", json!({"rows":"5:5", "height":42})).unwrap();
        app.run("home.insertSheet", json!({"name":"Second"})).unwrap();
        app.run("cell.set", json!({"cell":"A1", "input":"Second sheet"})).unwrap();
        app.run("sheet.activate", json!({"sheet":"Sheet1"})).unwrap();
        app.run("selection.set", json!({"cell":"A1"})).unwrap();
        let before = app.session.active().unwrap().revision;
        let document = app.session.active().unwrap().uid.to_string();
        app.begin_edit(Some("22".into()), false);
        let snapshot = super::export(&mut app).unwrap();
        assert_eq!(snapshot.document, document);
        assert!(app.session.active().unwrap().revision > before);
        assert_eq!(snapshot.revision, app.session.active().unwrap().revision.to_string());
        let bytes = snapshot.bytes;
        assert_eq!(app.editor.as_ref().unwrap().text, "22");
        assert!(app.session.active().unwrap().is_dirty());
        let (wb, _) = gridcraft_engine::io::open_bytes("snapshot.json", &bytes).unwrap();
        // Formula AST caches and rebuilt style indices are intentionally not persisted.
        assert_eq!(serde_json::to_value(&wb).unwrap(), serde_json::to_value(app.session.active().unwrap().wb.as_ref()).unwrap());
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json.get("sheets").is_some());
    }

    #[test]
    fn native_validation_length_boundary_is_checked_without_allocating_payloads() {
        assert!(super::validate_length(31 * 1024 * 1024).is_ok());
        assert!(super::validate_length(32 * 1024 * 1024).is_ok());
        assert!(super::validate_length(32 * 1024 * 1024 + 1).is_err());
    }

    #[test]
    fn native_validation_rejects_invalid_documents() {
        assert!(super::validate(b"not JSON").is_err());
        assert!(super::validate(br#"{"sheets":"broken"}"#).is_err());
    }

    #[test]
    fn rejected_draft_is_retained_instead_of_exporting_an_older_cell() {
        let mut app = app();
        app.begin_edit(Some("=1+".into()), false);
        assert!(super::export(&mut app).is_err());
        assert!(app.editor.is_some());
    }

    #[test]
    fn native_validation_does_not_accept_an_xlsx_disguised_as_json() {
        let mut session = Session::new();
        session.new_workbook();
        let encoded = session.execute("file.saveBytes", json!({"format":"xlsx"})).unwrap();
        let bytes = gridcraft_engine::io::base64_decode(encoded["base64"].as_str().unwrap()).unwrap();
        assert!(super::validate(&bytes).is_err());
    }

    #[test]
    fn native_export_has_no_four_mib_checkpoint_limit() {
        let mut app = app();
        let values: Vec<Vec<String>> = (0..130).map(|_| vec!["x".repeat(32767)]).collect();
        app.run("range.setValues", json!({"range":"A1", "values":values})).unwrap();
        let bytes = super::export(&mut app).unwrap().bytes;
        assert!(bytes.len() > 4 * 1024 * 1024);
        super::validate(&bytes).unwrap();
    }
}
