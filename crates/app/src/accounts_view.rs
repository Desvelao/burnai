use crate::theme::CLAY;
use crate::ui::{self, badge, GREY, RED};
use crate::{AccountForm, App};
use chrono::Utc;
use eframe::egui::{self, RichText};
use plan_burner_core::claude_usage;
use plan_burner_core::settings::Settings;
use plan_burner_core::{models::*, usage};

impl App {
    fn creds_text(&self, a: &Account) -> (String, egui::Color32) {
        let (t, l) = plan_burner_core::present::credentials_summary(a, &self.svc.env, Utc::now());
        (t, ui::level_color(l))
    }

    pub fn accounts_view(&mut self, ui: &mut egui::Ui) {
        ui::page_header(ui, "Accounts", |ui| {
            if ui::primary_button(ui, "Add account…").clicked() {
                self.account_form = Some(AccountForm::new());
            }
        });
        ui.label(RichText::new("Automatic Claude usage reads the OAuth token from the account's Claude Code credentials file (read-only; never refreshed, stored or shown) and queries an unofficial claude.ai endpoint.").small().color(GREY));
        if self.accounts.is_empty() {
            ui.label("No accounts yet.");
        }
        for a in self.accounts.clone() {
            ui::card(ui, Some(ui::provider_color(&a.provider_type)), |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(a.display()).strong().size(16.0));
                    badge(
                        ui,
                        ui::billing_label(a.billing_mode),
                        ui::billing_color(a.billing_mode),
                    );
                    badge(
                        ui,
                        if a.usage_source == UsageSource::ClaudeOauth {
                            "usage: automatic (unofficial)"
                        } else if a.usage_source == UsageSource::CustomScript {
                            "usage: custom script"
                        } else {
                            "usage: manual"
                        },
                        GREY,
                    );
                    ui.label(if a.subscription_plan.is_empty() {
                        "plan unknown".to_string()
                    } else {
                        a.subscription_plan.clone()
                    });
                });
                let (t, c) = self.creds_text(&a);
                ui.colored_label(c, t);
                if let Some(e) = self.svc.usage_errors.lock().unwrap().get(&a.id) {
                    ui.colored_label(RED, format!("Last usage fetch failed: {e}"));
                } else if let Ok(Some(u)) = usage::latest(&self.svc.conn, a.id) {
                    ui.label(
                        RichText::new(format!(
                            "Last reading {} min ago ({}, {})",
                            (Utc::now() - u.captured_at).num_minutes(),
                            u.confidence,
                            u.source
                        ))
                        .small()
                        .color(GREY),
                    );
                }
                ui.horizontal(|ui| {
                    if ui::primary_button(ui, "Edit").clicked() {
                        self.account_form = Some(AccountForm::from_account(&a));
                    }
                    self.burn_controls(ui, Some(&a));
                    if a.polls() && ui::primary_button(ui, "Refresh now").clicked() {
                        let r = self.svc.refresh_usage(a.id);
                        self.report(r, "Fetching usage…");
                    }
                    if self.confirm_delete == Some(a.id) {
                        match ui::inline_confirm(ui, "Delete this account?", "Yes, delete") {
                            Some(true) => {
                                self.confirm_delete = None;
                                let r = self.svc.delete_account(a.id);
                                self.report(r, "Account deleted.");
                            }
                            Some(false) => self.confirm_delete = None,
                            None => {}
                        }
                    } else if ui::secondary_button(ui, "Delete").clicked() {
                        self.confirm_delete = Some(a.id);
                    }
                });
            });
        }
    }

    pub fn account_window(&mut self, ui: &mut egui::Ui) {
        let Some(f) = &mut self.account_form else {
            return;
        };
        let general = self.settings.clone();
        let editing = f.id.is_some();
        let (mut save, mut cancel, mut discover) = (false, false, false);
        let mut pick: Option<usize> = None;
        let providers: Vec<String> = plan_burner_core::provider::registry()
            .iter()
            .map(|a| a.provider_type().to_string())
            .collect();
        // New Claude accounts: look for logins once when the form opens.
        if !editing && !f.discovery_done && f.provider != "custom" {
            f.discovery_done = true;
            discover = true;
        }
        let ch = f.changed_fields();
        let mut back = false;
        ui::page_header(
            ui,
            if editing {
                "Edit account"
            } else {
                "Add account"
            },
            |ui| back = ui::secondary_button(ui, "← Back").clicked(),
        );
        let body_h = (ui.available_height() - 56.0).max(120.0);
        egui::ScrollArea::vertical()
            .max_height(body_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if editing {
                    if ui.available_width() > 1000.0 {
                        ui.columns(3, |c| {
                            section(&mut c[0], "Identity", None, false, true, |ui| {
                                identity(ui, f, &ch, &providers)
                            });
                            section(&mut c[0], "Connection", None, false, true, |ui| {
                                connection(ui, f, &ch)
                            });
                            section(&mut c[1], "Usage tracking", None, false, true, |ui| {
                                usage(ui, f, &ch)
                            });
                            section(&mut c[1], "Availability", None, false, true, |ui| {
                                availability(ui, f, &ch)
                            });
                            let n = f.limit_override_count();
                            section(
                                &mut c[2],
                                "Eligibility limits",
                                Some(&overrides_text(n)),
                                false,
                                true,
                                |ui| limits(ui, f, &ch, &general),
                            );
                        });
                    } else {
                        section(ui, "Identity", None, false, true, |ui| {
                            identity(ui, f, &ch, &providers)
                        });
                        section(ui, "Connection", None, false, true, |ui| {
                            connection(ui, f, &ch)
                        });
                        section(ui, "Usage tracking", None, false, true, |ui| {
                            usage(ui, f, &ch)
                        });
                        section(ui, "Availability", None, false, true, |ui| {
                            availability(ui, f, &ch)
                        });
                        let n = f.limit_override_count();
                        section(
                            ui,
                            "Eligibility limits",
                            Some(&overrides_text(n)),
                            false,
                            true,
                            |ui| limits(ui, f, &ch, &general),
                        );
                    }
                } else {
                    ui.set_max_width(760.0);
                    if f.provider != "custom" {
                        section(ui, "Found on this machine", None, false, true, |ui| {
                            found_logins(ui, f, &mut discover, &mut pick)
                        });
                    }
                    section(ui, "Account", None, false, true, |ui| {
                        identity(ui, f, &ch, &providers)
                    });
                    let custom = f.provider == "custom";
                    let s = f.connection_summary();
                    section(ui, "Connection details", Some(&s), true, custom, |ui| {
                        connection(ui, f, &ch)
                    });
                    let s = f.usage_summary();
                    section(ui, "Usage tracking", Some(&s), true, false, |ui| {
                        usage(ui, f, &ch)
                    });
                    let s = f.availability_summary();
                    section(ui, "Availability schedule", Some(&s), true, false, |ui| {
                        availability(ui, f, &ch)
                    });
                    let n = f.limit_override_count();
                    section(
                        ui,
                        "Eligibility limits",
                        Some(&overrides_text(n)),
                        true,
                        n > 0,
                        |ui| limits(ui, f, &ch, &general),
                    );
                }
            });
        ui.separator();
        ui.horizontal(|ui| {
            if !ch.is_empty() {
                ui.label(
                    RichText::new(format!(
                        "● {} unsaved change{}",
                        ch.len(),
                        if ch.len() == 1 { "" } else { "s" }
                    ))
                    .color(CLAY),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                save =
                    ui::success_button(ui, if editing { "Save" } else { "Add account" }).clicked();
                cancel = ui::secondary_button(ui, "Cancel").clicked();
            });
        });
        cancel |= back;
        if discover {
            let found = claude_usage::discover_profiles(&self.svc.env);
            if found.is_empty() {
                self.status = Some(("No Claude logins found under ~/.claude*.".into(), false));
            }
            self.account_form.as_mut().unwrap().discovered = found;
        }
        if let Some(i) = pick {
            let f = self.account_form.as_mut().unwrap();
            let d = f.discovered[i].clone();
            f.apply_discovered(&d);
        }
        if save {
            let f = self.account_form.as_ref().unwrap();
            let r = self.svc.save_account(f);
            if self.report(r, "Account saved.").is_some() {
                self.account_form = None;
            }
        } else if cancel {
            self.account_form = None;
        }
    }
}

fn overrides_text(n: usize) -> String {
    match n {
        0 => "all inherited".into(),
        1 => "1 override".into(),
        n => format!("{n} overrides"),
    }
}

/// A card with a heading; optionally collapsible, with a one-line summary beside the heading.
fn section(
    ui: &mut egui::Ui,
    title: &str,
    summary: Option<&str>,
    collapsible: bool,
    open: bool,
    add: impl FnOnce(&mut egui::Ui),
) {
    ui::card(ui, None, |ui| {
        ui.set_width(ui.available_width());
        let head = |ui: &mut egui::Ui| {
            ui.label(RichText::new(title).strong());
            if let Some(s) = summary {
                ui.label(RichText::new(s).small().color(GREY));
            }
        };
        if collapsible {
            let id = ui.make_persistent_id(("acct_section", title));
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, open)
                .show_header(ui, head)
                .body(add);
        } else {
            ui.horizontal(head);
            ui.add_space(4.0);
            add(ui);
        }
    });
    ui.add_space(4.0);
}

/// Field label with an orange dot when the value differs from what the form was opened with.
fn lbl(ui: &mut egui::Ui, text: &str, changed: bool) {
    ui.horizontal(|ui| {
        ui.label(text);
        if changed {
            ui.label(RichText::new("●").small().color(CLAY))
                .on_hover_text("Modified");
        }
    });
}

fn text_row(ui: &mut egui::Ui, label: &str, hint: &str, changed: bool, v: &mut String) {
    lbl(ui, label, changed);
    let w = ui.available_width();
    ui.add(
        egui::TextEdit::singleline(v)
            .hint_text(hint)
            .desired_width(w),
    );
}

fn found_logins(ui: &mut egui::Ui, f: &AccountForm, discover: &mut bool, pick: &mut Option<usize>) {
    if f.discovered.is_empty() {
        ui.label(
            RichText::new(
                "No Claude logins found under ~/.claude*. Enter a profile dir under Connection details.",
            )
            .small()
            .color(GREY),
        );
    }
    ui.horizontal_wrapped(|ui| {
        for (i, d) in f.discovered.iter().enumerate() {
            let text = format!(
                "{} ({})",
                d.profile_dir,
                d.subscription_type
                    .clone()
                    .unwrap_or_else(|| "unknown plan".into())
            );
            let chosen = f.config_dir == d.profile_dir;
            let r = if chosen {
                ui::primary_button(ui, &text)
            } else {
                ui::secondary_button(ui, &text)
            };
            if r.clicked() {
                *pick = Some(i);
            }
        }
        if ui::secondary_button(ui, "Rescan ~/.claude*").clicked() {
            *discover = true;
        }
    });
}

fn identity(ui: &mut egui::Ui, f: &mut AccountForm, ch: &[&str], providers: &[String]) {
    let has = |k: &str| ch.contains(&k);
    ui.horizontal(|ui| {
        ui.add_enabled_ui(f.id.is_none(), |ui| {
            egui::ComboBox::from_label("Provider")
                .selected_text(&f.provider)
                .show_ui(ui, |ui| {
                    for p in providers {
                        ui.selectable_value(&mut f.provider, p.clone(), p);
                    }
                });
        });
        if f.id.is_some() {
            badge(ui, "locked", GREY);
        }
    });
    text_row(ui, "Label", "e.g. personal", has("label"), &mut f.label);
    text_row(
        ui,
        "Plan (blank = from login)",
        "pro, max5x …",
        has("plan"),
        &mut f.plan,
    );
}

fn connection(ui: &mut egui::Ui, f: &mut AccountForm, ch: &[&str]) {
    let has = |k: &str| ch.contains(&k);
    if f.provider == "custom" {
        lbl(ui, "Script (absolute path)", has("script"));
        ui.horizontal(|ui| {
            crate::ui::path_field(
                ui,
                &mut f.script,
                plan_burner_core::pick::PickKind::File,
                None,
            );
        });
        lbl(ui, "Fixed arguments (one per line)", has("script_args"));
        let w = ui.available_width();
        ui.add(
            egui::TextEdit::multiline(&mut f.script_args)
                .desired_rows(2)
                .desired_width(w),
        );
        lbl(ui, "Pull timeout (s)", has("script_timeout"));
        ui.add(egui::DragValue::new(&mut f.script_timeout).range(1..=600));
        ui.label(RichText::new("The script is called as `script [args] <usage|status|run> [--k=v]` (see docs/CUSTOM_PROVIDER.md). It runs with your privileges; changing it resets the subscription confirmation.").small().color(GREY));
        return;
    }
    lbl(ui, "Profile dir (CLAUDE_CONFIG_DIR)", has("config_dir"));
    ui.horizontal(|ui| {
        crate::ui::path_field(
            ui,
            &mut f.config_dir,
            plan_burner_core::pick::PickKind::Dir,
            None,
        );
    });
    text_row(
        ui,
        "CLI command (default: claude)",
        "claude",
        has("cli_command"),
        &mut f.cli_command,
    );
    text_row(
        ui,
        "Credentials file (optional)",
        "",
        has("credentials_file"),
        &mut f.credentials_file,
    );
    ui.label(
        RichText::new("Changing the profile dir, credentials file or CLI command resets the subscription confirmation. The CLI command is a bare program name (looked up in PATH) or an absolute path, run without a shell.")
            .small()
            .color(ui::AMBER),
    );
}

fn usage(ui: &mut egui::Ui, f: &mut AccountForm, ch: &[&str]) {
    let has = |k: &str| ch.contains(&k);
    let custom = f.provider == "custom";
    ui.horizontal(|ui| {
        ui.checkbox(
            &mut f.auto_usage,
            if custom {
                "call `usage` periodically (else enter manually)"
            } else {
                "read usage with the login token (unofficial endpoint)"
            },
        );
        if has("auto_usage") {
            ui.label(RichText::new("●").small().color(CLAY));
        }
    });
    if !custom {
        text_row(
            ui,
            "User-Agent override",
            "blank = the plugin's Chrome string",
            has("user_agent"),
            &mut f.user_agent,
        );
        ui.label(RichText::new("The endpoint sits behind Cloudflare bot checks, so a blank User-Agent uses the plugin's Chrome string.").small().color(GREY));
    }
}

fn availability(ui: &mut egui::Ui, f: &mut AccountForm, ch: &[&str]) {
    lbl(ui, "Schedule", ch.contains(&"schedule"));
    ui::checked_text(
        ui,
        &mut f.availability_schedule,
        "mon-fri 09:00-17:00; sat,sun 10:00-14:00",
        "`;`-separated rules `days HH:MM-HH:MM` in local time. Blank = always available.",
        ui::check::availability,
    );
    ui.label(RichText::new("`;`-separated rules `days HH:MM-HH:MM` in local time (days: mon-fri, sat,sun, fri-mon, or * for every day; windows may cross midnight). Blank = always available. The Available switch on the dashboard overrides it.").small().color(GREY));
}

fn limits(ui: &mut egui::Ui, f: &mut AccountForm, ch: &[&str], general: &Settings) {
    let has = |k: &str| ch.contains(&k);
    let provider = f.provider.clone();
    let cw = (ui.available_width() * 0.5).min(300.0);
    let l = &mut f.limits;
    ui.label(RichText::new("Tick a box to override the general setting for this account; unticked values are inherited. Stale or unknown usage, billing safety and the capacity a task needs are always enforced.").small().color(GREY));
    ui.horizontal(|ui| {
        ui.checkbox(
            &mut l.ignore_limits,
            RichText::new("Eligible ignoring limits (start threshold, reserve, idle, hours)")
                .color(ui::AMBER),
        );
        if has("ignore_limits") {
            ui.label(RichText::new("●").small().color(CLAY));
        }
    });
    egui::Grid::new("acct_limits")
        .num_columns(2)
        .min_col_width(cw)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            override_f64(
                ui,
                "Start when capacity above (%)",
                has("lim_start"),
                &mut l.start_threshold,
                general.minimum_capacity_threshold,
            );
            override_f64(
                ui,
                "Reserved capacity (%)",
                has("lim_reserved"),
                &mut l.reserved_capacity,
                general.reserved_capacity,
            );
            override_u32(
                ui,
                "Run on idle (min, 0 = any time)",
                has("lim_idle"),
                &mut l.idle_requirement,
                general.idle_requirement,
                0..=1440,
            );
            override_text(
                ui,
                "Allowed hours",
                has("lim_hours"),
                &mut l.allowed_hours,
                &general.allowed_hours,
            );
            override_u32(
                ui,
                "Max usage age (min)",
                has("lim_age"),
                &mut l.max_usage_age_minutes,
                general.max_usage_age_minutes,
                1..=1440,
            );
            override_u32(
                ui,
                "Max tasks per batch",
                has("lim_batch"),
                &mut l.max_tasks_per_batch,
                general.maximum_tasks_per_batch,
                1..=100,
            );
            override_u32(
                ui,
                "Max batch minutes",
                has("lim_minutes"),
                &mut l.max_execution_minutes,
                general.maximum_execution_minutes,
                1..=1440,
            );
            if provider == "claude" {
                lbl(ui, "5 h limit", has("ignore_session"));
                ui.checkbox(
                    &mut l.ignore_session,
                    "ignore the 5 h window (decide on weekly only)",
                );
                ui.end_row();
            }
        });
    if provider == "claude" {
        // A separate grid inside the enabled scope: a nested Ui inside one grid breaks its rows.
        ui.add_enabled_ui(!l.ignore_session, |ui| {
            egui::Grid::new("acct_limits_5h")
                .num_columns(2)
                .min_col_width(cw)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    override_f64(
                        ui,
                        "5 h: start when above (%)",
                        has("lim_sess_thr"),
                        &mut l.session_threshold,
                        l.start_threshold
                            .unwrap_or(general.minimum_capacity_threshold),
                    );
                    override_f64(
                        ui,
                        "5 h: reserved capacity (%)",
                        has("lim_sess_res"),
                        &mut l.session_reserved,
                        l.reserved_capacity.unwrap_or(general.reserved_capacity),
                    );
                });
        });
    }
    egui::Grid::new("acct_limits_burn")
        .num_columns(2)
        .min_col_width(cw)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            if provider == "claude" {
                override_hours(
                    ui,
                    "Burn 5 h window (h before reset)",
                    has("lim_burn_sess"),
                    &mut l.burn_session_hours,
                    general.burn_session_hours,
                    5.0,
                );
            }
            override_hours(
                ui,
                "Burn weekly window (h before reset)",
                has("lim_burn_week"),
                &mut l.burn_weekly_hours,
                general.burn_weekly_hours,
                168.0,
            );
        });
}

/// "Override" checkbox + value for an optional percentage; shows the inherited value when off.
fn override_f64(
    ui: &mut egui::Ui,
    label: &str,
    changed: bool,
    v: &mut Option<f64>,
    inherited: f64,
) {
    lbl(ui, label, changed);
    ui.horizontal(|ui| {
        let mut on = v.is_some();
        if ui.checkbox(&mut on, "override").changed() {
            *v = on.then_some(inherited);
        }
        match v {
            Some(x) => {
                ui.add(egui::DragValue::new(x).range(0.0..=100.0));
            }
            None => {
                ui.label(RichText::new(format!("inherits {inherited:.0}")).color(GREY));
            }
        }
    });
    ui.end_row();
}

/// "Burn before reset" override: 0 = off. Inherits the general value (None = off) when unticked.
fn override_hours(
    ui: &mut egui::Ui,
    label: &str,
    changed: bool,
    v: &mut Option<f64>,
    inherited: Option<f64>,
    max: f64,
) {
    lbl(ui, label, changed);
    ui.horizontal(|ui| {
        let mut on = v.is_some();
        if ui.checkbox(&mut on, "override").changed() {
            *v = on.then_some(inherited.unwrap_or(0.0));
        }
        match v {
            Some(x) => {
                ui.add(egui::DragValue::new(x).range(0.0..=max).speed(0.25));
                ui.label(RichText::new("0 = off").small().color(GREY));
            }
            None => {
                let t = inherited.map_or("off".to_string(), |h| format!("{h}"));
                ui.label(RichText::new(format!("inherits {t}")).color(GREY));
            }
        }
    });
    ui.end_row();
}

fn override_u32(
    ui: &mut egui::Ui,
    label: &str,
    changed: bool,
    v: &mut Option<u32>,
    inherited: u32,
    range: std::ops::RangeInclusive<u32>,
) {
    lbl(ui, label, changed);
    ui.horizontal(|ui| {
        let mut on = v.is_some();
        if ui.checkbox(&mut on, "override").changed() {
            *v = on.then_some(inherited);
        }
        match v {
            Some(x) => {
                ui.add(egui::DragValue::new(x).range(range));
            }
            None => {
                ui.label(RichText::new(format!("inherits {inherited}")).color(GREY));
            }
        }
    });
    ui.end_row();
}

fn override_text(
    ui: &mut egui::Ui,
    label: &str,
    changed: bool,
    v: &mut Option<String>,
    inherited: &str,
) {
    lbl(ui, label, changed);
    ui.horizontal(|ui| {
        let mut on = v.is_some();
        if ui.checkbox(&mut on, "override").changed() {
            *v = on.then(|| inherited.to_string());
        }
        match v {
            Some(x) => {
                ui::checked_text(
                    ui,
                    x,
                    "22:00-07:00, blank = always",
                    "Local time window in which this account may start tasks. Blank = any time.",
                    ui::check::hours,
                );
            }
            None => {
                let t = if inherited.is_empty() {
                    "always"
                } else {
                    inherited
                };
                ui.label(RichText::new(format!("inherits {t}")).color(GREY));
            }
        }
    });
    ui.end_row();
}
