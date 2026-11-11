use crate::ui::{self, badge, GREY};
use crate::{App, Filter, TaskForm, TemplateForm};
use eframe::egui::{self, RichText};
use plan_burner_core::ipc::TaskAction;
use plan_burner_core::{analytics, models::*, tasks};

/// State of the "Import tasks" panel (all rules live in `core::import` / `Service::import_file`).
#[derive(Default)]
pub struct ImportPanel {
    pub path: String,
    /// 0 = auto, 1 = JSON, 2 = Markdown.
    pub format: usize,
    pub dry_run: bool,
    pub report: Option<String>,
}

impl App {
    fn import_panel(&mut self, ui: &mut egui::Ui) {
        let Some(p) = &mut self.import else { return };
        let (mut go, mut close) = (false, false);
        ui::card(ui, Some(ui::BLUE), |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Import tasks").strong().size(16.0));
            ui.label(
                RichText::new(
                    "JSON array or Markdown list/headings. Tasks are queued as pending (approval stays as written in the file, default on); nothing starts.",
                )
                .small()
                .color(crate::theme::MUTED),
            );
            ui.horizontal(|ui| {
                ui.label("File");
                crate::ui::path_field(
                    ui,
                    &mut p.path,
                    plan_burner_core::pick::PickKind::File,
                    Some(360.0),
                );
                egui::ComboBox::from_id_salt("import_format")
                    .selected_text(["Auto", "JSON", "Markdown"][p.format.min(2)])
                    .show_ui(ui, |ui| {
                        for (i, n) in ["Auto", "JSON", "Markdown"].iter().enumerate() {
                            ui.selectable_value(&mut p.format, i, *n);
                        }
                    });
                ui.checkbox(&mut p.dry_run, "Check only");
            });
            ui.horizontal(|ui| {
                go = ui::primary_button(ui, if p.dry_run { "Check" } else { "Import" }).clicked();
                close = ui::secondary_button(ui, "Close").clicked();
            });
            if let Some(r) = &p.report {
                ui.separator();
                ui.label(RichText::new(r.as_str()).monospace());
            }
        });
        if go {
            let format = match p.format {
                1 => plan_burner_core::import::ImportFormat::Json,
                2 => plan_burner_core::import::ImportFormat::Markdown,
                _ => plan_burner_core::import::ImportFormat::Auto,
            };
            let (path, dry) = (p.path.clone(), p.dry_run);
            let r = self.svc.import_file(&path, format, dry);
            if let Some(rep) = self.report(r, "") {
                self.status = Some((rep.summary(), true));
                if let Some(p) = &mut self.import {
                    p.report = Some(rep.render());
                }
            }
        }
        if close {
            self.import = None;
        }
    }

    fn target_label(&self, t: &tasks::Task) -> String {
        match (&t.provider_type, t.account_id) {
            (_, Some(id)) => self
                .accounts
                .iter()
                .find(|a| a.id == id)
                .map(|a| a.display())
                .unwrap_or_else(|| "removed account".into()),
            (Some(p), None) => format!("any {p} account"),
            (None, _) => "any provider".into(),
        }
    }

    /// Opens a stored result in the viewer; a read failure is a status error, never viewer text
    /// (which could be mistaken for AI output and copied or exported).
    pub fn open_result(&mut self, name: &str, path: &str) {
        match std::fs::read_to_string(path) {
            Ok(body) => self.viewing_result = Some((name.to_string(), body)),
            Err(e) => {
                self.status = Some((format!("Cannot read the result file {path}: {e}"), false))
            }
        }
    }

    pub fn queue(&mut self, ui: &mut egui::Ui) {
        ui::page_header(ui, "Task queue", |ui| {
            let mut propose: Option<i64> = None;
            ui.menu_button("Propose batch on…", |ui| {
                if self.accounts.is_empty() {
                    ui.label("Add an account first");
                }
                for a in &self.accounts {
                    if ui.button(a.display()).clicked() {
                        propose = Some(a.id);
                        ui.close_menu();
                    }
                }
            });
            if let Some(id) = propose {
                self.propose_batch_for(id);
            }
            if ui::primary_button(ui, "New task").clicked() {
                self.task_form = Some(TaskForm::new_task());
            }
            if ui::secondary_button(ui, "Import…").clicked() {
                self.import = Some(ImportPanel::default());
            }
        });
        self.import_panel(ui);
        self.result_card(ui);
        if let Some(g) = &self.qstatus.global {
            ui.label(
                RichText::new(format!("Automation is holding back the queue: {g}"))
                    .small()
                    .color(ui::AMBER),
            );
        }
        ui.horizontal_wrapped(|ui| {
            if ui::chip(
                ui,
                self.filter == Filter::All,
                &format!("All ({})", self.tasks.len()),
                None,
            )
            .clicked()
            {
                self.filter = Filter::All;
            }
            for a in self.accounts.clone() {
                let n = self
                    .tasks
                    .iter()
                    .filter(|t| t.account_id == Some(a.id))
                    .count();
                let sel = self.filter == Filter::Account(a.id);
                if ui::chip(
                    ui,
                    sel,
                    &format!("{} ({n})", a.display()),
                    Some(ui::provider_color(&a.provider_type)),
                )
                .clicked()
                {
                    self.filter = Filter::Account(a.id);
                }
            }
            let n = self.tasks.iter().filter(|t| t.account_id.is_none()).count();
            if ui::chip(
                ui,
                self.filter == Filter::AnyAccount,
                &format!("Any account ({n})"),
                Some(crate::theme::OTHER_PROVIDER),
            )
            .clicked()
            {
                self.filter = Filter::AnyAccount;
            }
        });
        ui.add_space(6.0);
        let list: Vec<tasks::Task> = self
            .tasks
            .iter()
            .filter(|t| match self.filter {
                Filter::All => true,
                Filter::Account(id) => t.account_id == Some(id),
                Filter::AnyAccount => t.account_id.is_none(),
            })
            .cloned()
            .collect();
        if list.is_empty() {
            ui.label(RichText::new("No tasks.").color(GREY));
        }
        // Drag-and-drop result: (dragged id, target id, insert after target).
        let mut dropped: Option<(i64, i64, bool)> = None;
        for t in list {
            let target_color = match t.account_id {
                Some(id) => self
                    .accounts
                    .iter()
                    .find(|a| a.id == id)
                    .map(|a| ui::provider_color(&a.provider_type))
                    .unwrap_or(crate::theme::OTHER_PROVIDER),
                None => crate::theme::OTHER_PROVIDER,
            };
            let highlight = t.status == TaskStatus::AwaitingApproval;
            let frame = egui::Frame::new()
                .fill(if highlight {
                    crate::theme::ROW_HIGHLIGHT
                } else {
                    crate::theme::CARD
                })
                .stroke(egui::Stroke::new(1.0_f32, crate::theme::CARD_BORDER))
                .corner_radius(egui::CornerRadius::same(6))
                .inner_margin(egui::Margin::symmetric(14, 10));
            let row = frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let grip = ui
                        .add(
                            egui::Label::new(RichText::new("☰").color(GREY))
                                .sense(egui::Sense::drag()),
                        )
                        .on_hover_cursor(egui::CursorIcon::Grab)
                        .on_hover_text("Drag to reorder");
                    grip.dnd_set_drag_payload(t.id);
                    if arrow_button(ui, true).on_hover_text("Move up").clicked() {
                        let r = self.svc.task_action(t.id, TaskAction::MoveBy { delta: -1 });
                        self.report(r, "");
                    }
                    if arrow_button(ui, false).on_hover_text("Move down").clicked() {
                        let r = self.svc.task_action(t.id, TaskAction::MoveBy { delta: 1 });
                        self.report(r, "");
                    }
                    ui.vertical(|ui| {
                        if ui
                            .selectable_label(
                                self.selected == Some(t.id),
                                RichText::new(&t.name).strong(),
                            )
                            .clicked()
                        {
                            self.selected = if self.selected == Some(t.id) {
                                None
                            } else {
                                Some(t.id)
                            };
                        }
                        ui.horizontal(|ui| {
                            ui::swatch(ui, target_color);
                            ui.label(
                                RichText::new(format!(
                                    "#{} · {} · {}",
                                    t.id,
                                    self.target_label(&t),
                                    if t.model.is_empty() {
                                        "default model"
                                    } else {
                                        &t.model
                                    }
                                ))
                                .size(12.0)
                                .color(crate::theme::MUTED),
                            );
                        });
                        if let Some(why) = self.qstatus.reasons.get(&t.id) {
                            ui.label(RichText::new(why).size(12.0).color(ui::AMBER));
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        badge(ui, t.status.label(), ui::status_color(t.status));
                        if let Some(r) = &t.repeat {
                            ui.label(RichText::new(format!("↻ {r}")).size(13.0).color(ui::GREY));
                        }
                        ui.label(
                            RichText::new(format!("~{:.0}% (est.)", t.estimated_consumption))
                                .size(13.0),
                        );
                        ui.label(
                            RichText::new(ui::priority_label(t.priority))
                                .size(13.0)
                                .color(crate::theme::MUTED),
                        );
                    });
                });
                ui.horizontal_wrapped(|ui| {
                    if t.status != TaskStatus::Running && ui::primary_button(ui, "Edit").clicked() {
                        let deps = tasks::dependencies(&self.svc.conn, t.id).unwrap_or_default();
                        self.task_form = Some(TaskForm::from_task(&t, &deps));
                    }
                    if matches!(
                        t.status,
                        TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::Expired
                    ) && ui::primary_button(ui, "Retry").clicked()
                    {
                        let r = self.svc.task_action(t.id, TaskAction::Retry);
                        self.report(r, "Task re-queued.");
                    }
                    if ui::secondary_button(ui, "Duplicate").clicked() {
                        let r = self.svc.task_action(t.id, TaskAction::Duplicate);
                        self.report(r, "Task duplicated.");
                    }
                    if t.status == TaskStatus::Paused {
                        if ui::secondary_button(ui, "Resume").clicked() {
                            let r = self.svc.task_action(t.id, TaskAction::Resume);
                            self.report(r, "Task resumed.");
                        }
                    } else if !t.status.is_terminal()
                        && t.status != TaskStatus::Running
                        && ui::secondary_button(ui, "Pause").clicked()
                    {
                        let r = self.svc.task_action(t.id, TaskAction::Pause);
                        self.report(r, "Task paused.");
                    }
                    if !t.status.is_terminal() && t.status != TaskStatus::Running {
                        if self.pending == Some(crate::Pending::CancelTask(t.id)) {
                            match ui::inline_confirm(
                                ui,
                                &format!("Cancel \"{}\"?", t.name),
                                "Yes, cancel it",
                            ) {
                                Some(true) => {
                                    self.pending = None;
                                    let r = self.svc.task_action(t.id, TaskAction::Cancel);
                                    self.report(r, "Task cancelled.");
                                }
                                Some(false) => self.pending = None,
                                None => {}
                            }
                        } else if ui::secondary_button(ui, "Cancel").clicked() {
                            self.pending = Some(crate::Pending::CancelTask(t.id));
                        }
                    }
                    if t.status == TaskStatus::Running {
                        if self.pending == Some(crate::Pending::StopRunning(t.id)) {
                            match ui::inline_confirm(
                                ui,
                                &format!("Stop \"{}\"? Its process is killed.", t.name),
                                "Yes, stop it",
                            ) {
                                Some(true) => {
                                    self.pending = None;
                                    let r = self.svc.cancel_active();
                                    self.report(r, "Stop requested for the running task.");
                                }
                                Some(false) => self.pending = None,
                                None => {}
                            }
                        } else if ui::danger_button(ui, "Stop this task").clicked() {
                            self.pending = Some(crate::Pending::StopRunning(t.id));
                        }
                    }
                    if t.status.is_runnable() || t.status == TaskStatus::Paused {
                        let menu = ui.menu_button("Run now on…", |ui| {
                            let cands: Vec<Account> = self
                                .accounts
                                .iter()
                                .filter(|a| {
                                    t.provider_type
                                        .as_deref()
                                        .is_none_or(|p| p == a.provider_type)
                                })
                                .cloned()
                                .collect();
                            if cands.is_empty() {
                                ui.label("No matching account");
                            }
                            for a in cands {
                                // The same pre-flight the launch runs: ineligible accounts say why.
                                let why = self
                                    .svc
                                    .run_now_blockers(t.id, a.id)
                                    .unwrap_or_else(|e| vec![e.to_string()]);
                                let label = if why.is_empty() {
                                    a.display()
                                } else {
                                    format!("{} (not eligible)", a.display())
                                };
                                let resp = ui.add_enabled(why.is_empty(), egui::Button::new(label));
                                let resp = if why.is_empty() {
                                    resp
                                } else {
                                    resp.on_disabled_hover_text(why.join("\n"))
                                };
                                if resp.clicked() {
                                    let r = self.svc.run_now(t.id, a.id);
                                    self.report(r, "Run started.");
                                    ui.close_menu();
                                }
                            }
                        });
                        if t.status == TaskStatus::Paused {
                            menu.response.on_hover_text(
                                "A paused task is skipped by the scheduler; running it by hand still works.",
                            );
                        }
                    }
                    if let Some(p) = &t.result_path {
                        if ui::primary_button(ui, "Result").clicked() {
                            self.open_result(&t.name, p);
                        }
                    }
                    if t.status != TaskStatus::Running {
                        if self.confirm_delete_task == Some(t.id) {
                            match ui::inline_confirm(ui, "Delete this task?", "Yes, delete") {
                                Some(true) => {
                                    self.confirm_delete_task = None;
                                    let r = self.svc.task_action(t.id, TaskAction::Delete);
                                    self.report(r, "Task deleted.");
                                }
                                Some(false) => self.confirm_delete_task = None,
                                None => {}
                            }
                        } else if ui::secondary_button(ui, "Delete").clicked() {
                            self.confirm_delete_task = Some(t.id);
                        }
                    }
                });
            });
            let rect = row.response.rect;
            if let Some(src) = row.response.dnd_hover_payload::<i64>() {
                if *src != t.id {
                    let after = ui
                        .input(|i| i.pointer.interact_pos())
                        .is_some_and(|p| p.y > rect.center().y);
                    let y = if after { rect.bottom() } else { rect.top() };
                    ui.painter()
                        .hline(rect.x_range(), y, egui::Stroke::new(2.0_f32, ui::AMBER));
                    if row.response.dnd_release_payload::<i64>().is_some() {
                        dropped = Some((*src, t.id, after));
                    }
                }
            }
            ui.add_space(2.0);
        }
        if let Some((id, target, after)) = dropped {
            let r = self
                .svc
                .task_action(id, TaskAction::MoveNextTo { target, after });
            self.report(r, "");
        }
        if let Some(t) = self
            .tasks
            .iter()
            .find(|t| Some(t.id) == self.selected)
            .cloned()
        {
            ui.add_space(8.0);
            ui::card(ui, None, |ui| {
                ui.set_width(ui.available_width());
                ui::section_label(ui, &format!("Selected: {}", t.name));
                ui.label(
                    RichText::new("Prompt is stored locally and never sent in notifications.")
                        .small()
                        .color(GREY),
                );
                ui.label(&t.prompt);
                let deps = tasks::dependencies(&self.svc.conn, t.id).unwrap_or_default();
                let deps = plan_burner_core::present::dependency_names(&self.tasks, &deps);
                ui.label(
                    RichText::new(format!(
                        "Timeout {} min · retries {}/{} · budget {} · approval {} · depends on {}",
                        t.timeout_secs / 60,
                        t.retry_count,
                        t.retry_max,
                        t.maximum_budget
                            .map(|b| format!("{b:.0}%"))
                            .unwrap_or_else(|| "none".into()),
                        if t.requires_approval {
                            "required"
                        } else {
                            "policy"
                        },
                        deps
                    ))
                    .color(crate::theme::MUTED)
                    .size(13.0),
                );
            });
        }
    }

    /// Inline viewer for a task result (shown at the top of the Queue and History pages).
    pub fn result_card(&mut self, ui: &mut egui::Ui) {
        let Some((title, body)) = self.viewing_result.clone() else {
            return;
        };
        let mut close = false;
        ui::card(ui, Some(ui::BLUE), |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(format!("Result: {title}"))
                    .strong()
                    .size(16.0),
            );
            ui.label(
                RichText::new("AI output is untrusted text. Review before using it.")
                    .small()
                    .color(ui::AMBER),
            );
            ui.horizontal(|ui| {
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(body.clone());
                }
                if ui.button("Export to results folder").clicked() {
                    let p = std::path::Path::new(&self.settings.results_dir).join(format!(
                        "export-{}.txt",
                        plan_burner_core::engine::sanitize_filename(&title)
                    ));
                    let r = std::fs::create_dir_all(&self.settings.results_dir)
                        .and_then(|()| std::fs::write(&p, &body))
                        .map_err(plan_burner_core::Error::from);
                    self.report(r, &format!("Exported to {}", p.display()));
                }
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
            egui::ScrollArea::vertical()
                .id_salt("result_scroll")
                .max_height(320.0)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(&body).monospace()).wrap());
                });
        });
        if close {
            self.viewing_result = None;
        }
    }

    /// Inline manual-usage entry inside the account's dashboard card.
    pub fn usage_panel(&mut self, ui: &mut egui::Ui, account_id: i64) {
        let Some(f) = self
            .usage_form
            .as_mut()
            .filter(|f| f.account_id == account_id)
        else {
            return;
        };
        let name = self
            .accounts
            .iter()
            .find(|a| a.id == f.account_id)
            .map(|a| a.display())
            .unwrap_or_default();
        let mut save = false;
        let mut cancel = false;
        ui.group(|ui| {
            ui.label(RichText::new(format!("Update usage: {name}")).strong());
            ui.label("Enter the values shown by Claude (e.g. /usage). These are recorded as manual entries, not provider measurements. Unchecked = unknown, which blocks execution.");
            ui.checkbox(&mut f.session_known, "Session remaining known");
            ui.add_enabled(f.session_known, egui::DragValue::new(&mut f.session).range(0.0..=100.0).suffix(" %"));
            ui.add(egui::DragValue::new(&mut f.session_reset_h).range(0.0..=24.0).speed(0.1).prefix("session resets in ").suffix(" h"));
            ui.checkbox(&mut f.weekly_known, "Weekly remaining known");
            ui.add_enabled(f.weekly_known, egui::DragValue::new(&mut f.weekly).range(0.0..=100.0).suffix(" %"));
            ui.add(egui::DragValue::new(&mut f.weekly_reset_h).range(0.0..=200.0).speed(0.5).prefix("weekly resets in ").suffix(" h"));
            ui.horizontal(|ui| {
                save = ui::success_button(ui, "Save").clicked();
                cancel = ui::secondary_button(ui, "Cancel").clicked();
            });
        });
        if save {
            let f = self.usage_form.clone().unwrap();
            let r = self.svc.record_usage(&f);
            if self.report(r, "Usage recorded.").is_some() {
                self.usage_form = None;
            }
        } else if cancel {
            self.usage_form = None;
        }
    }

    pub fn task_window(&mut self, ui: &mut egui::Ui) {
        let Some(f) = &mut self.task_form else { return };
        let accts = self.accounts.clone();
        let all_tasks = self.tasks.clone();
        let templates = plan_burner_core::templates::list(&self.svc.conn).unwrap_or_default();
        let mut manage = false;
        let (mut cancel, mut save_tpl, mut suggest) = (false, false, false);
        let mut apply_tpl: Option<usize> = None;
        let title = if f.id.is_some() {
            "Edit task"
        } else {
            "New task"
        };
        let back = ui::editor_page(ui, title, |ui| {
            if f.id.is_none() {
                ui.horizontal(|ui| {
                    ui.label("Load template");
                    egui::ComboBox::from_id_salt("load_tpl")
                        .selected_text(if templates.is_empty() {
                            "no templates yet"
                        } else {
                            "Choose a template…"
                        })
                        .show_ui(ui, |ui| {
                            for (i, t) in templates.iter().enumerate() {
                                if ui.selectable_label(false, &t.name).clicked() {
                                    apply_tpl = Some(i);
                                }
                            }
                        });
                    if ui.small_button("Manage templates…").clicked() {
                        manage = true;
                    }
                });
                if templates.is_empty() {
                    ui.label(
                        RichText::new("Create templates under Templates, or fill in this form and use “Save as template” below.")
                            .small()
                            .color(crate::theme::MUTED),
                    );
                }
            }
            ui.horizontal(|ui| {
                ui.label("Name");
                ui::required_marker(ui);
                ui.text_edit_singleline(&mut f.t.name);
            });
            ui.label("Description");
            ui.text_edit_singleline(&mut f.t.description);
            ui.horizontal(|ui| {
                ui.label("Prompt");
                ui::required_marker(ui);
            });
            ui.add(
                egui::TextEdit::multiline(&mut f.t.prompt)
                    .desired_rows(6)
                    .desired_width(f32::INFINITY),
            );
            ui.separator();
            ui.label(RichText::new("Target").strong());
            ui.horizontal(|ui| {
                ui.checkbox(&mut f.any_provider, "Any provider");
                if !f.any_provider {
                    let providers: Vec<String> = plan_burner_core::provider::registry()
                        .iter()
                        .map(|a| a.provider_type().to_string())
                        .collect();
                    let cur =
                        f.t.provider_type
                            .clone()
                            .unwrap_or_else(|| providers.first().cloned().unwrap_or_default());
                    egui::ComboBox::from_id_salt("tprov")
                        .selected_text(&cur)
                        .show_ui(ui, |ui| {
                            for p in &providers {
                                ui.selectable_value(&mut f.t.provider_type, Some(p.clone()), p);
                            }
                        });
                    if f.t.provider_type.is_none() {
                        f.t.provider_type = Some(cur.clone());
                    }
                    let label = f
                        .pinned
                        .and_then(|id| accts.iter().find(|a| a.id == id))
                        .map(|a| a.label.clone())
                        .unwrap_or_else(|| "Any account".into());
                    egui::ComboBox::from_id_salt("tacct")
                        .selected_text(label)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut f.pinned, None, "Any account");
                            for a in accts
                                .iter()
                                .filter(|a| Some(&a.provider_type) == f.t.provider_type.as_ref())
                            {
                                ui.selectable_value(&mut f.pinned, Some(a.id), &a.label);
                            }
                        });
                    ui.checkbox(&mut f.t.allow_fallback, "Allow fallback to other account");
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Run mode");
                crate::ui::run_mode_combo(
                    ui,
                    "task_run_mode",
                    &mut f.t.run_mode,
                    "Account/global default",
                );
            });
            ui.horizontal(|ui| {
                ui.label("Model");
                ui.text_edit_singleline(&mut f.t.model);
                egui::ComboBox::from_id_salt("prio")
                    .selected_text(ui::priority_label(f.t.priority))
                    .show_ui(ui, |ui| {
                        for p in [2, 1, 0] {
                            ui.selectable_value(&mut f.t.priority, p, ui::priority_label(p));
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut f.t.estimated_consumption)
                        .range(0.0..=100.0)
                        .prefix("est. consumption ")
                        .suffix(" %"),
                );
                if ui.small_button("Suggest from history").clicked() {
                    suggest = true;
                }
                ui.add(
                    egui::DragValue::new(&mut f.t.estimated_minutes)
                        .range(1..=1440)
                        .prefix("est. ")
                        .suffix(" min"),
                );
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut f.budget_on, "Maximum budget");
                ui.add_enabled(
                    f.budget_on,
                    egui::DragValue::new(&mut f.budget)
                        .range(0.0..=100.0)
                        .suffix(" %"),
                );
                ui.add(
                    egui::DragValue::new(&mut f.timeout_min)
                        .range(1.0..=1440.0)
                        .prefix("timeout ")
                        .suffix(" min"),
                );
                ui.add(
                    egui::DragValue::new(&mut f.t.retry_max)
                        .range(0..=5)
                        .prefix("retries "),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Working dir (inside allowed root)");
                crate::ui::path_field(
                    ui,
                    &mut f.t.working_dir,
                    plan_burner_core::pick::PickKind::Dir,
                    None,
                );
            });
            ui.horizontal(|ui| {
                ui.label("Depends on");
                let chosen = f.dep_ids();
                let own_id = f.id;
                let summary = if chosen.is_empty() {
                    "no dependencies".to_string()
                } else {
                    plan_burner_core::present::dependency_names(&all_tasks, &chosen)
                };
                ui.menu_button(summary, |ui| {
                    ui.set_max_height(280.0);
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for t in all_tasks.iter().filter(|t| Some(t.id) != own_id) {
                            let mut on = chosen.contains(&t.id);
                            if ui
                                .checkbox(&mut on, format!("#{} {}", t.id, t.name))
                                .changed()
                            {
                                f.toggle_dep(t.id);
                            }
                        }
                        if all_tasks.len() <= usize::from(own_id.is_some()) {
                            ui.label("No other tasks yet.");
                        }
                    });
                });
            });
            ui.horizontal(|ui| {
                ui.label("Not before")
                    .on_hover_text("Blank = as soon as possible. after:<time> = not before that moment; daily:HH:MM = not before that local time each day (does not repeat).");
                let mut s = f.t.schedule.clone().unwrap_or_default();
                if ui::checked_text(
                    ui,
                    &mut s,
                    "after:2026-01-31T09:30:00Z  or  daily:09:00",
                    "after:<RFC 3339 time> or daily:HH:MM (local time); blank = anytime",
                    ui::check::task_schedule,
                )
                .changed()
                {
                    f.t.schedule = (!s.is_empty()).then_some(s);
                }
            });
            ui.horizontal(|ui| {
                ui.label("Repeat").on_hover_text(
                    "When the task finishes (done, failed or expired; not cancelled) the next copy is queued. Blank = runs once.",
                );
                ui::checked_text(
                    ui,
                    &mut f.repeat,
                    "every:6h  or  daily:09:00  or  weekly:mon-fri@18:30",
                    "every:<N><m|h|d|w>, daily:HH:MM or weekly:<days>@HH:MM (local time); blank = once",
                    ui::check::task_repeat,
                );
            });
            ui.horizontal(|ui| {
                ui.label("Expires").on_hover_text(
                    "The task is dropped if it has not started by this time. Blank = never.",
                );
                ui::checked_text(
                    ui,
                    &mut f.expires,
                    "2026-01-31T09:30:00Z",
                    "RFC 3339 time; blank = never expires",
                    ui::check::expiry,
                );
            });
            ui.checkbox(
                &mut f.t.requires_approval,
                "Always require approval for this task",
            );
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut f.template_name)
                        .hint_text("Template name (required to save a template)"),
                );
                save_tpl = ui.button("Save as template").clicked();
            });
        });
        // Save / Cancel live in the pinned bar (`task_action_bar`).
        cancel |= back;
        // Apply deferred actions.
        if let Some(i) = apply_tpl {
            let t = &templates[i];
            self.task_form.as_mut().unwrap().apply_template(t);
            self.status = Some((format!("Template “{}” loaded.", t.name), true));
        }
        if suggest {
            let model = self.task_form.as_ref().unwrap().t.model.clone();
            let cur = self.task_form.as_ref().unwrap().t.estimated_consumption;
            if let Ok(e) = analytics::suggest_estimate(&self.svc.conn, &model, cur) {
                self.task_form.as_mut().unwrap().t.estimated_consumption = e.value;
                self.status = Some((
                    format!(
                        "Estimate {:.1}% from {} measured run(s); confidence {}",
                        e.value, e.samples, e.confidence
                    ),
                    e.samples > 0,
                ));
            }
        }
        if save_tpl {
            let f = self.task_form.clone().unwrap();
            let r = TemplateForm::from_task(&f)
                .to_template()
                .and_then(|(name, n)| self.svc.save_template(&name, &n));
            self.report(
                r,
                "Template saved (a template with the same name was replaced).",
            );
        }
        if manage {
            self.view = crate::View::Templates;
        }
        if cancel {
            self.task_form = None;
        }
    }

    /// Pinned Save / Cancel bar for the task editor (always visible, Ctrl+Enter saves).
    pub fn task_action_bar(&mut self, ctx: &egui::Context) {
        if self.view != crate::View::Queue || self.task_form.is_none() {
            return;
        }
        let (mut save, mut cancel) = (false, false);
        egui::TopBottomPanel::bottom("task_actions")
            .frame(
                egui::Frame::new()
                    .fill(crate::theme::TITLE_BAR)
                    .inner_margin(egui::Margin::symmetric(24, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    save = ui::success_button(ui, "Save").clicked();
                    cancel = ui::secondary_button(ui, "Cancel").clicked();
                    ui.label(
                        RichText::new("Ctrl+Enter saves · * required")
                            .small()
                            .color(crate::theme::MUTED),
                    );
                });
            });
        save |= ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter));
        if save {
            let f = self.task_form.clone().unwrap();
            let res = self.svc.save_task(&f);
            if self.report(res, "Task saved.").is_some() {
                self.task_form = None;
            }
        } else if cancel {
            self.task_form = None;
        }
    }
}

/// Small square button with a painted up/down triangle. Drawn with the painter
/// because the default egui fonts do not reliably cover ▲/▼, which made both
/// buttons render the same placeholder glyph.
fn arrow_button(ui: &mut egui::Ui, up: bool) -> egui::Response {
    let size = egui::vec2(ui.spacing().interact_size.y, ui.spacing().interact_size.y);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        ui.painter().rect(
            rect.expand(visuals.expansion),
            visuals.corner_radius,
            visuals.weak_bg_fill,
            visuals.bg_stroke,
            egui::StrokeKind::Inside,
        );
        let c = rect.center();
        let (w, h) = (rect.width() * 0.22, rect.height() * 0.16);
        let pts = if up {
            vec![
                egui::pos2(c.x, c.y - h),
                egui::pos2(c.x + w, c.y + h),
                egui::pos2(c.x - w, c.y + h),
            ]
        } else {
            vec![
                egui::pos2(c.x, c.y + h),
                egui::pos2(c.x - w, c.y - h),
                egui::pos2(c.x + w, c.y - h),
            ]
        };
        ui.painter().add(egui::Shape::convex_polygon(
            pts,
            visuals.fg_stroke.color,
            egui::Stroke::NONE,
        ));
        if response.has_focus() {
            ui.painter().rect_stroke(
                rect.expand(2.0),
                visuals.corner_radius,
                egui::Stroke::new(2.0_f32, ui::BLUE),
                egui::StrokeKind::Outside,
            );
        }
    }
    // Screen readers get a name for the painted triangle.
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            true,
            if up { "Move up" } else { "Move down" },
        )
    });
    response
}
