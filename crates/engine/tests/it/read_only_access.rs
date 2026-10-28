use gridcraft_engine::Session;
use serde_json::json;
use std::sync::Arc;

#[test]
fn read_only_registry_refuses_mutations_before_they_touch_data_or_history() {
    let mut session = Session::new();
    session.new_workbook();
    session.execute("cell.set", json!({"cell":"A1","input":"original"})).unwrap();
    session.execute("insert.textBox", json!({"at":"D8","text":"original box"})).unwrap();
    let before = session.active().unwrap().wb.clone();
    let history = session.active().unwrap().undo.len();
    let journal = session.journal.len();
    session.set_read_only(true);
    for (command, params) in [
        ("cell.set", json!({"cell":"A1","input":"replacement"})),
        ("home.bold", json!({"range":"A1","on":true})),
        ("home.rowHeight", json!({"rows":"5:5","height":99})),
        ("shape.setText", json!({"id":1,"text":"replacement"})),
        ("edit.cut", json!({"range":"A1"})),
        ("edit.undo", json!({})),
        ("file.new", json!({})),
        ("file.open", json!({"name":"other.json","base64":"e30="})),
        ("file.save", json!({})),
    ] {
        let info = session.commands().into_iter().find(|info| info.id == command).unwrap();
        assert!(!info.enabled, "{command} must appear unavailable");
        let error = session.execute(command, params).unwrap_err().to_string();
        assert!(error.contains("read-only"), "{command}: {error}");
        assert!(Arc::ptr_eq(&before, &session.active().unwrap().wb));
    }
    assert_eq!(session.active().unwrap().undo.len(), history);
    assert_eq!(session.journal.len(), journal);
    assert!(session.is_read_only());
}

#[test]
fn read_only_keeps_selection_copy_find_and_queries_usable() {
    let mut session = Session::new();
    session.new_workbook();
    session.execute("cell.set", json!({"cell":"A1","input":"needle"})).unwrap();
    let before = session.active().unwrap().wb.clone();
    session.set_read_only(true);
    session.execute("selection.set", json!({"cell":"A1"})).unwrap();
    assert_eq!(session.execute("edit.copy", json!({})).unwrap()["text"], "needle\n");
    session.execute("selection.move", json!({"dr":1,"dc":1})).unwrap();
    session.execute("edit.find", json!({"what":"needle"})).unwrap();
    assert_eq!(session.execute("cell.get", json!({"cell":"A1"})).unwrap()["value"], "needle");
    assert!(Arc::ptr_eq(&before, &session.active().unwrap().wb));
}

#[test]
fn trusted_bytes_import_keeps_the_runtime_guard_and_does_not_enable_ui_file_open() {
    let mut source = Session::new();
    source.new_workbook();
    source.execute("cell.set", json!({"cell":"A1","input":"imported"})).unwrap();
    let encoded = source.execute("file.saveBytes", json!({"format":"json"})).unwrap();
    let bytes = gridcraft_engine::io::base64_decode(encoded["base64"].as_str().unwrap()).unwrap();
    let mut session = Session::new();
    session.new_workbook();
    let old = session.active().unwrap().uid;
    session.set_read_only(true);
    session.import_bytes("owned.json", &bytes).unwrap();
    assert!(session.is_read_only());
    assert_ne!(old, session.active().unwrap().uid);
    assert_eq!(session.execute("cell.get", json!({"cell":"A1"})).unwrap()["value"], "imported");
    assert!(session.execute("cell.set", json!({"cell":"A1","input":"forbidden"})).is_err());
    assert!(!session.commands().into_iter().find(|info| info.id == "file.open").unwrap().enabled);
    session.set_read_only(false);
    session.execute("cell.set", json!({"cell":"A1","input":"allowed"})).unwrap();
}

#[test]
fn read_only_policy_is_ephemeral_and_never_rewrites_workbook_protection() {
    let mut session = Session::new();
    session.new_workbook();
    let before = serde_json::to_value(session.active().unwrap().wb.as_ref()).unwrap();
    assert!(!session.is_read_only());
    session.set_read_only(true);
    session.set_read_only(false);
    assert_eq!(before, serde_json::to_value(session.active().unwrap().wb.as_ref()).unwrap());
}

#[test]
fn read_only_never_treats_query_metadata_as_permission_to_change_the_native_workbook() {
    let mut session = Session::new();
    session.new_workbook();
    let before = session.active().unwrap().wb.clone();
    let native = serde_json::to_value(before.as_ref()).unwrap();
    let dirty = session.active().unwrap().is_dirty();
    session.set_read_only(true);
    for params in [json!({"add":"Sheet1!A1"}), json!({"remove":"Sheet1!A1"})] {
        assert!(session.execute("formulas.watchWindow", params).is_err(), "query metadata must not authorize native stash changes");
        assert!(Arc::ptr_eq(&before, &session.active().unwrap().wb));
        assert_eq!(native, serde_json::to_value(session.active().unwrap().wb.as_ref()).unwrap());
        assert_eq!(dirty, session.active().unwrap().is_dirty());
    }
    let unreviewed = gridcraft_engine::cmd::CommandSpec {
        id: "unreviewed.query",
        label: "Unreviewed query",
        menu: &[],
        shortcut: None,
        params: "{}",
        enabled: |_| Ok(()),
        run: |_, _| Ok(json!({})),
        journal: false,
        undoable: false,
    };
    assert!(unreviewed.availability(&session).is_err(), "new query commands require an explicit access review");
}

#[test]
fn reviewed_read_only_queries_leave_native_bytes_dirty_baseline_and_history_unchanged() {
    let mut session = Session::new();
    session.new_workbook();
    session.execute("cell.set", json!({"cell":"A1","input":"21"})).unwrap();
    let before = session.active().unwrap().wb.clone();
    let baseline = session.active().unwrap().saved.clone();
    let native = serde_json::to_value(before.as_ref()).unwrap();
    let dirty = session.active().unwrap().is_dirty();
    let history = session.active().unwrap().undo.len();
    let journal = session.journal.len();
    session.set_read_only(true);
    let queries: Vec<_> = gridcraft_engine::cmd::command_specs()
        .iter()
        .filter(|spec| !spec.journal && !spec.undoable && spec.availability(&session).is_ok())
        .map(|spec| spec.id)
        .collect();
    assert!(!queries.is_empty());
    assert!(!queries.contains(&"formulas.watchWindow"));
    for command in queries {
        let params = match command {
            "view.formulaBar" | "review.showComments" | "review.showAllNotes" => json!({"on":true}),
            "view.statusBar" => json!({"minimum":true,"maximum":true}),
            "formulas.evaluate" => json!({"formula":"=1+1"}),
            "file.saveBytes" => json!({"format":"json"}),
            _ => json!({}),
        };
        let _ = session.execute(command, params); // Invalid query inputs may fail, but cannot change the native state.
        assert!(Arc::ptr_eq(&before, &session.active().unwrap().wb), "{command}");
        assert!(Arc::ptr_eq(&baseline, &session.active().unwrap().saved), "{command}");
        assert_eq!(native, serde_json::to_value(session.active().unwrap().wb.as_ref()).unwrap(), "{command}");
        assert_eq!(dirty, session.active().unwrap().is_dirty(), "{command}");
        assert_eq!(history, session.active().unwrap().undo.len(), "{command}");
        assert_eq!(journal, session.journal.len(), "{command}");
    }
}
