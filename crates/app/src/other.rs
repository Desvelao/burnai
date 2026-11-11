use crate::ui::{self, AMBER, GREY};
use crate::App;
use chrono::Utc;
use eframe::egui::{self, RichText};
use plan_burner_core::{analytics, audit, models::*};
use std::collections::HashSet;

/// "Burn before reset" row: when ticked, the window's threshold and reserve drop to 0 within `v` hours of its reset.
fn burn_row(ui: &mut egui::Ui, label: &str, v: &mut Option<f64>, max: f64) {
    ui.label(label);
    ui.horizontal(|ui| {
        let mut on = v.is_some_and(|h| h > 0.0);
        if ui.checkbox(&mut on, "on").changed() {
            *v = on.then_some(max.min(2.0));
        }
        if let Some(h) = v {
            ui.add(egui::DragValue::new(h).range(0.0..=max).speed(0.25));
        }
    });
    ui.end_row();
}

/// How many executions the History page lists (and exports).
const HISTORY_LIMIT: usize = 200;

impl App {
    pub fn approvals(&mut self, ui: &mut egui::Ui) {
        ui::page_header(ui, "Approvals", |_| {});
        ui.label(
            RichText::new(
                "Approving runs only the selected tasks, once. It never authorizes future batches.",
            )
            .small()
            .color(GREY),
        );
        let batches: Vec<_> = self.svc.pending_batches();
        if batches.is_empty() {
            ui.label("No pending requests. Use \"Run tasks here…\" on the dashboard, or wait for the scheduler.");
        }
        let always = self.settings.approval_policy == "always";
        for b in batches {
            let acct = self
                .accounts
                .iter()
                .find(|a| a.id == b.proposal.account_id)
                .map(|a| a.display())
                .unwrap_or_default();
            let mut sel_owned = self.approval_sel.remove(&b.id).unwrap_or_else(|| {
                b.proposal
                    .tasks
                    .iter()
                    .map(|t| t.id)
                    .collect::<HashSet<_>>()
            });
            let sel = &mut sel_owned;
            ui::card(ui, None, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Approve proposed batch").strong().size(18.0));
                ui.horizontal(|ui| {
                    ui::stat_tile(ui, "Account", RichText::new(acct.clone()));
                    ui::stat_tile(
                        ui,
                        "Tasks",
                        RichText::new(format!("{}", b.proposal.tasks.len())),
                    );
                    ui::stat_tile(
                        ui,
                        "Est. duration",
                        RichText::new(format!("~{} min", b.proposal.est_minutes)),
                    );
                    ui::stat_tile(
                        ui,
                        "Est. consumption",
                        RichText::new(format!("~{:.0}%", b.proposal.est_consumption)),
                    );
                    ui::stat_tile(ui, "Confidence", RichText::new(b.confidence.to_string()));
                });
                ui.label(RichText::new("A batch runs on one account. Other accounts need their own approval. Consumption figures are estimates.").small().color(GREY));
                let acct_reserved = self
                    .accounts
                    .iter()
                    .find(|a| a.id == b.proposal.account_id)
                    .map_or(self.settings.reserved_capacity, |a| {
                        self.settings.effective_for(a).reserved_capacity
                    });
                if let Ok(Some(u)) =
                    plan_burner_core::usage::latest(&self.svc.conn, b.proposal.account_id)
                {
                    let s =
                        |v: Option<f64>| v.map(|x| format!("{x:.0}%")).unwrap_or("unknown".into());
                    ui.label(format!(
                        "Session remaining {} · weekly remaining {} · reserved {:.0}% · updated {} min ago",
                        s(u.session_remaining),
                        s(u.weekly_remaining),
                        acct_reserved,
                        (Utc::now() - u.captured_at).num_minutes()
                    ));
                }
                egui::Grid::new(("apgrid", b.id))
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label("");
                        ui.label("Task");
                        ui.label("Model");
                        ui.label("Duration");
                        ui.label("Est. usage");
                        ui.label("Max budget");
                        ui.end_row();
                        for t in &b.proposal.tasks {
                            let mut on = sel.contains(&t.id);
                            if ui.checkbox(&mut on, "").changed() {
                                if on {
                                    sel.insert(t.id);
                                } else {
                                    sel.remove(&t.id);
                                }
                            }
                            ui.label(&t.name);
                            ui.label(if t.model.is_empty() {
                                "default"
                            } else {
                                &t.model
                            });
                            ui.label(format!("~{} min", t.estimated_minutes));
                            ui.label(format!("~{:.0}%", t.estimated_consumption));
                            ui.label(
                                t.maximum_budget
                                    .map(|x| format!("{x:.0}%"))
                                    .unwrap_or("none".into()),
                            );
                            ui.end_row();
                        }
                    });
                ui.label(RichText::new("Billing: subscription login verified before each task; API billing is blocked. The batch stops on a rate limit.").small().color(GREY));
                let chosen: Vec<i64> = sel.iter().copied().collect();
                ui.horizontal(|ui| {
                    if self.pending == Some(crate::Pending::RejectBatch(b.id)) {
                        match ui::inline_confirm(
                            ui,
                            &format!(
                                "Reject this batch of {} task(s)? They return to the queue.",
                                b.proposal.tasks.len()
                            ),
                            "Yes, reject",
                        ) {
                            Some(true) => {
                                self.pending = None;
                                let r = self.svc.reject_batch(b.id);
                                self.report(r, "Batch rejected; tasks returned to the queue.");
                            }
                            Some(false) => self.pending = None,
                            None => {}
                        }
                    } else if ui::secondary_button(ui, "Reject batch").clicked() {
                        self.pending = Some(crate::Pending::RejectBatch(b.id));
                    }
                    if ui
                        .add_enabled(
                            !chosen.is_empty(),
                            egui::Button::new(
                                RichText::new("Approve selected").color(egui::Color32::WHITE),
                            )
                            .fill(crate::theme::PRIMARY)
                            .min_size(egui::vec2(0.0, 30.0)),
                        )
                        .clicked()
                    {
                        let r = self.svc.approve_and_start(b.id, &chosen);
                        self.report(r, "Batch approved and started.");
                    }
                    if !always && ui::success_button(ui, "Approve entire batch").clicked() {
                        let all: Vec<i64> = b.proposal.tasks.iter().map(|t| t.id).collect();
                        let r = self.svc.approve_and_start(b.id, &all);
                        self.report(r, "Batch approved and started.");
                    }
                });
            });
            ui.add_space(8.0);
            self.approval_sel.insert(b.id, sel_owned);
        }
    }

    pub fn history(&mut self, ui: &mut egui::Ui) {
        ui::page_header(ui, "History & results", |_| {});
        self.result_card(ui);
        let rows = analytics::history(&self.svc.conn, HISTORY_LIMIT).unwrap_or_default();
        ui.horizontal(|ui| {
            if ui::secondary_button(ui, "Export CSV").clicked() {
                let p = std::path::Path::new(&self.settings.results_dir).join("history.csv");
                let r = std::fs::create_dir_all(&self.settings.results_dir)
                    .and_then(|()| std::fs::write(&p, analytics::export_history_csv(&rows)))
                    .map_err(plan_burner_core::Error::from);
                self.report(r, &format!("Exported {}", p.display()));
            }
            if ui::secondary_button(ui, "Export JSON").clicked() {
                let p = std::path::Path::new(&self.settings.results_dir).join("history.json");
                let r = analytics::export_history_json(&rows).and_then(|j| {
                    std::fs::create_dir_all(&self.settings.results_dir)
                        .and_then(|()| std::fs::write(&p, j))
                        .map_err(Into::into)
                });
                self.report(r, &format!("Exported {}", p.display()));
            }
        });
        if rows.is_empty() {
            ui.label("No executions yet.");
        } else if rows.len() >= HISTORY_LIMIT {
            ui.label(
                RichText::new(format!(
                    "Showing the latest {HISTORY_LIMIT} executions; the exports contain the same rows."
                ))
                .small()
                .color(GREY),
            );
        }
        for r in rows {
            ui::card(ui, None, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&r.task_name).strong());
                    ui::badge(
                        ui,
                        &r.status,
                        if r.status == "completed" {
                            ui::GREEN
                        } else {
                            ui::RED
                        },
                    );
                    ui.label(format!(
                        "{} · {} · {}",
                        r.provider,
                        r.account,
                        if r.model.is_empty() {
                            "default model"
                        } else {
                            &r.model
                        }
                    ));
                    ui.label(format!(
                        "{} · {} s · est. {} (estimate) · actual {}",
                        r.started_at.get(..16).unwrap_or(&r.started_at),
                        r.duration_secs.map(|d| d.to_string()).unwrap_or("?".into()),
                        r.estimated_consumption
                            .map(|v| format!("{v:.0}%"))
                            .unwrap_or("-".into()),
                        r.actual_consumption
                            .map(|v| format!("{v:.1}% (measured)"))
                            .unwrap_or("not measured".into()),
                    ));
                });
                if let Some(e) = &r.error {
                    ui.colored_label(ui::RED, e);
                }
                ui.horizontal(|ui| {
                    let v = self
                        .actual_inputs
                        .entry(r.execution_id)
                        .or_insert(r.actual_consumption.unwrap_or(0.0));
                    ui.add(
                        egui::DragValue::new(v)
                            .range(0.0..=100.0)
                            .speed(0.1)
                            .prefix("Measured usage consumed ")
                            .suffix(" %"),
                    )
                    .on_hover_text(
                        "How much of the usage window this run actually used, e.g. the drop in \
                         the % remaining you read from /usage before and after the run.",
                    );
                    if ui::secondary_button(ui, "Record measured usage").clicked() {
                        let val = *v;
                        match self.svc.record_actual(r.execution_id, val) {
                            Ok(over) => {
                                self.status = Some((
                                    if over {
                                        "Recorded: exceeded the task's maximum budget!".into()
                                    } else {
                                        "Recorded.".into()
                                    },
                                    !over,
                                ));
                                self.refresh();
                            }
                            Err(e) => self.status = Some((e.to_string(), false)),
                        }
                    }
                    if let Some(p) = &r.result_path {
                        if ui::primary_button(ui, "View result").clicked() {
                            self.open_result(&r.task_name, p);
                        }
                        ui.label(RichText::new(p).small().color(GREY));
                    }
                });
            });
        }
        ui.separator();
        ui.collapsing("Automation audit log", |ui| {
            for e in audit::recent(&self.svc.conn, 50).unwrap_or_default() {
                ui.label(
                    RichText::new(format!(
                        "{} [{}] {} {}",
                        e.at.get(..19).unwrap_or(&e.at),
                        e.actor,
                        e.action,
                        e.detail
                    ))
                    .small()
                    .monospace(),
                );
            }
        });
    }

    pub fn settings_view(&mut self, ui: &mut egui::Ui) {
        ui::page_header(ui, "Automation settings", |_| {});
        if !self.settings.enabled || self.settings.execution_mode != ExecutionPolicy::Automatic {
            ui::banner(ui, AMBER, "Automatic execution is off. Tasks run only after your approval, and only with a verified subscription and fresh usage.");
            ui.add_space(6.0);
        }
        let mut s = self
            .settings_draft
            .as_ref()
            .map_or_else(|| self.settings.clone(), |(_, d)| d.clone());
        let before = s.clone();
        ui.columns(2, |cols| {
            {
                let ui = &mut cols[0];
                ui::card(ui, None, |ui| {
                    ui.set_width(ui.available_width());
                    ui::section_label(ui, "Control");
                    egui::Grid::new("settings_Control")
                        .num_columns(2)
                        .spacing([16.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Automation enabled");
                            ui.checkbox(&mut s.enabled, "");
                            ui.end_row();
                            ui.label("Execution policy");
                            egui::ComboBox::from_id_salt("mode")
                                .selected_text(s.execution_mode.as_str())
                                .show_ui(ui, |ui| {
                                    for p in ExecutionPolicy::ALL {
                                        ui.selectable_value(&mut s.execution_mode, *p, p.as_str());
                                    }
                                });
                            ui.end_row();
                            ui.label("Approval");
                            egui::ComboBox::from_id_salt("appr")
                                .selected_text(if s.approval_policy == "always" {
                                    "Always require approval"
                                } else {
                                    "Ask once per batch"
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut s.approval_policy,
                                        "always".to_string(),
                                        "Always require approval",
                                    );
                                    ui.selectable_value(
                                        &mut s.approval_policy,
                                        "once_per_batch".to_string(),
                                        "Ask once per batch",
                                    );
                                });
                            ui.end_row();
                        });
                });
                ui.add_space(8.0);
                ui::card(ui, None, |ui| {
                    ui.set_width(ui.available_width());
                    ui::section_label(ui, "Limits");
                    egui::Grid::new("settings_Limits")
                        .num_columns(2)
                        .spacing([16.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Start when capacity above (%)");
                            ui.add(
                                egui::DragValue::new(&mut s.minimum_capacity_threshold)
                                    .range(0.0..=100.0),
                            );
                            ui.end_row();
                            ui.label("Reserved capacity (%)");
                            ui.add(
                                egui::DragValue::new(&mut s.reserved_capacity).range(0.0..=100.0),
                            );
                            ui.end_row();
                            burn_row(
                                ui,
                                "Burn 5 h window when it resets within (h)",
                                &mut s.burn_session_hours,
                                5.0,
                            );
                            burn_row(
                                ui,
                                "Burn weekly window when it resets within (h)",
                                &mut s.burn_weekly_hours,
                                168.0,
                            );
                            ui.label("Max tasks per batch");
                            ui.add(
                                egui::DragValue::new(&mut s.maximum_tasks_per_batch).range(1..=50),
                            );
                            ui.end_row();
                            ui.label("Max execution minutes per batch");
                            ui.add(
                                egui::DragValue::new(&mut s.maximum_execution_minutes)
                                    .range(1..=1440),
                            );
                            ui.end_row();
                        });
                });
                ui.add_space(8.0);
            }
            {
                let ui = &mut cols[1];
                ui::card(ui, None, |ui| {
                    ui.set_width(ui.available_width());
                    ui::section_label(ui, "Conditions");
                    egui::Grid::new("settings_Conditions")
                        .num_columns(2)
                        .spacing([16.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Idle requirement (min, 0 = off)");
                            ui.add(egui::DragValue::new(&mut s.idle_requirement).range(0..=240));
                            ui.end_row();
                            ui.label("Allowed hours");
                            ui::checked_text(
                                ui,
                                &mut s.allowed_hours,
                                "22:00-07:00",
                                "Local time window in which tasks may start. May cross midnight. Blank = any time.",
                                ui::check::hours,
                            );
                            ui.end_row();
                            ui.label("Default run mode");
                            crate::ui::run_mode_combo(
                                ui,
                                "default_run_mode",
                                &mut s.default_run_mode,
                                "Provider default",
                            );
                            ui.end_row();
                            ui.label("Max usage age (min)");
                            ui.add(
                                egui::DragValue::new(&mut s.max_usage_age_minutes).range(1..=1440),
                            );
                            ui.end_row();
                            ui.label("Keep history (days, 0 = forever)");
                            ui.add(egui::DragValue::new(&mut s.retention_days).range(0..=3650));
                            ui.end_row();
                            ui.label("Check interval normal / waiting (min)");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::DragValue::new(&mut s.interval_normal_minutes)
                                        .range(1..=240),
                                );
                                ui.add(
                                    egui::DragValue::new(&mut s.interval_waiting_minutes)
                                        .range(1..=240),
                                );
                            });
                            ui.end_row();
                            ui.label("Allowed working directory root");
                            ui.horizontal(|ui| { crate::ui::path_field(ui, &mut s.working_root, plan_burner_core::pick::PickKind::Dir, None); });
                            ui.end_row();
                            ui.label("Results folder");
                            ui.horizontal(|ui| { crate::ui::path_field(ui, &mut s.results_dir, plan_burner_core::pick::PickKind::Dir, None); });
                            ui.end_row();
                        });
                });
                ui.add_space(8.0);
                ui::card(ui, None, |ui| {
                    ui.set_width(ui.available_width());
                    ui::section_label(ui, "Notifications");
                    egui::Grid::new("settings_Notifications")
                        .num_columns(2)
                        .spacing([16.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Quiet hours");
                            ui::checked_text(
                                ui,
                                &mut s.quiet_hours,
                                "23:00-07:00",
                                "Local time window with no desktop or Telegram notifications. Blank = never quiet.",
                                ui::check::hours,
                            );
                            ui.end_row();
                            ui.label("Desktop notifications");
                            ui.checkbox(&mut s.notify_desktop, "");
                            ui.end_row();
                            ui.label("Telegram notifications");
                            ui.checkbox(
                                &mut s.notify_telegram,
                                "token from env PLAN_BURNER_TELEGRAM_TOKEN",
                            );
                            ui.end_row();
                            ui.label("Telegram chat id");
                            ui.text_edit_singleline(&mut s.telegram_chat_id);
                            ui.end_row();
                            ui.label("Include task names externally");
                            ui.checkbox(&mut s.notify_include_names, "(prompts are never sent)");
                            ui.end_row();
                        });
                });
                ui.add_space(8.0);
            }
        });
        if s != before {
            let base = self
                .settings_draft
                .take()
                .map_or_else(|| self.settings.clone(), |(b, _)| b);
            self.settings_draft = Some((base, s.clone()));
        }
        if !self.settings_dirty() {
            self.settings_draft = None;
        }
        let dirty = self.settings_dirty();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(dirty, egui::Button::new("Save settings"))
                .clicked()
            {
                if let Some((base, draft)) = self.settings_draft.clone() {
                    let mut out = draft;
                    // an Automation toggle made elsewhere (top bar, tray) since editing began wins
                    if out.enabled == base.enabled {
                        out.enabled = self.settings.enabled;
                    }
                    match self.svc.save_settings(&out) {
                        Ok(()) => {
                            self.settings_draft = None;
                            self.settings_error = None;
                            self.status = Some(("Settings saved.".into(), true));
                            self.refresh();
                        }
                        Err(e) => {
                            self.settings_error = Some(e.to_string());
                            self.status = Some((e.to_string(), false));
                        }
                    }
                }
            }
            if dirty && self.pending == Some(crate::Pending::DiscardSettings) {
                match ui::inline_confirm(ui, "Throw away your unsaved changes?", "Yes, discard") {
                    Some(true) => {
                        self.pending = None;
                        self.settings_draft = None;
                        self.settings_error = None;
                    }
                    Some(false) => self.pending = None,
                    None => {}
                }
            } else if dirty && ui::secondary_button(ui, "Discard changes").clicked() {
                self.pending = Some(crate::Pending::DiscardSettings);
            }
            if dirty {
                ui.colored_label(AMBER, "Unsaved changes (not applied yet)");
            }
            if let Some(e) = &self.settings_error {
                ui.colored_label(ui::RED, e);
            }
        });
        ui.add_space(8.0);
        ui::card(ui, None, |ui| {
            ui.set_width(ui.available_width());
            ui::section_label(ui, "Safety notes");
            ui.label(RichText::new("• Tasks run only with a verified subscription login; API keys or cloud-billing variables block execution.\n• Stale or unknown usage blocks execution (usage comes from automatic polling or manual entry).\n• Idle detection needs `xprintidle`; if missing, an idle requirement is never satisfied.\n• Email notifications are not supported.").size(13.0).color(AMBER));
        });
    }
}
