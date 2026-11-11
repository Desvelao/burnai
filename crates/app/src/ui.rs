use crate::theme::*;
use eframe::egui::{self, Color32, CornerRadius, RichText, Sense, Stroke, Vec2};
use plan_burner_core::control::RunState;
use plan_burner_core::models::{BillingMode, RunMode, TaskStatus};

pub const GREEN: Color32 = Color32::from_rgb(111, 211, 160);
pub const AMBER: Color32 = Color32::from_rgb(240, 176, 74);
pub const RED: Color32 = Color32::from_rgb(255, 139, 132);
pub const BLUE: Color32 = Color32::from_rgb(142, 192, 255);
pub const GREY: Color32 = MUTED;

/// Outlined badge: coloured text and border, transparent fill.
pub fn badge(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    egui::Frame::new()
        .stroke(Stroke::new(1.0_f32, color))
        .corner_radius(CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(8, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.0).color(color))
        })
        .response
}

/// Flat card with optional 3px accent stripe on top.
pub fn card<R>(
    ui: &mut egui::Ui,
    accent: Option<Color32>,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let out = egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, CARD_BORDER))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            if accent.is_some() {
                ui.add_space(2.0);
            }
            add(ui)
        });
    if let Some(c) = accent {
        let r = out.response.rect;
        let stripe = egui::Rect::from_min_size(r.min, Vec2::new(r.width(), 3.0));
        ui.painter().rect_filled(
            stripe,
            CornerRadius {
                nw: 6,
                ne: 6,
                sw: 0,
                se: 0,
            },
            c,
        );
    }
    out
}

/// Small uppercase label above a group of content.
pub fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text.to_uppercase())
            .size(11.5)
            .color(MUTED)
            .extra_letter_spacing(0.6),
    );
}

pub fn page_header(ui: &mut egui::Ui, title: &str, right: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(22.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
    });
    ui.add_space(4.0);
}

fn button(ui: &mut egui::Ui, text: &str, fill: Color32, strong: bool) -> egui::Response {
    let mut t = RichText::new(text).color(Color32::WHITE);
    if strong {
        t = t.strong();
    }
    ui.add(
        egui::Button::new(t)
            .fill(fill)
            .min_size(Vec2::new(0.0, 30.0)),
    )
}
pub fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    button(ui, text, PRIMARY, false)
}
pub fn success_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    button(ui, text, SUCCESS_BTN, true)
}
pub fn danger_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    button(ui, text, DANGER_BTN, true)
}
pub fn secondary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    button(ui, text, NEUTRAL_BTN, false)
}

/// Rounded filter chip.
pub fn chip(ui: &mut egui::Ui, selected: bool, text: &str, dot: Option<Color32>) -> egui::Response {
    let fill = if selected {
        PRIMARY
    } else {
        Color32::TRANSPARENT
    };
    let stroke = if selected { PRIMARY } else { CARD_BORDER };
    let r = egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, stroke))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(egui::Margin::symmetric(12, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if let Some(c) = dot {
                    swatch(ui, c);
                }
                ui.label(RichText::new(text).size(13.0).color(if selected {
                    Color32::WHITE
                } else {
                    TEXT
                }));
            });
        });
    r.response.interact(Sense::click())
}

pub fn swatch(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(2), color);
}

/// Provider colour used for stripes and swatches.
pub fn provider_color(provider_type: &str) -> Color32 {
    if provider_type == "claude" {
        CLAY
    } else {
        OTHER_PROVIDER
    }
}

/// Summary tile: small label over a prominent value.
pub fn stat_tile(ui: &mut egui::Ui, label: &str, value: impl Into<RichText>) {
    egui::Frame::new()
        .fill(INSET)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_width(140.0);
            section_label(ui, label);
            ui.label(value.into().size(18.0).strong());
        });
}

pub fn banner(ui: &mut egui::Ui, color: Color32, text: &str) {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, color))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                badge(ui, "Notice", color);
                ui.label(RichText::new(text).size(13.0));
            });
        });
}

/// Below this many percent left a meter turns red and shows a LOW marker.
const LOW_PERCENT: f64 = 20.0;

/// Diagonal stripes over `rect`: the "no reading" look, distinct from an empty (0% left) bar.
fn hatch(ui: &egui::Ui, rect: egui::Rect) {
    let painter = ui.painter().with_clip_rect(rect);
    let stroke = egui::Stroke::new(1.0_f32, AMBER);
    let mut x = rect.left() - rect.height();
    while x < rect.right() {
        painter.line_segment(
            [
                egui::pos2(x, rect.bottom()),
                egui::pos2(x + rect.height(), rect.top()),
            ],
            stroke,
        );
        x += 6.0;
    }
}

/// Usage meter: label, big value, track and reset text. `remaining` is percent left; `None` = unknown.
pub fn meter(ui: &mut egui::Ui, label: &str, remaining: Option<f64>, reset: Option<String>) {
    section_label(ui, label);
    match remaining {
        Some(p) => {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{p:.0}%")).size(24.0).strong());
                if p < LOW_PERCENT {
                    // not colour alone: a text marker for the red state
                    badge(ui, "LOW", BAR_LOW);
                }
                ui.label(
                    RichText::new(match &reset {
                        Some(r) => format!("left · {r}"),
                        None => "left".to_string(),
                    })
                    .color(MUTED)
                    .size(13.0),
                );
            });
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 8.0), Sense::hover());
            ui.painter().rect_filled(rect, CornerRadius::same(3), INSET);
            // The bar shows what is left, like the number beside it.
            let left = (p / 100.0).clamp(0.0, 1.0) as f32;
            let mut fill = rect;
            fill.set_width(rect.width() * left);
            ui.painter().rect_filled(
                fill,
                CornerRadius::same(3),
                if p < LOW_PERCENT { BAR_LOW } else { BAR_BLUE },
            );
        }
        None => {
            ui.label(RichText::new("unknown (treated as unavailable)").color(AMBER));
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 8.0), Sense::hover());
            ui.painter().rect_filled(rect, CornerRadius::same(3), INSET);
            hatch(ui, rect);
        }
    }
    ui.add_space(4.0);
}

pub fn run_state_label(s: RunState) -> &'static str {
    match s {
        RunState::Running => "Running",
        RunState::Paused => "Paused",
        RunState::Waiting => "Waiting",
        RunState::Stopped => "Stopped",
    }
}

pub fn status_color(s: TaskStatus) -> Color32 {
    match s {
        TaskStatus::Completed => GREEN,
        TaskStatus::Failed => RED,
        TaskStatus::AwaitingApproval => AMBER,
        TaskStatus::WaitingForCapacity | TaskStatus::Running => BLUE,
        _ => GREY,
    }
}

pub fn level_color(l: plan_burner_core::present::Level) -> Color32 {
    use plan_burner_core::present::Level;
    match l {
        Level::Good => GREEN,
        Level::Warn => AMBER,
        Level::Bad => RED,
        Level::Muted => GREY,
    }
}

pub fn billing_color(b: BillingMode) -> Color32 {
    level_color(plan_burner_core::present::billing_level(b))
}

pub fn billing_label(b: BillingMode) -> &'static str {
    plan_burner_core::present::billing_label(b)
}

/// Combo box over the provider-neutral run modes; `inherit_label` names what `Inherit` means here.
/// Returns true if the selection changed.
pub fn run_mode_combo(
    ui: &mut egui::Ui,
    salt: impl std::hash::Hash,
    mode: &mut RunMode,
    inherit_label: &str,
) -> bool {
    let before = *mode;
    let name = |m: RunMode| {
        if m == RunMode::Inherit {
            inherit_label.to_string()
        } else {
            m.label().to_string()
        }
    };
    egui::ComboBox::from_id_salt(salt)
        .selected_text(name(*mode))
        .show_ui(ui, |ui| {
            for m in RunMode::ALL {
                ui.selectable_value(mode, *m, name(*m));
            }
        });
    *mode != before
}

pub fn priority_label(p: i64) -> &'static str {
    plan_burner_core::present::priority_label(p)
}

pub fn fmt_minutes(m: i64) -> String {
    plan_burner_core::present::fmt_minutes(m)
}

/// Full-page editor: header with a Back button, the body in a card. Returns true if Back was clicked.
pub fn editor_page(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) -> bool {
    let mut back = false;
    page_header(ui, title, |ui| {
        back = secondary_button(ui, "← Back").clicked();
    });
    card(ui, None, |ui| {
        ui.set_width(ui.available_width());
        body(ui);
    });
    back
}

/// Single-line text field with an example (hint), hover help, and the validation error in red
/// right under it while the text is invalid. `check` returns the message for bad input.
pub fn checked_text(
    ui: &mut egui::Ui,
    value: &mut String,
    hint: &str,
    help: &str,
    check: impl Fn(&str) -> Result<(), String>,
) -> egui::Response {
    ui.vertical(|ui| {
        let r = ui
            .add(egui::TextEdit::singleline(value).hint_text(hint))
            .on_hover_text(help);
        if let Err(e) = check(value.trim()) {
            ui.colored_label(RED, e);
        }
        r
    })
    .inner
}

/// Validators shared by the form fields (the same parsers core uses on save).
pub mod check {
    use plan_burner_core::{availability, db, scheduler, settings};

    /// "HH:MM-HH:MM", blank = always.
    pub fn hours(t: &str) -> Result<(), String> {
        if t.is_empty() || settings::parse_hours(t).is_some() {
            Ok(())
        } else {
            Err("Use HH:MM-HH:MM, e.g. 22:00-07:00 (may cross midnight)".into())
        }
    }
    /// Weekly availability rules, blank = always.
    pub fn availability(t: &str) -> Result<(), String> {
        availability::validate(t)
    }
    /// Task schedule: blank, `after:<RFC 3339>` or `daily:HH:MM`.
    pub fn task_schedule(t: &str) -> Result<(), String> {
        scheduler::validate_schedule(Some(t)).map_err(|e| e.to_string())
    }
    /// Recurrence rule, blank = runs once.
    pub fn task_repeat(t: &str) -> Result<(), String> {
        plan_burner_core::recurrence::validate(Some(t)).map_err(|e| e.to_string())
    }
    /// RFC 3339 timestamp, blank = never.
    pub fn expiry(t: &str) -> Result<(), String> {
        if t.is_empty() || db::parse_ts(t).is_some() {
            Ok(())
        } else {
            Err("Use an RFC 3339 time, e.g. 2026-01-31T09:30:00Z".into())
        }
    }
}

/// Red asterisk after a label of a required field.
pub fn required_marker(ui: &mut egui::Ui) {
    ui.colored_label(RED, "*").on_hover_text("Required");
}

/// Two-step inline confirmation. Returns Some(true) when confirmed, Some(false) when cancelled.
pub fn inline_confirm(ui: &mut egui::Ui, question: &str, yes: &str) -> Option<bool> {
    let mut out = None;
    ui.horizontal_wrapped(|ui| {
        ui.colored_label(RED, question);
        if danger_button(ui, yes).clicked() {
            out = Some(true);
        }
        if secondary_button(ui, "Cancel").clicked() {
            out = Some(false);
        }
    });
    out
}

type PickSlot = std::sync::Arc<std::sync::Mutex<Option<String>>>;

/// Text field plus a "Browse…" button that opens the system chooser. Typing a
/// path still works; the dialog only fills the text. The dialog runs on a
/// worker thread so the UI stays responsive.
pub fn path_field(
    ui: &mut egui::Ui,
    value: &mut String,
    kind: plan_burner_core::pick::PickKind,
    width: Option<f32>,
) -> egui::Response {
    let mut edit = egui::TextEdit::singleline(value);
    if let Some(w) = width {
        edit = edit.desired_width(w);
    }
    let resp = ui.add(edit);
    let slot_id = resp.id.with("pick_slot");
    let slot: PickSlot = ui.ctx().data_mut(|d| {
        d.get_temp_mut_or_insert_with(slot_id, PickSlot::default)
            .clone()
    });
    if let Some(p) = slot.lock().ok().and_then(|mut g| g.take()) {
        *value = p;
    }
    if ui
        .button("Browse…")
        .on_hover_text("Choose with the system dialog (or type the path)")
        .clicked()
    {
        let (current, ctx, slot) = (value.clone(), ui.ctx().clone(), slot.clone());
        std::thread::spawn(move || {
            if let Some(p) = plan_burner_core::pick::pick_path(kind, &current) {
                if let Ok(mut g) = slot.lock() {
                    *g = Some(p);
                }
            }
            ctx.request_repaint();
        });
    }
    resp
}

#[cfg(test)]
mod check_tests {
    use super::check;

    #[test]
    fn field_validators_accept_blank_and_good_input_and_explain_bad_input() {
        assert!(check::hours("").is_ok() && check::hours("22:00-07:00").is_ok());
        assert!(check::hours("late").unwrap_err().contains("HH:MM-HH:MM"));
        assert!(check::availability("").is_ok());
        assert!(check::availability("mon-fri 09:00-17:00").is_ok());
        assert!(check::availability("whenever").is_err());
        assert!(check::task_schedule("").is_ok() && check::task_schedule("daily:09:00").is_ok());
        assert!(check::task_schedule("tomorrow").is_err());
        assert!(check::expiry("").is_ok() && check::expiry("2026-01-31T09:30:00Z").is_ok());
        assert!(check::expiry("31/01/2026")
            .unwrap_err()
            .contains("RFC 3339"));
    }
}
