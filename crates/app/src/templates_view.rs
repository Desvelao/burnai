//! Task templates: list, create, edit, duplicate, delete, and start a task from one.
use crate::ui::{self, GREY};
use crate::{App, View};
use eframe::egui::{self, RichText};
use plan_burner_core::forms::{TaskForm, TemplateForm};
use plan_burner_core::templates;

impl App {
    pub fn templates_view(&mut self, ui: &mut egui::Ui) {
        ui::page_header(ui, "Task templates", |ui| {
            if ui::primary_button(ui, "New template").clicked() {
                self.template_form = Some(TemplateForm::new());
            }
        });
        ui.label(
            RichText::new("A template holds default values for new tasks (prompt, model, estimates, priority, run mode, budget, timeout, retries, working dir, approval, provider). It never stores a target account, schedule or dependencies.")
                .small()
                .color(GREY),
        );
        let list = templates::list(&self.svc.conn).unwrap_or_default();
        if list.is_empty() {
            ui.label(
                "No templates yet. Create one here, or use “Save as template” in the task form.",
            );
        }
        for t in list {
            ui::card(ui, None, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&t.name).strong().size(16.0));
                    ui::badge(
                        ui,
                        t.task.provider_type.as_deref().unwrap_or("any provider"),
                        GREY,
                    );
                    ui.label(
                        RichText::new(format!(
                            "{} · ~{:.0}% · {} min · {} priority",
                            if t.task.model.is_empty() {
                                "default model"
                            } else {
                                &t.task.model
                            },
                            t.task.estimated_consumption,
                            t.task.estimated_minutes,
                            plan_burner_core::present::priority_label(t.task.priority)
                        ))
                        .small()
                        .color(GREY),
                    );
                });
                let first = t.task.prompt.lines().next().unwrap_or("");
                ui.label(RichText::new(first).small());
                ui.horizontal(|ui| {
                    if ui::success_button(ui, "Use for a new task").clicked() {
                        self.task_form = Some(TaskForm::from_template(&t));
                        self.view = View::Queue;
                    }
                    if ui::primary_button(ui, "Queue now").clicked() {
                        let r = self.svc.queue_from_template(t.id);
                        self.report(r, "Task queued from template.");
                    }
                    if ui::primary_button(ui, "Edit").clicked() {
                        self.template_form = Some(TemplateForm::from_template(&t));
                    }
                    if ui::secondary_button(ui, "Duplicate").clicked() {
                        let r = self.svc.duplicate_template(t.id);
                        self.report(r, "Template duplicated.");
                    }
                    if self.confirm_delete_template == Some(t.id) {
                        match ui::inline_confirm(ui, "Delete this template?", "Yes, delete") {
                            Some(true) => {
                                self.confirm_delete_template = None;
                                let r = self.svc.delete_template(t.id);
                                self.report(r, "Template deleted.");
                            }
                            Some(false) => self.confirm_delete_template = None,
                            None => {}
                        }
                    } else if ui::secondary_button(ui, "Delete").clicked() {
                        self.confirm_delete_template = Some(t.id);
                    }
                });
            });
        }
    }

    pub fn template_editor(&mut self, ui: &mut egui::Ui) {
        let Some(f) = &mut self.template_form else {
            return;
        };
        let (mut save, mut cancel) = (false, false);
        let title = if f.id.is_some() {
            "Edit template"
        } else {
            "New template"
        };
        let back = ui::editor_page(ui, title, |ui| {
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut f.name);
            });
            let t = &mut f.task;
            ui.label("Description");
            ui.text_edit_singleline(&mut t.t.description);
            ui.label("Prompt");
            ui.add(
                egui::TextEdit::multiline(&mut t.t.prompt)
                    .desired_rows(6)
                    .desired_width(f32::INFINITY),
            );
            ui.separator();
            ui.horizontal(|ui| {
                ui.checkbox(&mut t.any_provider, "Any provider");
                if !t.any_provider {
                    let providers: Vec<String> = plan_burner_core::provider::registry()
                        .iter()
                        .map(|a| a.provider_type().to_string())
                        .collect();
                    let cur =
                        t.t.provider_type
                            .clone()
                            .unwrap_or_else(|| providers.first().cloned().unwrap_or_default());
                    egui::ComboBox::from_id_salt("tpl_prov")
                        .selected_text(&cur)
                        .show_ui(ui, |ui| {
                            for p in &providers {
                                ui.selectable_value(&mut t.t.provider_type, Some(p.clone()), p);
                            }
                        });
                    if t.t.provider_type.is_none() {
                        t.t.provider_type = Some(cur);
                    }
                    ui.checkbox(&mut t.t.allow_fallback, "Allow fallback to other account");
                }
            });
            ui.horizontal(|ui| {
                ui.label("Run mode");
                ui::run_mode_combo(
                    ui,
                    "tpl_run_mode",
                    &mut t.t.run_mode,
                    "Account/global default",
                );
                ui.label("Model");
                ui.text_edit_singleline(&mut t.t.model);
                egui::ComboBox::from_id_salt("tpl_prio")
                    .selected_text(ui::priority_label(t.t.priority))
                    .show_ui(ui, |ui| {
                        for p in [2, 1, 0] {
                            ui.selectable_value(&mut t.t.priority, p, ui::priority_label(p));
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut t.t.estimated_consumption)
                        .range(0.0..=100.0)
                        .prefix("est. consumption ")
                        .suffix(" %"),
                );
                ui.add(
                    egui::DragValue::new(&mut t.t.estimated_minutes)
                        .range(1..=1440)
                        .prefix("est. ")
                        .suffix(" min"),
                );
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut t.budget_on, "Maximum budget");
                ui.add_enabled(
                    t.budget_on,
                    egui::DragValue::new(&mut t.budget)
                        .range(0.0..=100.0)
                        .suffix(" %"),
                );
                ui.add(
                    egui::DragValue::new(&mut t.timeout_min)
                        .range(1.0..=1440.0)
                        .prefix("timeout ")
                        .suffix(" min"),
                );
                ui.add(
                    egui::DragValue::new(&mut t.t.retry_max)
                        .range(0..=5)
                        .prefix("retries "),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Working dir (inside allowed root)");
                crate::ui::path_field(
                    ui,
                    &mut t.t.working_dir,
                    plan_burner_core::pick::PickKind::Dir,
                    None,
                );
            });
            ui.checkbox(
                &mut t.t.requires_approval,
                "Always require approval for tasks made from this template",
            );
            ui.horizontal(|ui| {
                save = ui::success_button(ui, "Save").clicked();
                cancel = ui::secondary_button(ui, "Cancel").clicked();
            });
        });
        cancel |= back;
        if save {
            let f = self.template_form.clone().unwrap();
            let r = self.svc.save_template_form(&f);
            if self.report(r, "Template saved.").is_some() {
                self.template_form = None;
            }
        } else if cancel {
            self.template_form = None;
        }
    }
}
