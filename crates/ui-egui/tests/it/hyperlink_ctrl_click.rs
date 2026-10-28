//! Ctrl+click on a hyperlink cell follows the link (#1198), like a plain click does.

use egui::{Event, Modifiers, PointerButton, Pos2};
use gridcraft_engine::Session;
use gridcraft_engine::core::CellRef;
use gridcraft_ui_egui::{SheetApp, grid::Geo};
use serde_json::json;

type Harness = egui_kittest::Harness<'static, SheetApp>;

fn harness() -> Harness {
    let mut session = Session::new();
    session.new_workbook();
    session.execute("cell.set", json!({"cell": "B2", "input": "web"})).unwrap();
    session.execute("insert.link", json!({"cell": "B2", "target": "https://example.org/a"})).unwrap();
    session.execute("cell.set", json!({"cell": "B4", "input": "jump"})).unwrap();
    session.execute("insert.link", json!({"cell": "B4", "target": "Sheet1!F9"})).unwrap();
    let mut h = crate::common::builder().with_size(egui::vec2(1200.0, 800.0)).build_ui_state(
        |ui, app: &mut SheetApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        crate::common::app(session),
    );
    h.run_steps(4);
    h
}

fn at_fraction(h: &Harness, cell: &str, fraction: f32) -> Pos2 {
    let app = h.state();
    let sheet = app.session.active().unwrap().wb.active().unwrap();
    let geo = Geo::new(sheet, app.grid.rect.unwrap(), app.view().scroll);
    let r = geo.cell_rect(sheet, CellRef::parse(cell).unwrap());
    egui::pos2(r.left() + r.width() * fraction, r.center().y)
}

fn click(h: &mut Harness, cell: &str, modifiers: Modifiers) {
    click_at(h, cell, 0.3, modifiers);
}

fn click_at(h: &mut Harness, cell: &str, fraction: f32, modifiers: Modifiers) {
    let pos = at_fraction(h, cell, fraction);
    h.input_mut().events.push(Event::ModifiersChanged(modifiers));
    h.input_mut().events.extend([
        Event::PointerMoved(pos),
        Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers },
        Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers },
    ]);
    h.run_steps(3);
    h.input_mut().events.push(Event::ModifiersChanged(Modifiers::NONE));
    h.run_steps(1);
}

#[test]
fn ctrl_click_on_a_web_link_asks_to_open_it() {
    let mut h = harness();
    click(&mut h, "B2", Modifiers::COMMAND);
    let d = h.state().dialog.as_ref().expect("Ctrl+click on a web link opens the confirmation");
    assert_eq!(d.name, "openLink");
}

#[test]
fn ctrl_click_on_an_internal_link_goes_there() {
    let mut h = harness();
    click(&mut h, "B4", Modifiers::COMMAND);
    assert_eq!(h.state().session.active().unwrap().selection.active.a1(), "F9");
}

#[test]
fn plain_click_still_follows() {
    let mut h = harness();
    click(&mut h, "B4", Modifiers::NONE);
    assert_eq!(h.state().session.active().unwrap().selection.active.a1(), "F9");
}

#[test]
fn ctrl_click_beside_the_link_text_still_extends_the_selection() {
    let mut h = harness();
    click_at(&mut h, "B2", 0.9, Modifiers::NONE);
    click_at(&mut h, "B4", 0.9, Modifiers::COMMAND);
    let d = h.state().session.active().unwrap();
    let ranges: Vec<String> = d.selection.ranges.iter().map(|r| r.a1()).collect();
    assert_eq!(ranges, ["B2", "B4"], "the hyperlink cell joins the selection");
    assert!(h.state().dialog.is_none(), "no link was followed");
}

#[test]
fn ctrl_alt_click_on_the_link_text_still_extends_the_selection() {
    let mut h = harness();
    click_at(&mut h, "B2", 0.9, Modifiers::NONE);
    click_at(&mut h, "B4", 0.3, Modifiers { alt: true, ..Modifiers::COMMAND });
    let d = h.state().session.active().unwrap();
    let ranges: Vec<String> = d.selection.ranges.iter().map(|r| r.a1()).collect();
    assert_eq!(ranges, ["B2", "B4"], "Ctrl+Alt+click adds the hyperlink cell to the selection");
    assert_eq!(d.selection.active.a1(), "B4", "the link was not followed");
    assert!(h.state().dialog.is_none(), "no link dialog");
}

#[test]
fn managed_external_link_click_is_not_offered_but_internal_navigation_and_data_remain() {
    let mut h = harness();
    h.state_mut().set_managed_document_save(Some(Box::new(|| Ok(()))));
    let before = h.state().session.active().unwrap().wb.clone();
    click(&mut h, "B2", Modifiers::COMMAND);
    assert!(h.state().dialog.is_none(), "managed external link must not offer a blocked Open button");
    assert!(h.state().message.as_ref().is_some_and(|(_, text)| text.contains("External links")));
    h.state_mut().message = None;
    h.run_steps(3);
    click(&mut h, "B4", Modifiers::COMMAND);
    assert_eq!(h.state().session.active().unwrap().selection.active.a1(), "F9");
    assert!(std::sync::Arc::ptr_eq(&before, &h.state().session.active().unwrap().wb), "links and workbook content stay unchanged");
    assert_eq!(h.state().session.active().unwrap().wb.active().unwrap().hyperlinks[&CellRef::parse("B2").unwrap()].target, "https://example.org/a");
}

#[test]
fn managed_external_link_command_refuses_without_claiming_to_open_or_calling_host() {
    let mut h = harness();
    let opened = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let capture = opened.clone();
    h.state_mut().services.open_url = Some(Box::new(move |_| {
        capture.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    h.state_mut().run("link.open", json!({"url":"https://example.org/a"})).unwrap();
    assert_eq!(opened.load(std::sync::atomic::Ordering::SeqCst), 1, "standalone host service remains usable");
    h.state_mut().set_managed_document_save(Some(Box::new(|| Ok(()))));
    assert!(h.state_mut().run("link.open", json!({"url":"https://example.org/a"})).is_err());
    assert_eq!(opened.load(std::sync::atomic::Ordering::SeqCst), 1, "managed host must not receive an impossible popup request");
    h.state_mut().run("link.open", json!({"url":"Sheet1!F9"})).unwrap();
    assert_eq!(h.state().session.active().unwrap().selection.active.a1(), "F9");
}
