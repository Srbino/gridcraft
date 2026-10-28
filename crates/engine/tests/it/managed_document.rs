use gridcraft_engine::Session;
use serde_json::json;
use std::sync::Arc;

fn encoded_workbook() -> String {
    let mut source = Session::new();
    source.new_workbook();
    source.execute("cell.set", json!({"cell":"B2","input":"=SUM(1,2)"})).unwrap();
    source.execute("home.bold", json!({"range":"B2","on":true})).unwrap();
    source.execute("file.saveBytes", json!({"format":"xlsx"})).unwrap()["base64"].as_str().unwrap().to_owned()
}

#[test]
fn managed_import_keeps_identity_saved_baseline_and_undoable_native_content() {
    let mut s = Session::new();
    s.new_workbook();
    s.execute("cell.set", json!({"cell":"A1","input":"before import"})).unwrap();
    let doc = s.active_mut().unwrap();
    doc.saved = doc.wb.clone();
    let (uid, revision, baseline) = (doc.uid, doc.revision, doc.saved.clone());
    let previous_window = s.active_window().unwrap();
    s.set_managed_document(true);
    s.execute("file.open", json!({"name":"incoming.xlsx","base64":encoded_workbook()})).unwrap();
    let doc = s.active().unwrap();
    assert_eq!(doc.uid, uid, "an import edits the host-owned document");
    assert_eq!(s.documents().len(), 1);
    assert!(Arc::ptr_eq(&doc.saved, &baseline), "no host ACK was received");
    assert!(doc.is_dirty());
    assert_eq!(doc.revision, revision + 1);
    assert_ne!(s.active_window(), Some(previous_window), "the imported native view needs fresh UI state");
    assert!(s.window(previous_window).is_none());
    assert_eq!(s.execute("cell.get", json!({"cell":"B2"})).unwrap()["value"], 3.0);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.active().unwrap().uid, uid);
    assert_eq!(s.execute("cell.get", json!({"cell":"A1"})).unwrap()["value"], "before import");
    s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(s.execute("cell.get", json!({"cell":"B2"})).unwrap()["value"], 3.0);
    assert!(Arc::ptr_eq(&s.active().unwrap().saved, &baseline));
}

#[test]
fn invalid_managed_import_preserves_data_baseline_and_history() {
    let mut s = Session::new();
    s.new_workbook();
    s.execute("cell.set", json!({"cell":"A1","input":"pending content"})).unwrap();
    {
        let doc = s.active_mut().unwrap();
        doc.saved = doc.wb.clone();
    }
    s.set_managed_document(true);
    let doc = s.active().unwrap();
    let (uid, wb, saved, revision, history) = (doc.uid, doc.wb.clone(), doc.saved.clone(), doc.revision, doc.undo.len());
    assert!(s.execute("file.open", json!({"name":"broken.json","base64":"e30="})).is_err());
    let doc = s.active().unwrap();
    assert_eq!((doc.uid, doc.revision, doc.undo.len()), (uid, revision, history));
    assert!(Arc::ptr_eq(&doc.wb, &wb));
    assert!(Arc::ptr_eq(&doc.saved, &saved));
}

#[test]
fn managed_lifecycle_is_owned_by_the_host_and_default_file_open_is_unchanged() {
    for id in ["file.new", "file.close", "window.close", "view.newWindow"] {
        let mut s = Session::new();
        s.new_workbook();
        let uid = s.active().unwrap().uid;
        s.set_managed_document(true);
        assert!(!s.commands().into_iter().find(|info| info.id == id).unwrap().enabled, "{id}");
        assert!(s.execute(id, json!({"force":true})).is_err(), "{id}");
        assert_eq!(s.active().unwrap().uid, uid);
    }
    let mut standalone = Session::new();
    standalone.new_workbook();
    assert!(!standalone.is_managed_document());
    let uid = standalone.active().unwrap().uid;
    standalone.execute("file.open", json!({"name":"standalone.xlsx","base64":encoded_workbook()})).unwrap();
    assert_ne!(standalone.active().unwrap().uid, uid);
    assert!(!standalone.active().unwrap().is_dirty());
}

#[test]
fn trusted_read_only_bootstrap_stays_clean_and_never_grants_ui_import() {
    let mut s = Session::new();
    s.new_workbook();
    s.set_managed_document(true);
    s.set_read_only(true);
    let bytes = gridcraft_engine::io::base64_decode(&encoded_workbook()).unwrap();
    assert!(s.execute("file.open", json!({"name":"forbidden.xlsx","base64":encoded_workbook()})).is_err());
    s.import_bytes("owned.xlsx", &bytes).unwrap();
    assert!(s.is_read_only());
    assert!(s.is_managed_document());
    assert!(!s.active().unwrap().is_dirty());
    assert!(s.execute("cell.set", json!({"cell":"A1","input":"forbidden"})).is_err());
}

#[test]
fn managed_import_requires_saved_content_or_an_explicit_discard_decision() {
    let mut s = Session::new();
    s.new_workbook();
    s.execute("cell.set", json!({"cell":"A1","input":"unsaved work"})).unwrap();
    s.set_managed_document(true);
    let uid = s.active().unwrap().uid;
    let before = s.active().unwrap().wb.clone();
    let saved = s.active().unwrap().saved.clone();
    let input = encoded_workbook();
    assert!(s.execute("file.open", json!({"name":"incoming.xlsx","base64":input})).is_err());
    assert!(Arc::ptr_eq(&s.active().unwrap().wb, &before));
    s.execute("file.open", json!({"name":"incoming.xlsx","base64":input,"discard":true})).unwrap();
    assert_eq!(s.active().unwrap().uid, uid);
    assert!(Arc::ptr_eq(&s.active().unwrap().saved, &saved));
    assert!(s.active().unwrap().is_dirty());
}

#[test]
fn managed_save_does_not_acknowledge_locally_or_choose_a_file_dialog() {
    let mut s = Session::new();
    s.new_workbook();
    s.execute("cell.set", json!({"cell":"A1","input":"unsaved"})).unwrap();
    s.set_managed_document(true);
    let baseline = s.active().unwrap().saved.clone();
    assert!(s.execute("file.save", json!({})).is_err());
    assert!(Arc::ptr_eq(&s.active().unwrap().saved, &baseline));
    assert!(s.active().unwrap().is_dirty());
    assert!(s.ui_requests.is_empty());
}
