//! Key-driven and render tests: the TUI is exercised through the same `Service` the GUI uses.
use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use plan_burner_core::{notify, provider};
use ratatui::{backend::TestBackend, Terminal};
use std::sync::Arc;

struct Quiet;
impl notify::ExternalSink for Quiet {
    fn desktop(&self, _: &str, _: &str) {}
    fn telegram(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
}

fn service(dir: &std::path::Path, driver: bool) -> Service {
    let mut s = Service::new(
        dir.join("t.db"),
        provider::registry(),
        HashMap::new(),
        Arc::new(Quiet),
    )
    .unwrap();
    let lock = if driver {
        session::acquire_driver_lock(dir)
    } else {
        None
    };
    s.set_driver_lock(lock);
    s
}

fn app() -> (App, tempfile::TempDir) {
    let d = tempfile::tempdir().unwrap();
    let svc = service(d.path(), true);
    let st = plan_burner_core::settings::load(&svc.conn).unwrap();
    (App::with_service(svc, st), d)
}

fn k(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}
fn press(a: &mut App, keys: &str) {
    for c in keys.chars() {
        a.key(k(KeyCode::Char(c)));
    }
}
fn ctrl_s(a: &mut App) {
    a.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
}
fn text_of(a: &App) -> String {
    let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
    t.draw(|f| ui::draw(f, a)).unwrap();
    t.backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect()
}
fn status(a: &App) -> String {
    a.status.clone().map(|s| s.0).unwrap_or_default()
}

#[test]
fn every_view_renders_with_and_without_data() {
    let (mut a, _d) = app();
    for v in [
        View::Dashboard,
        View::Queue,
        View::Accounts,
        View::Approvals,
        View::History,
    ] {
        a.set_view(v);
        text_of(&a);
    }
    a.svc.create_account("claude", "Work", "Pro", None).unwrap();
    let mut f = TaskForm::new_task();
    f.t.name = "demo".into();
    f.t.prompt = "do it".into();
    a.svc.save_task(&f).unwrap();
    a.refresh();
    for v in [View::Dashboard, View::Queue, View::Accounts, View::History] {
        a.set_view(v);
        let t = text_of(&a);
        if v == View::Queue {
            assert!(t.contains("demo"));
        }
        if v == View::Accounts {
            assert!(t.contains("Work"));
        }
    }
    a.mode = Mode::Help;
    assert!(text_of(&a).contains("EMERGENCY STOP"));
}

#[test]
fn help_scrolls_on_a_small_terminal_and_only_escape_closes_it() {
    let (mut a, _d) = app();
    let draw = |a: &App| -> String {
        let mut t = Terminal::new(TestBackend::new(100, 14)).unwrap();
        t.draw(|f| ui::draw(f, a)).unwrap();
        t.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    };
    press(&mut a, "?");
    let top = draw(&a);
    assert!(top.contains("GLOBAL") && top.contains("more below"));
    assert!(!top.contains("RESULT VIEW"));
    // scrolling keys do not close it
    press(&mut a, "j");
    a.key(k(KeyCode::PageDown));
    assert!(matches!(a.mode, Mode::Help));
    a.key(k(KeyCode::End));
    let end = draw(&a);
    assert!(end.contains("billing blocks execution") && !end.contains("more below"));
    a.key(k(KeyCode::Home));
    assert!(draw(&a).contains("GLOBAL"));
    a.key(k(KeyCode::Esc));
    assert!(matches!(a.mode, Mode::Browse));
}

fn draw_at(a: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| ui::draw(f, a)).unwrap();
    t.backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect()
}

#[test]
fn a_rejected_save_marks_and_focuses_the_offending_field() {
    let (mut a, _d) = app();
    press(&mut a, "7");
    if let Mode::Edit(e) = &mut a.mode {
        e.form.focus = e
            .form
            .fields
            .iter()
            .position(|f| f.key == "reserved")
            .unwrap();
    }
    for _ in 0..4 {
        a.key(k(KeyCode::Backspace));
    }
    press(&mut a, "zz");
    // move away so the focus jump is observable, then save
    a.key(k(KeyCode::Tab));
    ctrl_s(&mut a);
    match &a.mode {
        Mode::Edit(e) => {
            let i = e.form.error_field.expect("the field is identified");
            assert_eq!(e.form.fields[i].key, "reserved");
            assert_eq!(e.form.focus, i, "focus jumps to it");
            assert!(e.form.error.as_ref().unwrap().contains("Reserved capacity"));
        }
        _ => panic!("the editor must stay open"),
    }
    let t = text_of(&a);
    assert!(t.contains("field ") && t.contains("Ctrl-S save"), "{t}");
}

#[test]
fn burn_keys_toggle_and_burn_now_asks_before_skipping_approval() {
    let (mut a, _d) = app();
    let acct =
        plan_burner_core::accounts::create(&a.svc.conn, "claude", "Work", "Pro", None).unwrap();
    a.refresh();
    a.set_view(View::Accounts);
    // account toggle: on, then off again (explicit off)
    press(&mut a, "b");
    assert!(plan_burner_core::present::burn_active(
        &a.settings,
        &plan_burner_core::accounts::get(&a.svc.conn, acct.id).unwrap()
    ));
    assert!(text_of(&a).contains("ON"), "Burn column shows ON");
    press(&mut a, "b");
    assert!(!plan_burner_core::present::burn_active(
        &a.settings,
        &plan_burner_core::accounts::get(&a.svc.conn, acct.id).unwrap()
    ));
    // general toggle
    press(&mut a, "B");
    assert_eq!(a.settings.burn_session_hours, Some(1.0));
    press(&mut a, "B");
    assert_eq!(a.settings.burn_session_hours, None);
    // without approvals needs a yes; anything else cancels
    press(&mut a, "W");
    assert!(matches!(a.mode, Mode::Confirm(Confirm::BurnNow(Some(_)))));
    assert!(text_of(&a).contains("WITHOUT approval"));
    press(&mut a, "n");
    assert!(matches!(a.mode, Mode::Browse));
    press(&mut a, "X");
    assert!(matches!(a.mode, Mode::Confirm(Confirm::BurnNow(None))));
    press(&mut a, "n");
    // with approvals needs no prompt; with nothing queued it reports that
    press(&mut a, "w");
    assert!(matches!(a.mode, Mode::Browse));
    assert!(a
        .status
        .as_ref()
        .is_some_and(|(m, ok)| m.contains("nothing to burn") && !ok));
}

#[test]
fn confirmations_name_their_target_and_cancel_is_neutral() {
    let (mut a, _d) = app();
    let acct =
        plan_burner_core::accounts::create(&a.svc.conn, "claude", "Work", "Pro", None).unwrap();
    let t = plan_burner_core::tasks::create(
        &a.svc.conn,
        &plan_burner_core::tasks::NewTask {
            name: "Refactor docs".into(),
            prompt: "x".into(),
            ..Default::default()
        },
        plan_burner_core::models::TaskStatus::Pending,
    )
    .unwrap();
    a.refresh();
    a.mode = Mode::Confirm(Confirm::DeleteTask(t.id));
    assert!(text_of(&a).contains("Refactor docs"));
    a.mode = Mode::Confirm(Confirm::DeleteAccount(acct.id));
    assert!(text_of(&a).contains("Work"));
    a.mode = Mode::Confirm(Confirm::StopRunning(t.id));
    let s = text_of(&a);
    assert!(
        s.contains("Stop the running") && s.contains("Refactor docs"),
        "{s}"
    );
    a.mode = Mode::Confirm(Confirm::RejectBatch { id: 1, tasks: 3 });
    assert!(text_of(&a).contains("3 task(s)"));
    press(&mut a, "n");
    assert_eq!(a.status, Some((MSG_CANCELLED.to_string(), true)));
}

#[test]
fn successes_fade_and_errors_clear_on_the_next_key() {
    let (mut a, _d) = app();
    a.status = Some(("Saved.".into(), true));
    a.age_status(); // starts the clock
    assert!(a.status.is_some());
    a.status_seen = Some((
        ("Saved.".into(), true),
        Instant::now() - Duration::from_secs(60),
    ));
    a.age_status();
    assert!(a.status.is_none(), "old successes fade");
    a.status = Some(("it broke".into(), false));
    a.age_status();
    a.status_seen = Some((
        ("it broke".into(), false),
        Instant::now() - Duration::from_secs(600),
    ));
    a.age_status();
    assert!(a.status.is_some(), "errors do not time out");
    a.key(k(KeyCode::Down));
    assert!(a.status.is_none(), "any key dismisses an error");
}

#[test]
fn long_errors_wrap_onto_two_rows() {
    let (mut a, _d) = app();
    a.status = Some((
        "E1 ".to_string() + &"very long reason ".repeat(8) + "END",
        false,
    ));
    let t = draw_at(&a, 80, 24);
    assert!(t.contains("E1") && t.contains("END"), "{t}");
}

#[test]
fn tiny_terminals_get_a_size_message_instead_of_a_broken_screen() {
    let (a, _d) = app();
    let t = draw_at(&a, 40, 8);
    assert!(
        t.contains("Terminal too small") && t.contains("need 60x12"),
        "{t}"
    );
    assert!(!draw_at(&a, 80, 24).contains("Terminal too small"));
}

#[test]
fn narrow_terminals_keep_the_view_keys_visible() {
    let (mut a, _d) = app();
    press(&mut a, "2"); // queue
    let t = draw_at(&a, 80, 24);
    // view-specific keys come first and the bar wraps instead of cutting them off
    assert!(t.contains("n new") && t.contains("f filter"), "{t}");
    assert!(
        t.contains("q quit"),
        "global keys still reachable on a second row: {t}"
    );
}

#[test]
fn settings_are_edited_validated_and_saved() {
    let (mut a, _d) = app();
    press(&mut a, "7");
    assert!(matches!(a.mode, Mode::Edit(_)));
    // focus: enabled(check) -> toggle it
    press(&mut a, " ");
    // jump to "reserved" (4th field after enabled, policy, approval, threshold)
    for _ in 0..4 {
        a.key(k(KeyCode::Tab));
    }
    for _ in 0..4 {
        a.key(k(KeyCode::Backspace));
    }
    press(&mut a, "130");
    ctrl_s(&mut a);
    assert!(
        matches!(a.mode, Mode::Edit(_)),
        "core validation keeps the form open"
    );
    assert!(!status(&a).is_empty() && !a.status.as_ref().unwrap().1);
    for _ in 0..3 {
        a.key(k(KeyCode::Backspace));
    }
    press(&mut a, "30");
    ctrl_s(&mut a);
    assert!(matches!(a.mode, Mode::Browse), "saved: {}", status(&a));
    let s = plan_burner_core::settings::load(&a.svc.conn).unwrap();
    assert!(s.enabled);
    assert_eq!(s.reserved_capacity, 30.0);
}

#[test]
fn leaving_a_dirty_form_asks_before_discarding() {
    let (mut a, _d) = app();
    press(&mut a, "7");
    press(&mut a, " ");
    a.key(k(KeyCode::Esc));
    assert!(matches!(&a.mode, Mode::Edit(e) if e.confirm_discard));
    press(&mut a, "n");
    assert!(matches!(&a.mode, Mode::Edit(e) if !e.confirm_discard));
    a.key(k(KeyCode::Esc));
    press(&mut a, "y");
    assert!(matches!(a.mode, Mode::Browse));
    assert!(
        !plan_burner_core::settings::load(&a.svc.conn)
            .unwrap()
            .enabled
    );
    // a clean form closes immediately
    press(&mut a, "7");
    a.key(k(KeyCode::Esc));
    assert!(matches!(a.mode, Mode::Browse));
}

#[test]
fn ctrl_c_does_not_discard_a_dirty_form_silently() {
    let ctrl_c = || KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let (mut a, _d) = app();
    // clean form or browse mode: quits at once
    press(&mut a, "7");
    a.key(ctrl_c());
    assert!(a.quit);
    // a dirty form asks first and keeps the edits
    let (mut a, _d) = app();
    press(&mut a, "7");
    press(&mut a, " ");
    a.key(ctrl_c());
    assert!(!a.quit);
    assert!(matches!(&a.mode, Mode::Edit(e) if e.confirm_discard && e.form.dirty));
    // answering n keeps editing, y discards the form without quitting
    press(&mut a, "n");
    assert!(matches!(&a.mode, Mode::Edit(e) if !e.confirm_discard && e.form.dirty));
    a.key(ctrl_c());
    press(&mut a, "y");
    assert!(matches!(a.mode, Mode::Browse) && !a.quit);
    // insisting (Ctrl-C twice) quits
    press(&mut a, "7");
    press(&mut a, " ");
    a.key(ctrl_c());
    a.key(ctrl_c());
    assert!(a.quit);
}

#[test]
fn adding_and_editing_an_account_with_limits() {
    let (mut a, _d) = app();
    a.set_view(View::Accounts);
    press(&mut a, "a");
    assert!(matches!(a.mode, Mode::Edit(_)));
    // provider is focused first; go to label
    a.key(k(KeyCode::Tab));
    ctrl_s(&mut a);
    assert!(
        matches!(a.mode, Mode::Edit(_)),
        "a label is mandatory: {}",
        status(&a)
    );
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    e.form.set_text("label", "Home");
    e.form.set_text("plan", "Max");
    // override the start threshold and ignore limits
    e.form.focus = e
        .form
        .fields
        .iter()
        .position(|f| f.key == "lim_start")
        .unwrap();
    press(&mut a, " ");
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    e.form.set_text("lim_start", "40");
    e.form.focus = e
        .form
        .fields
        .iter()
        .position(|f| f.key == "ignore_limits")
        .unwrap();
    press(&mut a, " ");
    ctrl_s(&mut a);
    assert!(matches!(a.mode, Mode::Browse), "saved: {}", status(&a));
    let acc = a.snap.accounts[0].clone();
    assert_eq!(acc.label, "Home");
    assert_eq!(acc.limits.start_threshold, Some(40.0));
    assert!(acc.limits.ignore_limits);
    // quick toggles on the account
    press(&mut a, "i");
    assert!(!a.snap.accounts[0].limits.ignore_limits);
    press(&mut a, "v");
    assert!(!a.snap.accounts[0].available);
    press(&mut a, "c");
    assert!(a.snap.accounts[0].subscription_confirmed);
    press(&mut a, "m");
    assert_ne!(
        a.snap.accounts[0].run_mode,
        plan_burner_core::models::RunMode::Inherit
    );
    // delete asks first
    press(&mut a, "D");
    assert!(matches!(a.mode, Mode::Confirm(Confirm::DeleteAccount(_))));
    press(&mut a, "n");
    assert_eq!(a.snap.accounts.len(), 1);
    press(&mut a, "D");
    press(&mut a, "y");
    assert!(a.snap.accounts.is_empty());
}

#[test]
fn creating_editing_and_deleting_a_task() {
    let (mut a, _d) = app();
    a.set_view(View::Queue);
    press(&mut a, "n");
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    e.form.set_text("name", "write docs");
    e.form.set_text("prompt", "line one\nline two");
    e.form.set_text("est", "12");
    ctrl_s(&mut a);
    assert!(matches!(a.mode, Mode::Browse), "saved: {}", status(&a));
    assert_eq!(a.snap.tasks.len(), 1);
    assert_eq!(a.snap.tasks[0].prompt, "line one\nline two");
    assert_eq!(a.snap.tasks[0].estimated_consumption, 12.0);
    // edit
    press(&mut a, "e");
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    assert_eq!(e.form.text("name"), "write docs");
    e.form.set_text("name", "write more docs");
    ctrl_s(&mut a);
    assert_eq!(a.snap.tasks[0].name, "write more docs");
    // bad number is reported inside the form
    press(&mut a, "e");
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    e.form.set_text("est", "lots");
    ctrl_s(&mut a);
    let Mode::Edit(e) = &a.mode else { panic!() };
    assert!(e
        .form
        .error
        .as_deref()
        .unwrap()
        .contains("Est. consumption"));
    a.key(k(KeyCode::Esc));
    // duplicate (y) adds a copy; remove it again before the rest
    press(&mut a, "y");
    assert_eq!(a.snap.tasks.len(), 2);
    assert_eq!(a.snap.tasks[1].name, "write more docs (copy)");
    a.sel = 1;
    press(&mut a, "d");
    press(&mut a, "y");
    assert_eq!(a.snap.tasks.len(), 1);
    a.sel = 0;
    // pause / resume / cancel / retry
    press(&mut a, "x");
    assert_eq!(a.snap.tasks[0].status, TaskStatus::Paused);
    press(&mut a, "x");
    assert_ne!(a.snap.tasks[0].status, TaskStatus::Paused);
    press(&mut a, "c");
    assert_eq!(a.snap.tasks[0].status, TaskStatus::Cancelled);
    press(&mut a, "t");
    assert_ne!(a.snap.tasks[0].status, TaskStatus::Cancelled);
    // detail pane toggles
    a.key(k(KeyCode::Enter));
    assert!(text_of(&a).contains("Prompt is stored locally"));
    // delete needs confirmation
    press(&mut a, "d");
    press(&mut a, "x");
    assert_eq!(a.snap.tasks.len(), 1, "any key but y cancels");
    press(&mut a, "d");
    press(&mut a, "y");
    assert!(a.snap.tasks.is_empty());
}

#[test]
fn queue_filter_cycles_through_accounts() {
    let (mut a, _d) = app();
    let acc = a.svc.create_account("claude", "A", "", None).unwrap();
    let mut f = TaskForm::new_task();
    f.t.name = "pinned".into();
    f.t.prompt = "p".into();
    f.pinned = Some(acc.id);
    a.svc.save_task(&f).unwrap();
    let mut g = TaskForm::new_task();
    g.t.name = "loose".into();
    g.t.prompt = "p".into();
    a.svc.save_task(&g).unwrap();
    a.set_view(View::Queue);
    assert_eq!(a.visible_tasks().len(), 2);
    press(&mut a, "f");
    assert_eq!(a.filter, Filter::Account(acc.id));
    assert_eq!(a.visible_tasks().len(), 1);
    press(&mut a, "f");
    assert_eq!(a.filter, Filter::AnyAccount);
    assert_eq!(a.visible_tasks()[0].name, "loose");
    press(&mut a, "f");
    assert_eq!(a.filter, Filter::All);
}

#[test]
fn manual_usage_entry_records_a_manual_snapshot() {
    let (mut a, _d) = app();
    let acc = a.svc.create_account("claude", "A", "", None).unwrap();
    a.refresh();
    press(&mut a, "U");
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    e.form.set_text("session", "70");
    e.form.set_text("weekly", "80");
    if let Some(f) = e.form.field_mut("session_known") {
        f.kind = crate::form::Kind::Check(true);
    }
    if let Some(f) = e.form.field_mut("weekly_known") {
        f.kind = crate::form::Kind::Check(true);
    }
    ctrl_s(&mut a);
    assert!(matches!(a.mode, Mode::Browse), "{}", status(&a));
    let u = usage::latest(&a.svc.conn, acc.id).unwrap().unwrap();
    assert_eq!(u.session_remaining, Some(70.0));
    assert_eq!(u.weekly_remaining, Some(80.0));
    assert_eq!(u.confidence, plan_burner_core::models::Confidence::Manual);
}

#[test]
fn stop_requires_confirmation() {
    let (mut a, _d) = app();
    press(&mut a, "S");
    assert!(matches!(a.mode, Mode::Confirm(Confirm::Stop)));
    press(&mut a, "n");
    assert_eq!(a.svc.run_state(chrono::Utc::now()), RunState::Waiting);
    press(&mut a, "S");
    press(&mut a, "y");
    assert_eq!(a.svc.run_state(chrono::Utc::now()), RunState::Stopped);
    press(&mut a, "C");
    assert_eq!(a.svc.run_state(chrono::Utc::now()), RunState::Waiting);
    press(&mut a, "p");
    assert_eq!(a.svc.run_state(chrono::Utc::now()), RunState::Paused);
    press(&mut a, "p");
    assert_eq!(a.svc.run_state(chrono::Utc::now()), RunState::Waiting);
}

#[test]
fn automation_toggle_and_pause_until_without_reset() {
    let (mut a, _d) = app();
    press(&mut a, "E");
    assert!(a.settings.enabled);
    press(&mut a, "E");
    assert!(!a.settings.enabled);
    press(&mut a, "T");
    assert!(status(&a).contains("no upcoming usage reset"));
}

#[test]
fn history_actual_usage_and_audit_toggle() {
    let (mut a, _d) = app();
    a.set_view(View::History);
    press(&mut a, "a");
    assert!(matches!(a.mode, Mode::Browse), "nothing to record yet");
    press(&mut a, "A");
    assert!(a.hist_audit);
    assert!(text_of(&a).contains("audit log"));
    press(&mut a, "A");
    assert!(!a.hist_audit);
    // export writes both files
    let dir = tempfile::tempdir().unwrap();
    a.settings.results_dir = dir.path().join("out").to_string_lossy().into_owned();
    press(&mut a, "e");
    assert!(dir.path().join("out/history.csv").exists());
    assert!(dir.path().join("out/history.json").exists());
}

#[test]
fn result_viewer_scrolls_and_exports() {
    let (mut a, d) = app();
    let p = d.path().join("res.txt");
    std::fs::write(&p, "a\nb\nc\n").unwrap();
    a.view_result("task one", p.to_str().unwrap());
    assert!(text_of(&a).contains("untrusted"));
    press(&mut a, "j");
    assert!(matches!(&a.mode, Mode::Text(t) if t.scroll == 1));
    a.settings.results_dir = d.path().join("exp").to_string_lossy().into_owned();
    press(&mut a, "e");
    assert!(status(&a).starts_with("Exported to"));
    a.key(k(KeyCode::Esc));
    assert!(matches!(a.mode, Mode::Browse));
}

#[test]
fn read_only_instance_cannot_write_directly() {
    let d = tempfile::tempdir().unwrap();
    let _driver_lock = session::acquire_driver_lock(d.path());
    let svc = service(d.path(), false);
    let st = plan_burner_core::settings::load(&svc.conn).unwrap();
    let mut a = App::with_service(svc, st);
    assert!(a.svc.read_only);
    a.set_view(View::Queue);
    press(&mut a, "n");
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    e.form.set_text("name", "x");
    e.form.set_text("prompt", "y");
    ctrl_s(&mut a);
    // No driver answers on the socket: the write is refused, never done behind its back.
    assert!(a.snap.tasks.is_empty());
    assert!(!a.status.as_ref().unwrap().1, "{}", status(&a));
}

#[test]
fn base64_matches_known_vectors() {
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
    assert_eq!(base64(b"hello world"), "aGVsbG8gd29ybGQ=");
}

const ACCTS: [(i64, &str); 3] = [(1, "claude"), (2, "claude"), (3, "custom")];
fn cands(pinned: Option<i64>, provider: Option<&str>) -> Vec<i64> {
    run_now_candidates(ACCTS.iter().copied(), pinned, provider)
}

#[test]
fn run_now_never_auto_picks_between_several_accounts() {
    assert_eq!(cands(None, None).len(), 3);
    assert_eq!(cands(None, Some("claude")), vec![1, 2]);
}

#[test]
fn run_now_picks_only_an_unambiguous_account() {
    assert_eq!(cands(Some(2), Some("claude")), vec![2]);
    assert_eq!(cands(Some(3), None), vec![3]);
    assert_eq!(cands(None, Some("custom")), vec![3]);
}

#[test]
fn run_now_with_no_match_is_empty() {
    assert!(cands(Some(9), None).is_empty());
    assert!(cands(Some(1), Some("custom")).is_empty());
    assert!(cands(None, Some("other")).is_empty());
}

#[test]
fn ambiguous_run_now_opens_the_account_picker() {
    let (mut a, _d) = app();
    a.svc.create_account("claude", "A", "", None).unwrap();
    a.svc.create_account("claude", "B", "", None).unwrap();
    let mut f = TaskForm::new_task();
    f.t.name = "t".into();
    f.t.prompt = "p".into();
    a.svc.save_task(&f).unwrap();
    a.set_view(View::Queue);
    press(&mut a, "r");
    assert!(matches!(&a.mode, Mode::Pick { accounts, .. } if accounts.len() == 2));
    assert!(text_of(&a).contains("Run now on"));
    a.key(k(KeyCode::Esc));
    assert!(matches!(a.mode, Mode::Browse));
}

#[test]
fn templates_can_be_created_edited_duplicated_and_deleted() {
    let (mut a, _d) = app();
    press(&mut a, "6");
    assert_eq!(a.view, View::Templates);
    assert!(text_of(&a).contains("No templates yet"));
    press(&mut a, "n");
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    e.form.set_text("tpl_name", "review");
    e.form.set_text("prompt", "review the diff");
    e.form.set_text("retries", "2");
    ctrl_s(&mut a);
    assert!(matches!(a.mode, Mode::Browse), "saved: {}", status(&a));
    assert_eq!(a.templates.len(), 1);
    assert_eq!(a.templates[0].task.retry_max, 2);
    assert!(text_of(&a).contains("review"));
    // a second template cannot reuse the name
    press(&mut a, "n");
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    e.form.set_text("tpl_name", "review");
    e.form.set_text("prompt", "x");
    ctrl_s(&mut a);
    assert!(
        matches!(a.mode, Mode::Edit(_)),
        "name conflict keeps the form open"
    );
    assert!(status(&a).contains("already exists"));
    a.key(k(KeyCode::Esc)); // set_text does not mark the form dirty, so it closes at once
    assert!(matches!(a.mode, Mode::Browse));
    // rename
    press(&mut a, "e");
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    assert_eq!(e.form.text("tpl_name"), "review");
    e.form.set_text("tpl_name", "code review");
    ctrl_s(&mut a);
    assert_eq!(a.templates[0].name, "code review");
    // duplicate, then delete with confirmation
    press(&mut a, "y");
    assert_eq!(a.templates.len(), 2);
    press(&mut a, "D");
    assert!(matches!(a.mode, Mode::Confirm(Confirm::DeleteTemplate(_))));
    press(&mut a, "n");
    assert_eq!(a.templates.len(), 2);
    press(&mut a, "D");
    press(&mut a, "y");
    assert_eq!(a.templates.len(), 1);
}

#[test]
fn load_template_field_fills_the_task_form_and_is_visible_when_empty() {
    let (mut a, _d) = app();
    a.set_view(View::Queue);
    press(&mut a, "n");
    assert!(
        text_of(&a).contains("Load template"),
        "visible even with no templates"
    );
    a.key(k(KeyCode::Esc));
    // create a template, then load it from the task form
    let mut tf = TemplateForm::new();
    tf.name = "daily".into();
    tf.task.t.prompt = "summarise the day".into();
    tf.task.t.priority = 2;
    tf.task.budget_on = true;
    tf.task.budget = 7.0;
    a.svc.save_template_form(&tf).unwrap();
    a.refresh();
    press(&mut a, "n");
    let Mode::Edit(e) = &a.mode else { panic!() };
    assert_eq!(e.form.focus, 0, "the template field is first");
    a.key(k(KeyCode::Right));
    let Mode::Edit(e) = &a.mode else { panic!() };
    assert_eq!(e.form.text("prompt"), "summarise the day");
    assert_eq!(e.form.text("name"), "daily");
    assert_eq!(e.form.choice_idx("priority"), 2);
    assert!(e.form.checked("budget_on"));
    assert!(status(&a).contains("loaded"));
    ctrl_s(&mut a);
    assert!(matches!(a.mode, Mode::Browse), "{}", status(&a));
    assert_eq!(a.snap.tasks[0].priority, 2);
    assert_eq!(a.snap.tasks[0].maximum_budget, Some(7.0));
}

#[test]
fn save_as_template_from_a_task_form_and_new_task_from_template() {
    let (mut a, _d) = app();
    a.set_view(View::Queue);
    press(&mut a, "n");
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    e.form.set_text("name", "nightly");
    e.form.set_text("prompt", "run the nightly job");
    a.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert_eq!(a.templates.len(), 1, "{}", status(&a));
    assert_eq!(a.templates[0].name, "nightly");
    a.key(k(KeyCode::Esc));
    press(&mut a, "y");
    // start a task from the template on the Templates page
    press(&mut a, "6");
    press(&mut a, "t");
    let Mode::Edit(e) = &a.mode else { panic!() };
    assert_eq!(e.form.text("prompt"), "run the nightly job");
    assert_eq!(a.view, View::Queue);
}

#[test]
fn importing_a_file_queues_pending_tasks_and_shows_the_report() {
    let (mut a, d) = app();
    let f = d.path().join("tasks.md");
    std::fs::write(&f, "- lint: run clippy\n- docs: update README\n").unwrap();
    a.set_view(View::Queue);
    // A dry run creates nothing but still reports.
    press(&mut a, "i");
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    e.form.set_text("path", f.to_str().unwrap());
    e.form.set_checked("dry_run", true);
    ctrl_s(&mut a);
    assert!(matches!(a.mode, Mode::Text(_)), "{}", status(&a));
    assert!(a.snap.tasks.is_empty());
    press(&mut a, "q");
    // The real import.
    press(&mut a, "i");
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    e.form.set_text("path", f.to_str().unwrap());
    ctrl_s(&mut a);
    let Mode::Text(t) = &a.mode else { panic!() };
    assert!(t.body.contains("imported 2 task(s)"), "{}", t.body);
    assert_eq!(a.snap.tasks.len(), 2);
    assert!(a.snap.tasks.iter().all(|t| t.requires_approval));
    // A missing file is reported inside the form, which stays open.
    press(&mut a, "q");
    press(&mut a, "i");
    let Mode::Edit(e) = &mut a.mode else { panic!() };
    e.form.set_text("path", "/nonexistent/x.json");
    ctrl_s(&mut a);
    let Mode::Edit(e) = &a.mode else { panic!() };
    assert!(e.form.error.is_some());
}
