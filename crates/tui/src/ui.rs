//! Rendering only. Texts and severities come from `core::present` so they match the GUI.
use crate::{App, Filter, Mode, View};
use plan_burner_core::{
    control::RunState,
    models::{Account, BillingMode, TaskStatus},
    present::{self, Level},
    service::SchedulerStatus,
    usage,
};
use ratatui::{prelude::*, widgets::*};

fn level_color(l: Level) -> Color {
    match l {
        Level::Good => Color::Green,
        Level::Warn => Color::Yellow,
        Level::Bad => Color::Red,
        Level::Muted => Color::DarkGray,
    }
}

fn account_name(app: &App, id: i64) -> String {
    app.snap
        .accounts
        .iter()
        .find(|a| a.id == id)
        .map_or(format!("#{id}"), |a| a.display())
}

/// "Session  80% ████████████████░░░░  resets in 2 h 5 min"
fn meter<'a>(label: &str, v: Option<f64>, reset: Option<String>) -> Line<'a> {
    match v {
        None => Line::from(vec![
            format!("   {label:<8}").into(),
            "unknown (treated as unavailable)".yellow(),
        ]),
        Some(p) => {
            let filled = ((p.clamp(0.0, 100.0)) / 5.0).round() as usize;
            let color = if p < 20.0 {
                Color::Red
            } else if p < 50.0 {
                Color::Yellow
            } else {
                Color::Green
            };
            Line::from(vec![
                format!("   {label:<8}{p:>4.0}% ").into(),
                Span::styled("█".repeat(filled), Style::new().fg(color)),
                Span::styled("░".repeat(20 - filled), Style::new().fg(Color::DarkGray)),
                reset.map_or(Span::raw(""), |r| format!("  resets in {r}").into()),
            ])
        }
    }
}

/// Smallest terminal the layout works in (tabs 3 + body 5 + status 1 + hints 1 = 10, plus margin).
pub const MIN_SIZE: (u16, u16) = (60, 12);

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width < MIN_SIZE.0 || area.height < MIN_SIZE.1 {
        f.render_widget(
            Paragraph::new(format!(
                "Terminal too small: need {}x{}, have {}x{}. Enlarge the window.",
                MIN_SIZE.0, MIN_SIZE.1, area.width, area.height
            ))
            .wrap(Wrap { trim: true })
            .style(Style::new().fg(Color::Yellow)),
            area,
        );
        return;
    }
    // View keys first (they matter most); the hint bar takes a second row when it would not fit.
    let hint_len = hint_text(app).chars().count() as u16;
    let hint_rows = if hint_len > area.width && area.height >= 16 {
        2
    } else {
        1
    };
    // A long message (typically an error) gets a second row instead of being cut off.
    let (msg, style) = status_line(app);
    let msg = clip_to_rows(&msg, area.width, 2);
    let status_rows = if msg.chars().count() as u16 > area.width && area.height >= 16 {
        2
    } else {
        1
    };
    let [tabs, body, status, help] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(status_rows),
        Constraint::Length(hint_rows),
    ])
    .areas(area);

    let now = chrono::Utc::now();
    let (label, color) = match app.svc.run_state(now) {
        RunState::Running => ("RUNNING", Color::Green),
        RunState::Paused => ("PAUSED", Color::Yellow),
        RunState::Waiting => ("WAITING", Color::Cyan),
        RunState::Stopped => ("STOPPED", Color::Red),
    };
    let mut titles: Vec<Line> = View::ALL
        .iter()
        .enumerate()
        .map(|(i, v)| Line::from(format!("{} {}", i + 1, v.title())))
        .collect();
    let sched = app.svc.scheduler_status();
    if sched != SchedulerStatus::Running {
        titles.push(Line::from(format!("[{}]", sched.label()).red()));
    }
    let policy = format!(
        " automation {} · policy {}{} ",
        if app.settings.enabled { "ON" } else { "off" },
        app.settings.execution_mode,
        app.qstatus
            .global
            .as_ref()
            .map_or(String::new(), |g| format!(" · holding: {g}"))
    );
    f.render_widget(
        Tabs::new(titles)
            .select(View::ALL.iter().position(|v| *v == app.view).unwrap_or(0))
            .highlight_style(Style::new().bold().fg(Color::Cyan))
            .block(
                Block::bordered()
                    .title(format!(" {} ", plan_burner_core::brand::NAME))
                    .title_top(Line::from(format!(" {label} ").fg(color).bold()).right_aligned())
                    .title_bottom(Line::from(policy.fg(Color::DarkGray)).right_aligned()),
            ),
        tabs,
    );

    match &app.mode {
        Mode::Edit(e) => e.form.render(f, body),
        Mode::Text(t) => f.render_widget(
            Paragraph::new(t.body.as_str())
                .block(Block::bordered().title(format!(
                    " {} – AI output is untrusted text; review before using it ",
                    t.title
                )))
                .scroll((t.scroll, 0))
                .wrap(Wrap { trim: false }),
            body,
        ),
        Mode::Help => help_page(f, body, app),
        Mode::Pick { accounts, idx, .. } => {
            let task = match &app.mode {
                Mode::Pick { task, .. } => *task,
                _ => 0,
            };
            let items: Vec<ListItem> = accounts
                .iter()
                .map(|id| {
                    let why = app.svc.run_now_blockers(task, *id).unwrap_or_default();
                    match why.first() {
                        None => ListItem::new(account_name(app, *id)),
                        Some(first) => ListItem::new(format!(
                            "{}  - not eligible: {first}",
                            account_name(app, *id)
                        ))
                        .style(Style::new().fg(Color::DarkGray)),
                    }
                })
                .collect();
            let mut st = ListState::default().with_selected(Some(*idx));
            f.render_stateful_widget(
                List::new(items)
                    .block(Block::bordered().title(" Run now on… (Enter run, Esc cancel) "))
                    .highlight_style(Style::new().reversed()),
                body,
                &mut st,
            );
        }
        _ => match app.view {
            View::Dashboard => dashboard(f, body, app),
            View::Queue => queue(f, body, app),
            View::Accounts => accounts(f, body, app),
            View::Approvals => approvals(f, body, app),
            View::History => history(f, body, app),
            View::Templates => templates(f, body, app),
            View::Settings => {}
        },
    }

    f.render_widget(
        Paragraph::new(msg).style(style).wrap(Wrap { trim: true }),
        status,
    );

    f.render_widget(
        Paragraph::new(hint_text(app))
            .wrap(Wrap { trim: true })
            .style(Style::new().fg(Color::DarkGray)),
        help,
    );
}

/// The question for a pending confirmation, naming what it would affect.
fn confirm_text(app: &App, c: &crate::Confirm) -> String {
    use crate::Confirm as C;
    let task = |id: i64| {
        app.snap.tasks.iter().find(|t| t.id == id).map_or_else(
            || format!("task #{id}"),
            |t| format!("task #{id} \"{}\"", t.name),
        )
    };
    let tail = "Press y to confirm, any other key to cancel.";
    match c {
        C::Stop => {
            format!("EMERGENCY STOP? This kills the running task and halts automation. {tail}")
        }
        C::BurnNow(None) => format!(
            "Burn now WITHOUT approval on the first eligible account? Thresholds and reserve are ignored for it. {tail}"
        ),
        C::BurnNow(Some(id)) => {
            let name = app
                .snap
                .accounts
                .iter()
                .find(|a| a.id == *id)
                .map_or_else(|| format!("account #{id}"), |a| a.display());
            format!(
                "Burn now WITHOUT approval on \"{name}\"? Thresholds and reserve are ignored for this batch. {tail}"
            )
        }
        C::StopRunning(id) => format!(
            "Stop the running {}? Its process is killed. {tail}",
            task(*id)
        ),
        C::RejectBatch { tasks, .. } => format!(
            "Reject this proposed batch of {tasks} task(s)? They return to the queue. {tail}"
        ),
        C::DeleteTask(id) => format!("Delete {}? {tail}", task(*id)),
        C::DeleteAccount(id) => {
            let name = app
                .snap
                .accounts
                .iter()
                .find(|a| a.id == *id)
                .map_or_else(|| format!("account #{id}"), |a| a.display());
            let pinned = app
                .snap
                .tasks
                .iter()
                .filter(|t| t.account_id == Some(*id) && !t.status.is_terminal())
                .count();
            let effect = if pinned > 0 {
                format!("It will be refused while {pinned} open task(s) are pinned to it.")
            } else {
                "Its usage readings are deleted; run history is kept.".to_string()
            };
            format!("Delete account \"{name}\"? {effect} {tail}")
        }
        C::DeleteTemplate(id) => {
            let name = app.templates.iter().find(|t| t.id == *id).map_or_else(
                || format!("template #{id}"),
                |t| format!("template \"{}\"", t.name),
            );
            format!("Delete {name}? {tail}")
        }
    }
}

/// Keeps at most `rows` lines of `width` columns, ending in an ellipsis when text was dropped.
fn clip_to_rows(msg: &str, width: u16, rows: u16) -> String {
    let max = (width as usize * rows as usize).saturating_sub(1);
    if msg.chars().count() <= max.max(1) {
        return msg.to_string();
    }
    let mut t: String = msg.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// The message row: a pending confirmation, else the latest status.
fn status_line(app: &App) -> (String, Style) {
    match &app.mode {
        Mode::Confirm(c) => (confirm_text(app, c), Style::new().fg(Color::Red).bold()),
        Mode::Edit(e) if e.confirm_discard => (
            "Discard unsaved changes? Press y to discard, any other key to keep editing.".into(),
            Style::new().fg(Color::Yellow).bold(),
        ),
        _ => match &app.status {
            Some((m, _)) if m == crate::MSG_CANCELLED => {
                (m.clone(), Style::new().fg(Color::DarkGray))
            }
            Some((m, ok)) => (
                m.clone(),
                Style::new().fg(if *ok { Color::Green } else { Color::Red }),
            ),
            None => (String::new(), Style::new()),
        },
    }
}

/// The key hints for the current mode/view: view-specific keys first, then the global ones.
fn hint_text(app: &App) -> String {
    let keys = match (&app.mode, app.view) {
        (Mode::Edit(_), _) => "Tab/Shift-Tab field  Space toggle  ←/→ choose  Ctrl-S save  Esc cancel  Ctrl-X inherit".to_string(),
        (Mode::Text(_), _) => "j/k PgUp/PgDn scroll  y copy  e export  Esc close".to_string(),
        (Mode::Help, _) => "j/k PgUp/PgDn scroll, Esc/q/? closes help".to_string(),
        (Mode::Pick { .. }, _) => "j/k choose  Enter run  Esc cancel".to_string(),
        (_, View::Queue) => "n new  i import  e edit  Enter detail  r run now  x pause  c cancel  t retry  y duplicate  d delete  J/K move  v result  f filter".to_string(),
        (_, View::Accounts) | (_, View::Dashboard) => "a add  e edit  D delete  g propose  u refresh  U usage  i ignore-limits  b burn-before-reset  w/W burn now  v available  c confirm-sub  m mode  P policy".to_string(),
        (_, View::Approvals) => "[ ] task  Space toggle  Enter approve selected  a approve all  x reject".to_string(),
        (_, View::History) => "a record actual  v result  e export  A audit log".to_string(),
        (_, View::Templates) => "n new  e edit  y duplicate  D delete  t new task from template  Q queue now".to_string(),
        _ => String::new(),
    };
    if matches!(app.mode, Mode::Browse) {
        let global = "? help  q quit  Tab/1-7 view  j/k move  p pause  T pause-to-reset  E enable  S STOP  C clear  R start scheduler";
        if keys.is_empty() {
            global.to_string()
        } else {
            format!("{keys}  |  {global}")
        }
    } else {
        keys
    }
}

fn table<'a>(
    rows: Vec<Row<'a>>,
    widths: &[Constraint],
    header: &[&'a str],
    title: String,
    sel: usize,
) -> impl FnOnce(&mut Frame, Rect) + 'a {
    let widths = widths.to_vec();
    let header = Row::new(header.iter().map(|h| Cell::from(*h))).style(Style::new().bold());
    move |f, area| {
        let mut st = TableState::default().with_selected(Some(sel));
        let t = Table::new(rows, widths)
            .header(header)
            .block(Block::bordered().title(title))
            .row_highlight_style(Style::new().reversed());
        f.render_stateful_widget(t, area, &mut st);
    }
}

/// Shown by `?`; scrolls with j/k, PgUp/PgDn.
pub const HELP_TEXT: &str = "\
GLOBAL
  Tab / Shift-Tab, ← →, 1-7   switch view (7 = Settings editor)
  j k / ↑ ↓                  move selection
  p pause/resume   T pause until next reset   E enable/disable automation
  B burn before reset on/off (general)   O burn now, all accounts, with approvals
  X burn now, all accounts, WITHOUT approvals (asks y)
  S EMERGENCY STOP (asks y)   C clear stop   R start the scheduler   ? this help   q / Ctrl-C quit

DASHBOARD / ACCOUNTS  (act on the selected account)
  a add   e edit   D delete (asks y)   g propose a batch   u refresh usage
  U enter usage manually   i toggle eligible-ignoring-limits
  b burn before reset on/off (this account)   w burn now with approvals
  W burn now WITHOUT approvals (asks y)
  v toggle available   c toggle subscription confirmed
  m cycle run mode   P cycle execution policy   f filter the queue by it

QUEUE
  n new   i import from JSON/Markdown file   e edit   Enter detail   x pause/resume   c cancel (stops a running task)
  t retry   y duplicate   d delete (asks y)   J / K move down/up   r run now (choose account)
  v view result   f cycle filter (all / each account / any account)

TEMPLATES (6)
  n new   e / Enter edit (rename too)   y duplicate   D delete (asks y)
  t start a new task from the template   Q queue it now   (task form: ←/→ on “Load template”)

APPROVALS
  [ ] choose task   Space include/exclude   Enter approve selected
  a approve the whole batch (only when approval is 'once per batch')   x reject

HISTORY
  a record measured usage   v view result   e export CSV + JSON   A audit log

FORMS (accounts, tasks, usage, settings)
  Tab / Shift-Tab fields   Space toggles / overrides   ← → chooses   Ctrl-S save   Esc cancel
  Ctrl-X returns an override to 'inherit'   paste works
  Account: Ctrl-D discover Claude logins
  Task: “Load template” field (←/→)   Ctrl-L suggest estimate   Ctrl-O save as template

RESULT VIEW
  j k PgUp PgDn scroll   y copy (terminal clipboard)   e export to the results folder   Esc close

Subscription safety: unknown or stale usage never counts as available; unverified or API
billing blocks execution; \"ignore limits\" never bypasses these checks.";

fn help_page(f: &mut Frame, area: Rect, app: &App) {
    let view_h = area.height.saturating_sub(2);
    app.help_height.set(view_h);
    let lines = HELP_TEXT.lines().count() as u16;
    let scroll = app.help_scroll.min(lines.saturating_sub(view_h));
    let more = if scroll + view_h < lines {
        " · more below ▼"
    } else {
        ""
    };
    f.render_widget(
        Paragraph::new(HELP_TEXT).scroll((scroll, 0)).block(
            Block::bordered().title(format!(" Help (j/k PgUp/PgDn scroll, Esc closes){more} ")),
        ),
        area,
    );
}

// ---------------------------------------------------------------- views

fn account_lines<'a>(app: &App, a: &Account, selected: bool) -> Vec<Line<'a>> {
    let now = chrono::Utc::now();
    let det = app.snap.dets.get(&a.id);
    let billing = det.map_or(BillingMode::Unverified, |d| d.billing);
    let mark = if selected { "▶ " } else { "  " };
    let mut out = vec![Line::from(vec![
        Span::styled(
            format!("{mark}{}", a.display()),
            Style::new()
                .bold()
                .fg(if selected { Color::Cyan } else { Color::Reset }),
        ),
        format!(
            "  {}  ",
            if a.subscription_plan.is_empty() {
                "plan not set"
            } else {
                &a.subscription_plan
            }
        )
        .into(),
        Span::styled(
            present::billing_label(billing),
            Style::new().fg(level_color(present::billing_level(billing))),
        ),
    ])];
    if let Some(d) = det {
        if !d.cli_found {
            out.push("   CLI/script not available".red().into());
        }
        for r in &d.reasons {
            out.push(Line::from(format!("   {r}")).yellow());
        }
    }
    let u = app.usage.get(&a.id);
    let reset = |t| usage::minutes_until(t, now).map(present::fmt_minutes);
    out.push(meter(
        "Session",
        u.and_then(|u| u.session_remaining),
        u.and_then(|u| reset(u.session_reset_at)),
    ));
    out.push(meter(
        "Weekly",
        u.and_then(|u| u.weekly_remaining),
        u.and_then(|u| reset(u.weekly_reset_at)),
    ));
    if a.polls() {
        if let Some(e) = app.svc.usage_errors.lock().unwrap().get(&a.id) {
            out.push(Line::from(format!("   Automatic usage failed: {e}")).red());
        }
    }
    match u {
        Some(u) => {
            let (age, stale) =
                present::reading_age(u, now, app.settings.effective_for(a).max_usage_age_minutes);
            out.push(
                Line::from(format!(
                    "   {} · {} min old{}   {}",
                    u.confidence,
                    age,
                    if stale { " · STALE: blocked" } else { "" },
                    present::provenance(u)
                ))
                .fg(if stale { Color::Red } else { Color::Yellow }),
            );
        }
        None => out.push("   No usage data: blocked".red().into()),
    }
    let pinned = app
        .snap
        .tasks
        .iter()
        .filter(|t| !t.status.is_terminal() && t.account_id == Some(a.id))
        .count();
    out.push(Line::from(format!(
        "   {pinned} pinned task(s) · policy {} · mode {} · subscription {} · available {}{}",
        a.execution_policy,
        a.run_mode,
        if a.subscription_confirmed {
            "confirmed"
        } else {
            "NOT confirmed"
        },
        if a.available { "yes" } else { "no" },
        if a.limits.ignore_limits {
            " · LIMITS IGNORED"
        } else {
            ""
        },
    )));
    if plan_burner_core::present::burn_active(&app.settings, a) {
        let e = app.settings.effective_for(a);
        out.push(Line::from(format!(
            "  burn before reset: ON (5 h window last {} h, weekly last {} h)",
            e.burn_session_hours.unwrap_or(0.0),
            e.burn_weekly_hours.unwrap_or(0.0)
        )));
    }
    out
}

fn dashboard(f: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = vec![];
    if app.settings.working_root.is_empty() {
        lines.push(
            Line::from("No allowed working directory is configured (Settings): tasks cannot run.")
                .yellow(),
        );
    }
    if !app.settings.enabled
        || app.settings.execution_mode != plan_burner_core::models::ExecutionPolicy::Automatic
    {
        lines.push(
            Line::from(
                "Automatic execution is off (E toggles automation; policy is set in Settings).",
            )
            .yellow(),
        );
    }
    if app.snap.accounts.is_empty() {
        lines.push("No accounts yet. Press a to add one.".into());
    }
    let mut sel_line = 0;
    for (i, a) in app.snap.accounts.iter().enumerate() {
        if i == app.sel {
            sel_line = lines.len();
        }
        lines.extend(account_lines(app, a, i == app.sel));
        lines.push("".into());
    }
    let q = present::queue_counts(&app.snap.accounts, &app.snap.tasks);
    lines.push(Line::from("Queue by target").bold());
    for (id, n) in &q.pinned {
        lines.push(format!("   {} · {n} pinned", account_name(app, *id)).into());
    }
    lines.push(format!("   Any account of a provider · {}", q.any_account).into());
    lines.push(format!("   Any provider · {}", q.any_provider).into());
    lines.push(
        match &q.running {
            Some(n) => format!("   Running now: {n}"),
            None => "   Running now: nothing. One task at a time across all accounts.".into(),
        }
        .into(),
    );
    lines.push(format!("   {} batch(es) awaiting approval", app.pending().len()).into());
    lines.push("".into());
    lines.push(Line::from("Unused capacity (estimate)").bold());
    for a in &app.snap.accounts {
        lines.push(
            format!(
                "   {}: {}",
                a.label,
                app.forecasts.get(&a.id).map_or("…", String::as_str)
            )
            .into(),
        );
    }
    lines.push(Line::from("   Estimates only; not provider measurements.").dark_gray());
    lines.push("".into());
    lines.push(Line::from("Notifications").bold());
    if app.notifs.is_empty() {
        lines.push(Line::from("   none yet").dark_gray());
    }
    for n in &app.notifs {
        lines.push(format!("   {}", n.message).into());
    }
    if let Some(s) = app.svc.stop_report().as_ref() {
        lines.push("".into());
        lines.push(
            Line::from(format!(
                "Last STOP: {} confirmed={:?}",
                s.at.format("%H:%M:%S"),
                s.process_confirmed
            ))
            .red(),
        );
    }
    let scroll = (sel_line as u16)
        .saturating_sub(1)
        .min((lines.len() as u16).saturating_sub(area.height.saturating_sub(2)));
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" Dashboard "))
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn filter_label(app: &App) -> String {
    match app.filter {
        Filter::All => "all".into(),
        Filter::Account(id) => account_name(app, id),
        Filter::AnyAccount => "any account".into(),
    }
}

fn queue(f: &mut Frame, area: Rect, app: &App) {
    let [area, why_row] = Layout::vertical([Constraint::Min(4), Constraint::Length(1)]).areas(area);
    let vis = app.visible_tasks();
    let why = vis
        .get(app.sel)
        .and_then(|t| app.qstatus.reasons.get(&t.id))
        .map_or_else(
            || app.qstatus.global.clone().unwrap_or_default(),
            |r| format!("Why not running: {r}"),
        );
    f.render_widget(Paragraph::new(why).fg(Color::Yellow), why_row);
    let (top, bottom) = if app.detail && !vis.is_empty() {
        let [a, b] = Layout::vertical([Constraint::Min(6), Constraint::Length(11)]).areas(area);
        (a, Some(b))
    } else {
        (area, None)
    };
    let rows = vis
        .iter()
        .map(|t| {
            let target = t.account_id.map_or_else(
                || t.provider_type.clone().unwrap_or_else(|| "any".into()),
                |id| account_name(app, id),
            );
            let st = match t.status {
                TaskStatus::Completed => Style::new().fg(Color::Green),
                TaskStatus::Failed => Style::new().fg(Color::Red),
                TaskStatus::AwaitingApproval => Style::new().fg(Color::Yellow),
                TaskStatus::Running | TaskStatus::WaitingForCapacity => {
                    Style::new().fg(Color::Cyan)
                }
                _ => Style::new(),
            };
            Row::new(vec![
                Cell::from(t.id.to_string()),
                Cell::from(t.name.clone()),
                Cell::from(t.status.label().to_string()).style(st),
                Cell::from(present::priority_label(t.priority)),
                Cell::from(format!("~{:.0}%", t.estimated_consumption)),
                Cell::from(target),
                Cell::from(if t.model.is_empty() {
                    "default model".to_string()
                } else {
                    t.model.clone()
                }),
            ])
        })
        .collect();
    table(
        rows,
        &[
            Constraint::Length(5),
            Constraint::Min(20),
            Constraint::Length(22),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Min(16),
            Constraint::Min(12),
        ],
        &["ID", "Name", "Status", "Prio", "Est.", "Target", "Model"],
        format!(
            " Queue · filter: {} (estimates, not measurements) ",
            filter_label(app)
        ),
        app.sel,
    )(f, top);
    if let (Some(b), Some(t)) = (bottom, vis.get(app.sel)) {
        let deps = plan_burner_core::tasks::dependencies(&app.svc.conn, t.id).unwrap_or_default();
        let deps = present::dependency_names(&app.snap.tasks, &deps);
        let lines = vec![
            Line::from(format!("Prompt: {}", t.prompt.lines().next().unwrap_or(""))),
            Line::from("Prompt is stored locally and never sent in notifications.").dark_gray(),
            Line::from(format!(
                "timeout {} min · retries {}/{} · budget {} · approval {} · run mode {} · priority {}",
                t.timeout_secs / 60,
                t.retry_count,
                t.retry_max,
                t.maximum_budget.map_or("none".into(), |b| format!("{b:.0}%")),
                if t.requires_approval { "required" } else { "policy" },
                t.run_mode,
                present::priority_label(t.priority)
            )),
            Line::from(format!(
                "est. {:.0}% / {} min · schedule {} · repeat {} · expires {} · fallback {}",
                t.estimated_consumption,
                t.estimated_minutes,
                t.schedule.as_deref().unwrap_or("-"),
                t.repeat.as_deref().unwrap_or("-"),
                t.expiration_at.map_or("-".into(), plan_burner_core::db::ts),
                if t.allow_fallback { "yes" } else { "no" }
            )),
            Line::from(format!(
                "working dir {} · depends on {}",
                if t.working_dir.is_empty() { "-" } else { &t.working_dir },
                deps
            )),
            Line::from(format!(
                "result: {}",
                t.result_path.as_deref().unwrap_or("none yet")
            )),
        ];
        f.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title(format!(" Task {} – {} ", t.id, t.name)))
                .wrap(Wrap { trim: false }),
            b,
        );
    }
}

fn accounts(f: &mut Frame, area: Rect, app: &App) {
    let [top, bottom] = Layout::vertical([Constraint::Min(5), Constraint::Length(9)]).areas(area);
    let rows = app
        .snap
        .accounts
        .iter()
        .map(|a| {
            let d = app.snap.dets.get(&a.id);
            let l = &a.limits;
            let overrides = [
                l.start_threshold.is_some(),
                l.reserved_capacity.is_some(),
                l.idle_requirement.is_some(),
                l.allowed_hours.is_some(),
                l.max_usage_age_minutes.is_some(),
                l.max_tasks_per_batch.is_some(),
                l.max_execution_minutes.is_some(),
                l.session_threshold.is_some(),
                l.session_reserved.is_some(),
                l.ignore_session,
            ]
            .iter()
            .filter(|b| **b)
            .count();
            Row::new(vec![
                a.display(),
                a.subscription_plan.clone(),
                present::billing_label(d.map_or(BillingMode::Unverified, |d| d.billing))
                    .to_string(),
                d.map_or("?".into(), |d| {
                    if d.cli_found {
                        "found".into()
                    } else {
                        "missing".into()
                    }
                }),
                a.execution_policy.as_str().to_string(),
                match a.availability(plan_burner_core::availability::LocalClock::now()) {
                    Ok(()) => "yes".to_string(),
                    Err(_) if !a.available => "off".to_string(),
                    Err(_) => "closed".to_string(),
                },
                a.run_mode.as_str().to_string(),
                if a.subscription_confirmed {
                    "yes"
                } else {
                    "no"
                }
                .to_string(),
                if l.ignore_limits {
                    "IGNORED".to_string()
                } else if overrides > 0 {
                    format!("{overrides} custom")
                } else {
                    "general".to_string()
                },
                if present::burn_active(&app.settings, a) {
                    "ON".to_string()
                } else {
                    "off".to_string()
                },
            ])
        })
        .collect();
    table(
        rows,
        &[
            Constraint::Min(22),
            Constraint::Length(10),
            Constraint::Length(22),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(5),
        ],
        &[
            "Account", "Plan", "Billing", "CLI", "Policy", "Avail.", "Mode", "Sub. ok", "Limits",
            "Burn",
        ],
        " Accounts ".into(),
        app.sel,
    )(f, top);
    let Some(a) = app.snap.accounts.get(app.sel) else {
        f.render_widget(Block::bordered().title(" Details "), bottom);
        return;
    };
    let (ct, cl) = present::credentials_summary(a, &app.svc.env, chrono::Utc::now());
    let src = match a.usage_source {
        plan_burner_core::models::UsageSource::ClaudeOauth => "automatic (unofficial)",
        plan_burner_core::models::UsageSource::CustomScript => "custom script",
        plan_burner_core::models::UsageSource::Manual => "manual",
    };
    let mut lines = vec![Line::from(vec![
        "Credentials: ".into(),
        Span::styled(ct, Style::new().fg(level_color(cl))),
        format!("   usage: {src}").into(),
    ])];
    if let Some(e) = app.svc.usage_errors.lock().unwrap().get(&a.id) {
        lines.push(Line::from(format!("Last usage fetch failed: {e}")).red());
    } else if let Some(u) = app.usage.get(&a.id) {
        lines.push(
            Line::from(format!(
                "Last reading {} min ago ({}, {})",
                (chrono::Utc::now() - u.captured_at).num_minutes(),
                u.confidence,
                u.source
            ))
            .dark_gray(),
        );
    }
    lines.push(Line::from(format!(
        "profile {} · cli {} · schedule {}",
        a.config_dir.as_deref().unwrap_or("default"),
        a.cli_command.as_deref().unwrap_or("claude"),
        if a.availability_schedule.is_empty() {
            "always"
        } else {
            &a.availability_schedule
        }
    )));
    let g = &app.settings;
    let l = &a.limits;
    let o = |v: Option<String>, inh: String| v.unwrap_or(format!("{inh} (general)"));
    lines.push(Line::from(format!(
        "start {}% · reserve {}% · idle {} min · hours {} · max age {} min",
        o(
            l.start_threshold.map(|v| format!("{v:.0}")),
            format!("{:.0}", g.minimum_capacity_threshold)
        ),
        o(
            l.reserved_capacity.map(|v| format!("{v:.0}")),
            format!("{:.0}", g.reserved_capacity)
        ),
        o(
            l.idle_requirement.map(|v| v.to_string()),
            g.idle_requirement.to_string()
        ),
        o(
            l.allowed_hours
                .clone()
                .map(|h| if h.is_empty() { "always".into() } else { h }),
            if g.allowed_hours.is_empty() {
                "always".into()
            } else {
                g.allowed_hours.clone()
            }
        ),
        o(
            l.max_usage_age_minutes.map(|v| v.to_string()),
            g.max_usage_age_minutes.to_string()
        ),
    )));
    lines.push(Line::from(format!(
        "batch {} tasks / {} min · 5 h window: {}",
        o(
            l.max_tasks_per_batch.map(|v| v.to_string()),
            g.maximum_tasks_per_batch.to_string()
        ),
        o(
            l.max_execution_minutes.map(|v| v.to_string()),
            g.maximum_execution_minutes.to_string()
        ),
        if l.ignore_session {
            "ignored (weekly only)".to_string()
        } else {
            format!(
                "start {} reserve {}",
                l.session_threshold
                    .map_or("as above".into(), |v| format!("{v:.0}%")),
                l.session_reserved
                    .map_or("as above".into(), |v| format!("{v:.0}%"))
            )
        }
    )));
    if l.ignore_limits {
        lines.push(
            Line::from("Eligible ignoring limits: start threshold, reserve, idle and hours are not applied.")
                .yellow(),
        );
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(format!(" {} ", a.display())))
            .wrap(Wrap { trim: false }),
        bottom,
    );
}

fn approvals(f: &mut Frame, area: Rect, app: &App) {
    let pending = app.pending();
    if pending.is_empty() {
        f.render_widget(
            Paragraph::new("No pending requests. Propose a batch from an account (g), or wait for the scheduler.")
                .block(Block::bordered().title(" Awaiting approval (single-use) ")),
            area,
        );
        return;
    }
    let [top, bottom] = Layout::vertical([
        Constraint::Length(pending.len().min(5) as u16 + 3),
        Constraint::Min(6),
    ])
    .areas(area);
    let rows = pending
        .iter()
        .map(|(id, acct, ids)| {
            Row::new(vec![
                id.to_string(),
                account_name(app, *acct),
                ids.len().to_string(),
            ])
        })
        .collect();
    table(
        rows,
        &[
            Constraint::Length(6),
            Constraint::Min(22),
            Constraint::Length(7),
        ],
        &["Batch", "Account", "Tasks"],
        " Awaiting approval (single-use; approving runs only the selected tasks, once) ".into(),
        app.sel,
    )(f, top);
    let Some((id, acct, _)) = pending.get(app.sel) else {
        return;
    };
    let batch = app.svc.pending_batches().into_iter().find(|b| b.id == *id);
    let mut lines = vec![];
    if let Some(b) = &batch {
        lines.push(Line::from(format!(
            "Account {} · {} task(s) · est. {} min · est. consumption {:.0}% · confidence {}",
            account_name(app, *acct),
            b.proposal.tasks.len(),
            b.proposal.est_minutes,
            b.proposal.est_consumption,
            b.confidence
        )));
        if let Some(u) = app.usage.get(acct) {
            let reserved = app
                .snap
                .accounts
                .iter()
                .find(|a| a.id == *acct)
                .map_or(app.settings.reserved_capacity, |a| {
                    app.settings.effective_for(a).reserved_capacity
                });
            let p = |v: Option<f64>| v.map_or("unknown".into(), |x| format!("{x:.0}%"));
            lines.push(Line::from(format!(
                "Session remaining {} · weekly remaining {} · reserved {:.0}% · updated {} min ago",
                p(u.session_remaining),
                p(u.weekly_remaining),
                reserved,
                (chrono::Utc::now() - u.captured_at).num_minutes()
            )));
        }
        lines.push(
            Line::from("A batch runs on one account. Other accounts need their own approval. Consumption figures are estimates.")
                .dark_gray(),
        );
        let off = app.approval_off.get(id).cloned().unwrap_or_default();
        for (i, t) in b.proposal.tasks.iter().enumerate() {
            let on = !off.contains(&t.id);
            let cur = if i == app.tsel { "▶" } else { " " };
            let line = format!(
                "{cur} [{}] {}  · {} · {} min · ~{:.0}% · max budget {}",
                if on { "x" } else { " " },
                t.name,
                if t.model.is_empty() {
                    "default model"
                } else {
                    &t.model
                },
                t.estimated_minutes,
                t.estimated_consumption,
                t.maximum_budget
                    .map_or("none".into(), |m| format!("{m:.0}%"))
            );
            lines.push(if i == app.tsel {
                Line::from(line).bold()
            } else {
                Line::from(line)
            });
        }
        lines.push(
            Line::from(format!(
                "Approval policy: {}. Billing is re-checked just before each task starts.",
                app.settings.approval_policy
            ))
            .dark_gray(),
        );
    } else {
        lines.push(Line::from(format!("Batch {id}: no longer pending.")));
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(format!(" Batch {id} ")))
            .wrap(Wrap { trim: false }),
        bottom,
    );
}

fn history(f: &mut Frame, area: Rect, app: &App) {
    if app.hist_audit {
        let lines: Vec<Line> = if app.audit.is_empty() {
            vec!["No audit entries yet.".into()]
        } else {
            app.audit
                .iter()
                .map(|e| {
                    Line::from(format!(
                        "{} [{}] {} {}",
                        e.at.chars().take(19).collect::<String>(),
                        e.actor,
                        e.action,
                        e.detail
                    ))
                })
                .collect()
        };
        f.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title(" Automation audit log (A = back to runs) "))
                .scroll((app.sel as u16, 0)),
            area,
        );
        return;
    }
    let [top, bottom] = Layout::vertical([Constraint::Min(5), Constraint::Length(5)]).areas(area);
    let rows = app
        .history
        .iter()
        .map(|h| {
            Row::new(vec![
                h.started_at.chars().take(16).collect::<String>(),
                h.task_name.clone(),
                format!("{} · {}", h.provider, h.account),
                h.status.clone(),
                h.duration_secs.map_or("-".into(), |s| format!("{s}s")),
                h.estimated_consumption
                    .map_or("-".into(), |e| format!("~{e:.0}% est")),
                h.actual_consumption
                    .map_or("not measured".into(), |a| format!("{a:.0}% measured")),
            ])
        })
        .collect();
    table(
        rows,
        &[
            Constraint::Length(17),
            Constraint::Min(20),
            Constraint::Length(20),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(16),
        ],
        &[
            "Started",
            "Task",
            "Provider · account",
            "Status",
            "Dur.",
            "Estimate",
            "Measured",
        ],
        " History & results ".into(),
        app.sel,
    )(f, top);
    let lines = match app.history.get(app.sel) {
        Some(h) => vec![
            Line::from(format!(
                "model {}",
                if h.model.is_empty() {
                    "default"
                } else {
                    &h.model
                }
            )),
            match &h.error {
                Some(e) => Line::from(format!("error: {e}")).red(),
                None => Line::from("no error"),
            },
            Line::from(format!(
                "result: {}",
                h.result_path.as_deref().unwrap_or("none")
            )),
        ],
        None => vec![Line::from("No executions yet.")],
    };
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Selected run ")),
        bottom,
    );
}

fn templates(f: &mut Frame, area: Rect, app: &App) {
    if app.templates.is_empty() {
        f.render_widget(
            Paragraph::new("No templates yet. Press n to create one, or fill in a task form and press Ctrl-O (save as template).")
                .block(Block::bordered().title(" Task templates ")),
            area,
        );
        return;
    }
    let [top, bottom] = Layout::vertical([Constraint::Min(5), Constraint::Length(9)]).areas(area);
    let rows = app
        .templates
        .iter()
        .map(|t| {
            Row::new(vec![
                t.name.clone(),
                t.task
                    .provider_type
                    .clone()
                    .unwrap_or_else(|| "any provider".into()),
                if t.task.model.is_empty() {
                    "default model".to_string()
                } else {
                    t.task.model.clone()
                },
                format!("~{:.0}%", t.task.estimated_consumption),
                t.task.estimated_minutes.to_string(),
                present::priority_label(t.task.priority).to_string(),
            ])
        })
        .collect();
    table(
        rows,
        &[
            Constraint::Min(20),
            Constraint::Length(14),
            Constraint::Min(14),
            Constraint::Length(7),
            Constraint::Length(6),
            Constraint::Length(8),
        ],
        &["Name", "Provider", "Model", "Est.", "Min", "Prio"],
        " Task templates ".into(),
        app.sel,
    )(f, top);
    if let Some(t) = app.templates.get(app.sel) {
        let d = &t.task;
        let lines =
            vec![
                Line::from(format!("Prompt: {}", d.prompt.lines().next().unwrap_or(""))),
                Line::from(format!(
                    "Description: {}",
                    if d.description.is_empty() {
                        "-"
                    } else {
                        &d.description
                    }
                )),
                Line::from(format!(
                "run mode {} · timeout {} min · retries {} · budget {} · approval {} · fallback {}",
                d.run_mode,
                d.timeout_secs / 60,
                d.retry_max,
                d.maximum_budget.map_or("none".into(), |b| format!("{b:.0}%")),
                if d.requires_approval { "required" } else { "policy" },
                if d.allow_fallback { "yes" } else { "no" }
            )),
                Line::from(format!(
                    "working dir {}",
                    if d.working_dir.is_empty() {
                        "-"
                    } else {
                        &d.working_dir
                    }
                )),
                Line::from("A template never stores a target account, schedule or dependencies.")
                    .dark_gray(),
            ];
        f.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title(format!(" {} ", t.name)))
                .wrap(Wrap { trim: false }),
            bottom,
        );
    }
}
