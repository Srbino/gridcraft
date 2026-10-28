use super::*;

impl SheetApp {
    /// Opt-in host document custody: the callback requests durable native Save;
    /// it never acknowledges storage or turns a download into Saved.
    pub fn set_managed_document_save(&mut self, handler: Option<Box<dyn Fn() -> Result<(), String>>>) {
        self.session.set_managed_document(handler.is_some());
        self.managed_document_save = handler;
    }
}

#[cfg(test)]
mod tests {
    use crate::{Services, SheetApp};
    use gridcraft_engine::Session;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    type Downloads = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    fn app() -> (SheetApp, Arc<AtomicUsize>, Downloads) {
        let mut session = Session::new();
        session.new_workbook();
        let downloads = Arc::new(Mutex::new(Vec::new()));
        let copy = downloads.clone();
        let mut app = SheetApp::new(
            session,
            Services {
                download: Some(Box::new(move |name, bytes| copy.lock().unwrap().push((name.to_owned(), bytes.to_vec())))),
                ..Default::default()
            },
        );
        let saves = Arc::new(AtomicUsize::new(0));
        let count = saves.clone();
        app.set_managed_document_save(Some(Box::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })));
        (app, saves, downloads)
    }

    #[test]
    fn ordinary_save_requests_native_storage_without_download_or_saved_baseline() {
        let (mut app, saves, downloads) = app();
        app.run("cell.set", json!({"cell":"A1","input":"unsaved"})).unwrap();
        let baseline = app.session.active().unwrap().saved.clone();
        app.run("file.save", json!({})).unwrap();
        assert_eq!(saves.load(Ordering::SeqCst), 1);
        assert!(downloads.lock().unwrap().is_empty());
        assert!(Arc::ptr_eq(&app.session.active().unwrap().saved, &baseline));
        assert!(app.session.active().unwrap().is_dirty());
        assert!(app.dialog.is_none());
    }

    #[test]
    fn explicit_save_as_encodes_xlsx_without_requesting_native_save_or_acknowledging() {
        let (mut app, saves, downloads) = app();
        app.run("cell.set", json!({"cell":"A1","input":"downloaded"})).unwrap();
        let baseline = app.session.active().unwrap().saved.clone();
        app.open_dialog("saveAs", json!({}));
        assert_eq!(saves.load(Ordering::SeqCst), 0);
        let downloaded = downloads.lock().unwrap();
        assert_eq!(downloaded.len(), 1);
        assert!(downloaded[0].0.ends_with(".xlsx"));
        let mut check = Session::new();
        check.new_workbook();
        check.execute("file.open", json!({"name":downloaded[0].0,"base64":gridcraft_engine::io::base64_encode(&downloaded[0].1)})).unwrap();
        assert_eq!(check.execute("cell.get", json!({"cell":"A1"})).unwrap()["value"], "downloaded");
        assert!(Arc::ptr_eq(&app.session.active().unwrap().saved, &baseline));
        assert!(app.session.active().unwrap().is_dirty());
    }

    #[test]
    fn managed_lifecycle_refuses_before_settling_the_current_draft() {
        for id in ["file.new", "file.close", "window.close", "view.newWindow"] {
            let (mut app, _, _) = app();
            app.begin_edit(Some("retained".into()), false);
            let before = app.session.active().unwrap().wb.clone();
            assert!(app.run(id, json!({"force":true})).is_err(), "{id}");
            assert!(Arc::ptr_eq(&before, &app.session.active().unwrap().wb));
            assert_eq!(app.cell_draft().unwrap().text, "retained");
        }
    }

    #[test]
    fn managed_open_keeps_a_settled_draft_when_the_import_is_refused() {
        let (mut app, _, _) = app();
        app.begin_edit(Some("previous pending content".into()), false);
        assert!(app.run("file.open", json!({"name":"other.json","base64":"e30="})).is_err());
        assert_eq!(app.run("cell.get", json!({"cell":"A1"})).unwrap()["value"], "previous pending content");
        assert!(app.session.active().unwrap().is_dirty());
    }

    #[test]
    fn readonly_refuses_save_and_import_before_flushing_retained_drafts() {
        let (mut app, saves, downloads) = app();
        app.begin_edit(Some("retained".into()), false);
        app.set_read_only(true);
        let before = app.session.active().unwrap().wb.clone();
        assert!(app.run("file.save", json!({})).is_err());
        assert!(app.run("file.open", json!({"name":"other.json","base64":"e30="})).is_err());
        assert_eq!(saves.load(Ordering::SeqCst), 0);
        assert!(downloads.lock().unwrap().is_empty());
        assert!(Arc::ptr_eq(&before, &app.session.active().unwrap().wb));
        assert_eq!(app.cell_draft().unwrap().text, "retained");
    }
}
