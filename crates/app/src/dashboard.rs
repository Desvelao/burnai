use crate::theme::{CARD_BORDER, MUTED, OTHER_PROVIDER};
use crate::ui::{self, badge, meter, AMBER, GREEN, GREY, RED};
use crate::{AccountForm, App, Filter, Pending, UsageForm, View};
use chrono::Utc;
use eframe::egui::{self, RichText};
use plan_burner_core::{analytics, models::*, usage};

impl App {
    /// "Burn before reset" toggle plus the "Burn now" menu, for one account or (`None`) the general
    /// setting / every account. Same buttons on the dashboard cards, the Accounts page and the top bar.
    pub fn burn_controls(&mut self, ui: &mut egui::Ui, account: Option<&Account>) {
        let id = account.map(|a| a.id);
        let on = match account {
            Some(a) => plan_burner_core::present::burn_active(&self.settings, a),
            None => {
                self.settings.burn_session_hours.is_some_and(|h| h > 0.0)
                    || self.settings.burn_weekly_hours.is_some_and(|h| h > 0.0)
            }
        };
        let label = format!("Burn before reset: {}", if on { "ON" } else { "OFF" });
        let toggle = if on {
            ui::success_button(ui, &label)
        } else {
            ui::secondary_button(ui, &label)
        }
        .on_hover_text(
            "When a usage window is close to resetting (last 1 h of the 5 h window, last 24 h of the week), \
             spend what is left instead of keeping the reserve. Stale usage, billing checks and the \
             capacity a task needs still apply.",
        );
        if toggle.clicked() {
            let r = match id {
                Some(i) => self.svc.set_account_burn(i, !on),
                None => self.svc.set_burn_before_reset(!on),
            };
            let ok = r.is_ok();
            self.report(
                r,
                if on {
                    "Burn before reset turned off."
                } else {
                    "Burn before reset turned on (last 1 h of the 5 h window, last 24 h of the week)."
                },
            );
            if ok && id.is_none() {
                // keep an open Settings draft from undoing this on Save
                let (s, w) = (
                    self.settings.burn_session_hours,
                    self.settings.burn_weekly_hours,
                );
                if let Some((base, draft)) = &mut self.settings_draft {
                    if draft.burn_session_hours == base.burn_session_hours {
                        draft.burn_session_hours = s;
                    }
                    if draft.burn_weekly_hours == base.burn_weekly_hours {
                        draft.burn_weekly_hours = w;
                    }
                    base.burn_session_hours = s;
                    base.burn_weekly_hours = w;
                }
            }
        }
        if self.pending == Some(Pending::BurnNow(id)) {
            let q = match account {
                Some(a) => format!(
                    "Start a burn batch on {} now, without approval?",
                    a.display()
                ),
                None => "Start a burn batch now, without approval?".to_string(),
            };
            match ui::inline_confirm(ui, &q, "Yes, burn now") {
                Some(true) => {
                    self.pending = None;
                    self.run_burn(id, false);
                }
                Some(false) => self.pending = None,
                None => {}
            }
        } else {
            ui.menu_button(
                if id.is_some() {
                    "Burn now"
                } else {
                    "Burn now (all)"
                },
                |ui| {
                    if ui.button("Burn now with approvals").clicked() {
                        self.run_burn(id, true);
                        ui.close_menu();
                    }
                    if ui.button("Burn now without approvals").clicked() {
                        self.pending = Some(Pending::BurnNow(id));
                        ui.close_menu();
                    }
                },
            )
            .response
            .on_hover_text(
                "One batch that ignores the start threshold and reserve to spend what is left now.",
            );
        }
    }

    fn run_burn(&mut self, id: Option<i64>, with_approval: bool) {
        match self.svc.burn_now(id, with_approval) {
            Ok(rep) => {
                let did = rep.proposed + rep.started > 0;
                self.status = Some((rep.message, did));
                if with_approval && rep.proposed > 0 {
                    self.view = View::Approvals;
                }
            }
            Err(e) => self.status = Some((e.to_string(), false)),
        }
        self.refresh();
    }

    /// Proposes a batch on `account_id` and, if one is queued, jumps to Approvals (shared by the
    /// dashboard card and the Queue page's "Propose batch on…" menu).
    pub fn propose_batch_for(&mut self, account_id: i64) {
        match self.svc.propose_for(account_id) {
            Ok(Some(_)) => {
                self.status = Some(("Batch proposed. Review it under Approvals.".into(), true));
                self.view = View::Approvals;
            }
            Ok(None) => {
                let mut why: Vec<&str> = Vec::new();
                for r in self.qstatus.reasons.values() {
                    if !why.contains(&r.as_str()) && why.len() < 3 {
                        why.push(r);
                    }
                }
                let msg = if why.is_empty() {
                    "Nothing can run on this account right now: no eligible tasks are queued."
                        .to_string()
                } else {
                    format!(
                        "Nothing can run on this account right now: {}",
                        why.join(" | ")
                    )
                };
                self.status = Some((msg, false));
            }
            Err(e) => self.status = Some((e.to_string(), false)),
        }
        self.refresh();
    }

    pub fn dashboard(&mut self, ui: &mut egui::Ui) {
        ui::page_header(ui, "Dashboard", |ui| {
            let st = self.svc.run_state(Utc::now());
            badge(ui, ui::run_state_label(st), GREY);
            badge(
                ui,
                &format!("Policy: {}", self.settings.execution_mode),
                if self.settings.enabled { GREEN } else { AMBER },
            );
        });
        if self.settings.working_root.is_empty() {
            ui::banner(
                ui,
                AMBER,
                "No allowed working directory is configured (Settings). Tasks cannot run until one is set.",
            );
            ui.add_space(6.0);
        }
        let now = Utc::now();
        let accts = self.accounts.clone();
        let had_form = self.account_form.is_some();
        // Fixed-width cards flow into rows that fit the window instead of running off the right edge.
        const CARD_W: f32 = 320.0;
        const GAP: f32 = 16.0;
        let per_row = (((ui.available_width() + GAP) / (CARD_W + GAP)).floor() as usize).max(1);
        // one slot per account plus the trailing "Add account" card (`None`)
        let slots: Vec<Option<&Account>> = accts.iter().map(Some).chain([None]).collect();
        for (n, row) in slots.chunks(per_row).enumerate() {
            if n > 0 {
                ui.add_space(GAP);
            }
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                for slot in row {
                    ui.vertical(|ui| {
                        ui.set_width(CARD_W);
                        match slot {
                            Some(a) => self.account_card(ui, a, now),
                            None => add_account_card(ui, &mut self.account_form),
                        }
                    });
                }
            });
        }
        if !had_form && self.account_form.is_some() {
            self.view = View::Accounts;
        }

        ui.add_space(12.0);
        let open = |t: &plan_burner_core::tasks::Task| !t.status.is_terminal();
        ui.columns(3, |c| {
            ui::card(&mut c[0], None, |ui| {
                ui.set_min_height(130.0);
                ui::section_label(ui, "Queue by target");
                for a in &accts {
                    let n = self
                        .tasks
                        .iter()
                        .filter(|t| open(t) && t.account_id == Some(a.id))
                        .count();
                    ui.horizontal(|ui| {
                        ui::swatch(ui, ui::provider_color(&a.provider_type));
                        ui.label(format!("{} · {n} pinned", a.display()));
                    });
                }
                let any_acct = self
                    .tasks
                    .iter()
                    .filter(|t| open(t) && t.provider_type.is_some() && t.account_id.is_none())
                    .count();
                let any_prov = self
                    .tasks
                    .iter()
                    .filter(|t| open(t) && t.provider_type.is_none())
                    .count();
                ui.horizontal(|ui| {
                    ui::swatch(ui, OTHER_PROVIDER);
                    ui.label(format!("Any account of a provider · {any_acct}"));
                });
                ui.horizontal(|ui| {
                    ui::swatch(ui, OTHER_PROVIDER);
                    ui.label(format!("Any provider · {any_prov}"));
                });
                ui.separator();
                let running = self.tasks.iter().find(|t| t.status == TaskStatus::Running);
                ui.label(
                    RichText::new(match running {
                        Some(t) => format!("Running now: {}", t.name),
                        None => {
                            "Running now: nothing. One task at a time across all accounts.".into()
                        }
                    })
                    .color(MUTED)
                    .size(13.0),
                );
            });
            ui::card(&mut c[1], None, |ui| {
                ui.set_min_height(130.0);
                ui::section_label(ui, "Unused capacity (estimate)");
                for a in &accts {
                    let f = analytics::forecast(
                        &self.svc.conn,
                        a.id,
                        self.settings.effective_for(a).reserved_capacity,
                        self.settings.effective_for(a).max_usage_age_minutes as i64,
                        now,
                    );
                    match f {
                        Ok(f) => match f.unused_weekly_estimate {
                            Some(v) => {
                                ui.label(format!("{}: ~{v:.0}% weekly ({})", a.label, f.confidence))
                            }
                            None => ui.label(
                                RichText::new(format!("{}: unknown ({})", a.label, f.note))
                                    .color(GREY),
                            ),
                        },
                        Err(_) => ui.label("error"),
                    };
                }
                ui.label(
                    RichText::new("Estimates only; not provider measurements.")
                        .small()
                        .color(GREY),
                );
            });
            ui::card(&mut c[2], None, |ui| {
                ui.set_min_height(130.0);
                ui::section_label(ui, "Notifications");
                if let Ok(list) = plan_burner_core::notify::recent(&self.svc.conn, 6) {
                    if list.is_empty() {
                        ui.label(RichText::new("none yet").color(GREY));
                    }
                    for n in list {
                        ui.label(RichText::new(n.message).size(13.0));
                    }
                }
            });
        });
    }

    fn account_card(&mut self, ui: &mut egui::Ui, a: &Account, now: chrono::DateTime<Utc>) {
        let det = self.dets.get(&a.id).cloned();
        let billing = det
            .as_ref()
            .map(|d| d.billing)
            .unwrap_or(BillingMode::Unverified);
        let snap = usage::latest(&self.svc.conn, a.id).ok().flatten();
        ui::card(ui, Some(ui::provider_color(&a.provider_type)), |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui::swatch(ui, ui::provider_color(&a.provider_type));
                ui.vertical(|ui| {
                    ui.label(RichText::new(a.display()).strong().size(16.0));
                    ui.label(
                        RichText::new(if a.subscription_plan.is_empty() {
                            "plan not set".to_string()
                        } else {
                            a.subscription_plan.clone()
                        })
                        .small()
                        .color(MUTED),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    badge(ui, ui::billing_label(billing), ui::billing_color(billing));
                });
            });
            if let Some(d) = &det {
                if !d.cli_found {
                    ui.colored_label(RED, "CLI/script not available");
                }
                for r in &d.reasons {
                    ui.label(RichText::new(r).small().color(AMBER));
                }
            }
            ui.add_space(4.0);
            let (sess, week) = snap
                .as_ref()
                .map(|s| (s.session_remaining, s.weekly_remaining))
                .unwrap_or((None, None));
            let reset = |t: Option<chrono::DateTime<Utc>>| {
                usage::minutes_until(t, now).map(|m| format!("resets in {}", ui::fmt_minutes(m)))
            };
            meter(
                ui,
                "Session",
                sess,
                snap.as_ref().and_then(|s| reset(s.session_reset_at)),
            );
            meter(
                ui,
                "Weekly",
                week,
                snap.as_ref().and_then(|s| reset(s.weekly_reset_at)),
            );
            if a.polls() {
                if let Some(e) = self.svc.usage_errors.lock().unwrap().get(&a.id) {
                    ui.colored_label(RED, format!("Automatic usage failed: {e}"));
                }
            }
            match &snap {
                Some(s) => {
                    let (age, stale) = plan_burner_core::present::reading_age(
                        s,
                        now,
                        self.settings.effective_for(a).max_usage_age_minutes,
                    );
                    badge(
                        ui,
                        &format!(
                            "{}{} · {} min old{}",
                            if stale { "⚠ " } else { "" },
                            s.confidence,
                            age,
                            if stale { " · STALE: blocked" } else { "" }
                        ),
                        if stale { RED } else { AMBER },
                    );
                    ui.label(
                        RichText::new(plan_burner_core::present::provenance(s))
                            .small()
                            .color(MUTED),
                    );
                }
                None => {
                    badge(ui, "⚠ No usage data: blocked", RED);
                }
            }
            // Why it matters and what to do, right where the problem shows.
            let max_age = self.settings.effective_for(a).max_usage_age_minutes;
            let why_blocked = match &snap {
                None => Some(
                    "No usage reading yet. Automation will not start tasks on this account until usage is known."
                        .to_string(),
                ),
                Some(s) => {
                    let (age, stale) = plan_burner_core::present::reading_age(s, now, max_age);
                    stale.then(|| {
                        format!(
                            "This reading is {age} min old (limit {max_age} min). Automation will not start tasks until it is refreshed."
                        )
                    })
                }
            };
            if let Some(msg) = why_blocked {
                ui.label(RichText::new(msg).small().color(AMBER));
                ui.horizontal(|ui| {
                    if a.polls() && ui.small_button("Refresh now").clicked() {
                        let r = self.svc.refresh_usage(a.id);
                        self.report(r, "Fetching usage…");
                    }
                    let manual = if a.polls() {
                        "Enter manually"
                    } else {
                        "Update usage"
                    };
                    if ui.small_button(manual).clicked() {
                        self.usage_form = Some(UsageForm::for_account(a.id, sess, week));
                    }
                });
            }
            let pinned = self
                .tasks
                .iter()
                .filter(|t| !t.status.is_terminal() && t.account_id == Some(a.id))
                .count();
            ui.label(
                RichText::new(format!(
                    "{pinned} pinned task(s) · policy: {}",
                    a.execution_policy
                ))
                .color(MUTED)
                .size(13.0),
            );
            ui.horizontal_wrapped(|ui| {
                if a.polls() && ui::primary_button(ui, "Refresh now").clicked() {
                    let r = self.svc.refresh_usage(a.id);
                    self.report(r, "Fetching usage…");
                }
                if ui::primary_button(
                    ui,
                    if a.usage_source != UsageSource::Manual {
                        "Enter manually"
                    } else {
                        "Update usage"
                    },
                )
                .clicked()
                {
                    self.usage_form = Some(UsageForm::for_account(a.id, sess, week));
                }
                if ui::success_button(ui, "Run tasks here…").clicked() {
                    self.propose_batch_for(a.id);
                }
            });
            ui.horizontal_wrapped(|ui| self.burn_controls(ui, Some(a)));
            self.usage_panel(ui, a.id);
            ui.collapsing("Account options", |ui| {
                let mut conf = a.subscription_confirmed;
                if ui
                    .checkbox(
                        &mut conf,
                        "I confirm this account uses a subscription (not an API key)",
                    )
                    .changed()
                {
                    let r = self.svc.set_subscription_confirmed(a.id, conf);
                    self.report(r, if conf { "Subscription confirmed for this account." } else { "Subscription confirmation removed." });
                }
                let mut avail = a.available;
                if ui
                    .checkbox(&mut avail, "Available (untick to stop using this account)")
                    .changed()
                {
                    let r = self.svc.set_account_available(a.id, avail);
                    self.report(r, if avail { "Account available." } else { "Account marked unavailable." });
                }
                let mut ign = a.limits.ignore_limits;
                if ui
                    .checkbox(
                        &mut ign,
                        "Eligible ignoring limits (start threshold, reserve, idle, hours)",
                    )
                    .on_hover_text("Stale/unknown usage, billing checks, the capacity a task needs, availability and STOP still apply.")
                    .changed()
                {
                    let r = self.svc.set_account_ignore_limits(a.id, ign);
                    self.report(r, if ign { "Limits are now ignored for this account." } else { "Limits apply to this account again." });
                }
                if ign {
                    ui.colored_label(
                        AMBER,
                        "Limits are ignored for this account (general and account settings).",
                    );
                }
                if !a.availability_schedule.is_empty() {
                    ui.label(
                        RichText::new(format!("Schedule: {}", a.availability_schedule))
                            .small()
                            .color(GREY),
                    );
                }
                ui.horizontal(|ui| {
                    ui.label("Run mode");
                    let mut rm = a.run_mode;
                    if ui::run_mode_combo(ui, ("rm", a.id), &mut rm, "Use global default") {
                        let r = self.svc.set_account_run_mode(a.id, rm);
                        self.report(r, "Run mode updated.");
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Policy");
                    let mut pol = a.execution_policy;
                    egui::ComboBox::from_id_salt(("pol", a.id))
                        .selected_text(pol.as_str())
                        .show_ui(ui, |ui| {
                            for p in ExecutionPolicy::ALL {
                                ui.selectable_value(&mut pol, *p, p.as_str());
                            }
                        });
                    if pol != a.execution_policy {
                        let r = self.svc.set_account_policy(a.id, pol);
                        self.report(r, "Execution policy updated.");
                    }
                });
                ui.horizontal(|ui| {
                    if ui::secondary_button(ui, "Filter queue").clicked() {
                        self.filter = Filter::Account(a.id);
                        self.view = View::Queue;
                    }
                    if self.confirm_delete == Some(a.id) {
                        let question = if pinned > 0 {
                            format!(
                                "Removal is refused while {pinned} open task(s) are pinned to this account. Try anyway?"
                            )
                        } else {
                            "Remove this account? Its usage readings are deleted; run history is kept.".to_string()
                        };
                        match ui::inline_confirm(ui, &question, "Yes, remove") {
                            Some(true) => {
                                self.confirm_delete = None;
                                let r = self.svc.delete_account(a.id);
                                self.report(r, "Account removed.");
                            }
                            Some(false) => self.confirm_delete = None,
                            None => {}
                        }
                    } else if ui::secondary_button(ui, "Remove").clicked() {
                        self.confirm_delete = Some(a.id);
                    }
                });
            });
        });
    }
}

/// Dashed placeholder card inviting the user to add another account/provider.
fn add_account_card(ui: &mut egui::Ui, form: &mut Option<AccountForm>) {
    let r = egui::Frame::new()
        .fill(crate::theme::SIDEBAR)
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(120.0);
            ui.label(RichText::new("Add account").strong().size(16.0).color(MUTED));
            ui.label(RichText::new("Providers plug in through adapters (Claude, or any agent through a Custom script). Each provider can have several accounts with their own usage, resets and safeguards.").size(13.0).color(MUTED));
            if ui::primary_button(ui, "Add account…").clicked() {
                *form = Some(AccountForm::new());
            }
        });
    let rect = r.response.rect;
    ui.painter().add(egui::Shape::dashed_line(
        &[
            rect.left_top(),
            rect.right_top(),
            rect.right_bottom(),
            rect.left_bottom(),
            rect.left_top(),
        ],
        egui::Stroke::new(1.0_f32, CARD_BORDER),
        6.0,
        4.0,
    ));
}
