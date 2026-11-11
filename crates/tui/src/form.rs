//! A small full-page form engine for the TUI: labelled fields, focus, editing, scrolling and rendering.
//! It knows nothing about accounts or tasks; `editors` maps core form models to and from it.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{prelude::*, widgets::*};
use tui_textarea::{CursorMove, TextArea};

pub enum Kind {
    /// Single-line text (numbers are text too and parsed by the editor).
    Text(TextArea<'static>),
    /// Multi-line text shown with the given number of rows.
    Multi(TextArea<'static>, u16),
    Check(bool),
    Choice {
        opts: Vec<String>,
        idx: usize,
    },
    /// A per-account override: off = inherit `inherited`; on = edit a value of its own.
    Opt {
        on: bool,
        text: TextArea<'static>,
        inherited: String,
    },
    /// A non-focusable section heading.
    Heading,
}

pub struct Field {
    pub key: &'static str,
    pub label: String,
    pub kind: Kind,
    pub visible: bool,
    /// Read-only fields can be seen but not focused or changed.
    pub enabled: bool,
    /// Differs from the value the form was opened with (shown as a dot after the label).
    pub marked: bool,
}

pub struct Form {
    pub title: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub error: Option<String>,
    /// The field a rejected save is about (shown in red, focused), found from the error text.
    pub error_field: Option<usize>,
    pub dirty: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FormEvent {
    None,
    Save,
    Cancel,
    /// A form-level shortcut (Ctrl-<letter>) the owner handles, e.g. 'e' = suggest estimate.
    Shortcut(char),
}

fn area(text: &str, multi: bool) -> TextArea<'static> {
    let lines: Vec<String> = if text.is_empty() {
        vec![String::new()]
    } else if multi {
        text.split('\n').map(str::to_string).collect()
    } else {
        vec![text.replace('\n', " ")]
    };
    let mut ta = TextArea::new(lines);
    ta.set_cursor_line_style(Style::default());
    ta.move_cursor(CursorMove::Bottom);
    ta.move_cursor(CursorMove::End);
    ta
}

impl Field {
    fn new(key: &'static str, label: &str, kind: Kind) -> Field {
        Field {
            key,
            label: label.to_string(),
            kind,
            visible: true,
            enabled: true,
            marked: false,
        }
    }
    pub fn heading(key: &'static str, label: &str) -> Field {
        let mut f = Field::new(key, label, Kind::Heading);
        f.enabled = false;
        f
    }
    pub fn text(key: &'static str, label: &str, v: &str) -> Field {
        Field::new(key, label, Kind::Text(area(v, false)))
    }
    pub fn multi(key: &'static str, label: &str, v: &str, rows: u16) -> Field {
        Field::new(key, label, Kind::Multi(area(v, true), rows))
    }
    pub fn check(key: &'static str, label: &str, v: bool) -> Field {
        Field::new(key, label, Kind::Check(v))
    }
    pub fn choice(key: &'static str, label: &str, opts: Vec<String>, idx: usize) -> Field {
        let idx = idx.min(opts.len().saturating_sub(1));
        Field::new(key, label, Kind::Choice { opts, idx })
    }
    pub fn opt(key: &'static str, label: &str, v: Option<&str>, inherited: &str) -> Field {
        Field::new(
            key,
            label,
            Kind::Opt {
                on: v.is_some(),
                text: area(v.unwrap_or(""), false),
                inherited: inherited.to_string(),
            },
        )
    }
    pub fn hidden_unless(mut self, show: bool) -> Field {
        self.visible = show;
        self
    }
    pub fn disabled(mut self) -> Field {
        self.enabled = false;
        self
    }
    fn focusable(&self) -> bool {
        self.visible && self.enabled
    }
    fn height(&self) -> u16 {
        match &self.kind {
            Kind::Multi(_, h) => (*h).max(2),
            _ => 1,
        }
    }
}

impl Form {
    pub fn new(title: &str, fields: Vec<Field>) -> Form {
        let mut f = Form {
            title: title.to_string(),
            fields,
            focus: 0,
            error: None,
            error_field: None,
            dirty: false,
        };
        f.focus = f.fields.iter().position(Field::focusable).unwrap_or(0);
        f
    }

    /// The visible label of field `key` (used so validation messages name what the user sees).
    pub fn label(&self, key: &str) -> Option<&str> {
        self.get(key).map(|f| f.label.as_str())
    }

    /// Shows a rejected save: the message, with focus moved to the field it names. A field matches
    /// when the message starts with its label up to the first " (" (e.g. "Reserved capacity").
    pub fn set_error(&mut self, msg: String) {
        let lower = msg.to_lowercase();
        let hit = self.fields.iter().position(|f| {
            let head = f
                .label
                .split(" (")
                .next()
                .unwrap_or(&f.label)
                .to_lowercase();
            f.visible && f.focusable() && !head.is_empty() && lower.starts_with(&head)
        });
        if let Some(i) = hit {
            self.focus = i;
        }
        self.error_field = hit;
        self.error = Some(msg);
    }

    fn get(&self, key: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.key == key)
    }
    pub fn field_mut(&mut self, key: &str) -> Option<&mut Field> {
        self.fields.iter_mut().find(|f| f.key == key)
    }

    /// Text of a text/multi/override field (an override that is off yields its text too).
    pub fn text(&self, key: &str) -> String {
        match self.get(key).map(|f| &f.kind) {
            Some(Kind::Text(t)) | Some(Kind::Multi(t, _)) | Some(Kind::Opt { text: t, .. }) => {
                t.lines().join("\n")
            }
            _ => String::new(),
        }
    }
    pub fn set_text(&mut self, key: &str, v: &str) {
        if let Some(f) = self.field_mut(key) {
            match &mut f.kind {
                Kind::Text(t) => *t = area(v, false),
                Kind::Multi(t, _) => *t = area(v, true),
                Kind::Opt { text, on, .. } => {
                    *text = area(v, false);
                    *on = true;
                }
                _ => {}
            }
        }
    }
    pub fn checked(&self, key: &str) -> bool {
        matches!(self.get(key).map(|f| &f.kind), Some(Kind::Check(true)))
    }
    pub fn choice_idx(&self, key: &str) -> usize {
        match self.get(key).map(|f| &f.kind) {
            Some(Kind::Choice { idx, .. }) => *idx,
            _ => 0,
        }
    }
    pub fn set_choice(&mut self, key: &str, i: usize) {
        if let Some(Field {
            kind: Kind::Choice { opts, idx },
            ..
        }) = self.field_mut(key)
        {
            *idx = i.min(opts.len().saturating_sub(1));
        }
    }
    pub fn set_checked(&mut self, key: &str, v: bool) {
        if let Some(Field {
            kind: Kind::Check(c),
            ..
        }) = self.field_mut(key)
        {
            *c = v;
        }
    }
    /// `Some(text)` when the override is on, `None` when it inherits.
    pub fn opt(&self, key: &str) -> Option<String> {
        match self.get(key).map(|f| &f.kind) {
            Some(Kind::Opt { on: true, text, .. }) => Some(text.lines().join("\n")),
            _ => None,
        }
    }
    pub fn set_visible(&mut self, key: &str, v: bool) {
        if let Some(f) = self.field_mut(key) {
            f.visible = v;
        }
        if !self.fields.get(self.focus).is_some_and(Field::focusable) {
            self.move_focus(1);
        }
    }

    fn move_focus(&mut self, d: i32) {
        let n = self.fields.len() as i32;
        let mut i = self.focus as i32;
        for _ in 0..n {
            i = (i + d).rem_euclid(n);
            if self.fields[i as usize].focusable() {
                self.focus = i as usize;
                return;
            }
        }
    }

    pub fn paste(&mut self, s: &str) {
        if let Some(f) = self.fields.get_mut(self.focus) {
            let multi = matches!(f.kind, Kind::Multi(..));
            let ta = match &mut f.kind {
                Kind::Text(t) | Kind::Multi(t, _) => Some(t),
                Kind::Opt { on: true, text, .. } => Some(text),
                _ => None,
            };
            if let Some(t) = ta {
                t.insert_str(if multi {
                    s.to_string()
                } else {
                    s.replace('\n', " ")
                });
                self.dirty = true;
            }
        }
    }

    /// Fields that take a filesystem path, and what the system chooser should pick for them.
    fn focused_pick(&self) -> Option<plan_burner_core::pick::PickKind> {
        use plan_burner_core::pick::PickKind::{Dir, File};
        let f = self.fields.get(self.focus)?;
        if !f.enabled || !matches!(f.kind, Kind::Text(_)) {
            return None;
        }
        match f.key {
            "working_root" | "results_dir" | "config_dir" | "working_dir" => Some(Dir),
            "script" | "path" => Some(File),
            _ => None,
        }
    }

    fn browse(&mut self) {
        let Some(kind) = self.focused_pick() else {
            return;
        };
        let key = self.fields[self.focus].key;
        let cur = self.text(key);
        match plan_burner_core::pick::pick_path(kind, &cur) {
            Some(p) => {
                self.set_text(key, &p);
                self.dirty = true;
            }
            None => {
                self.error = Some(
                    "No path chosen (dialog cancelled or unavailable); type the path instead."
                        .into(),
                )
            }
        }
    }

    pub fn handle(&mut self, k: KeyEvent) -> FormEvent {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('s') if ctrl => return FormEvent::Save,
            KeyCode::Esc => return FormEvent::Cancel,
            // Ctrl-B on a path field: choose with the system dialog (typing still works).
            KeyCode::Char('b') if ctrl && self.focused_pick().is_some() => {
                self.browse();
                return FormEvent::None;
            }
            KeyCode::Tab => {
                self.move_focus(1);
                return FormEvent::None;
            }
            KeyCode::BackTab => {
                self.move_focus(-1);
                return FormEvent::None;
            }
            KeyCode::Char(c)
                if ctrl && !matches!(c, 'a' | 'e' | 'k' | 'u' | 'w' | 'h' | 'j' | 'x' | 'c') =>
            {
                return FormEvent::Shortcut(c)
            }
            _ => {}
        }
        self.error = None;
        self.error_field = None;
        let Some(f) = self.fields.get_mut(self.focus) else {
            return FormEvent::None;
        };
        let mut changed = false;
        match &mut f.kind {
            Kind::Check(v) => match k.code {
                KeyCode::Char(' ') | KeyCode::Enter => {
                    *v = !*v;
                    changed = true;
                }
                KeyCode::Up => self.move_focus(-1),
                KeyCode::Down => self.move_focus(1),
                _ => {}
            },
            Kind::Choice { opts, idx } => match k.code {
                KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter => {
                    *idx = (*idx + 1) % opts.len().max(1);
                    changed = true;
                }
                KeyCode::Left => {
                    *idx = (*idx + opts.len().max(1) - 1) % opts.len().max(1);
                    changed = true;
                }
                KeyCode::Up => self.move_focus(-1),
                KeyCode::Down => self.move_focus(1),
                _ => {}
            },
            Kind::Text(t) => match k.code {
                KeyCode::Enter | KeyCode::Down => self.move_focus(1),
                KeyCode::Up => self.move_focus(-1),
                _ => changed = t.input(k),
            },
            Kind::Multi(t, _) => changed = t.input(k),
            Kind::Heading => {}
            Kind::Opt {
                on,
                text,
                inherited,
            } => match k.code {
                // Ctrl-X goes back to inheriting.
                KeyCode::Char('x') if ctrl => {
                    *on = false;
                    changed = true;
                }
                _ if !*on => match k.code {
                    KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Right => {
                        *on = true;
                        *text = area(inherited, false);
                        changed = true;
                    }
                    KeyCode::Up => self.move_focus(-1),
                    KeyCode::Down => self.move_focus(1),
                    _ => {}
                },
                KeyCode::Enter | KeyCode::Down => self.move_focus(1),
                KeyCode::Up => self.move_focus(-1),
                _ => changed = text.input(k),
            },
        }
        if changed {
            self.error_field = None;
        }
        self.dirty |= changed;
        FormEvent::None
    }

    /// Draws the form into `area`, scrolled so the focused field is visible.
    pub fn render(&self, f: &mut Frame, area: Rect) {
        let inner = Block::bordered().inner(area);
        let err_rows = match &self.error {
            Some(e) if inner.height > 3 => {
                if e.chars().count() as u16 > inner.width {
                    2
                } else {
                    1
                }
            }
            _ => 0,
        };
        let (body, foot) = if err_rows > 0 {
            let [b, e] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(err_rows)]).areas(inner);
            (b, Some(e))
        } else {
            (inner, None)
        };
        let vis: Vec<usize> = (0..self.fields.len())
            .filter(|i| self.fields[*i].visible)
            .collect();
        let focus_pos = vis.iter().position(|i| *i == self.focus).unwrap_or(0);
        // First visible field such that everything up to the focused one fits.
        let mut start = focus_pos;
        let mut used = vis.get(focus_pos).map_or(0, |i| self.fields[*i].height());
        while start > 0 {
            let h = self.fields[vis[start - 1]].height();
            if used + h > body.height {
                break;
            }
            used += h;
            start -= 1;
        }
        // How far the visible window reaches, for the "more above/below" markers.
        let mut end = start.min(vis.len());
        let mut filled = 0;
        for &i in &vis[end..] {
            let h = self.fields[i].height();
            if filled + h > body.height {
                break;
            }
            filled += h;
            end += 1;
        }
        let save_hint = if self.title.contains("Ctrl-S") {
            ""
        } else {
            " · Ctrl-S save · Esc cancel"
        };
        let save_hint = if self.focused_pick().is_some() {
            format!("{save_hint} · Ctrl-B browse")
        } else {
            save_hint.to_string()
        };
        let heads = |upto: usize| {
            vis.iter()
                .take(upto)
                .filter(|i| matches!(self.fields[**i].kind, Kind::Heading))
                .count()
        };
        let position = format!(
            " field {}/{}{}{} ",
            focus_pos + 1 - heads(focus_pos + 1),
            vis.len() - heads(vis.len()),
            if start > 0 { " ▲ more above" } else { "" },
            if end < vis.len() {
                " ▼ more below"
            } else {
                ""
            }
        );
        f.render_widget(
            Block::bordered()
                .title(format!(" {}{save_hint} ", self.title))
                .title_bottom(Line::from(position).right_aligned()),
            area,
        );
        if let (Some(r), Some(e)) = (foot, &self.error) {
            f.render_widget(
                Paragraph::new(e.as_str())
                    .wrap(Wrap { trim: true })
                    .style(Style::new().fg(Color::Red).bold()),
                r,
            );
        }
        let label_w = 34.min(body.width / 2);
        let mut y = body.y;
        for &i in &vis[start.min(vis.len())..] {
            let fld = &self.fields[i];
            let h = fld.height();
            if y + h > body.y + body.height {
                break;
            }
            let focused = i == self.focus;
            let label_style = if self.error_field == Some(i) {
                Style::new().fg(Color::Red).bold()
            } else if !fld.enabled {
                Style::new().fg(Color::DarkGray)
            } else if focused {
                Style::new().fg(Color::Cyan).bold()
            } else {
                Style::new()
            };
            if matches!(fld.kind, Kind::Heading) {
                f.render_widget(
                    Paragraph::new(format!("── {} ──", fld.label))
                        .style(Style::new().fg(Color::Yellow).bold()),
                    Rect::new(body.x, y, body.width, 1),
                );
                y += h;
                continue;
            }
            let lrect = Rect::new(body.x, y, label_w, 1);
            let vrect = Rect::new(body.x + label_w, y, body.width - label_w, h);
            f.render_widget(
                Paragraph::new(format!(
                    "{}{}{}",
                    if focused { "▸ " } else { "  " },
                    fld.label,
                    if fld.marked { " ●" } else { "" }
                ))
                .style(label_style),
                lrect,
            );
            match &fld.kind {
                Kind::Text(t) | Kind::Multi(t, _) => {
                    if focused && fld.enabled {
                        f.render_widget(t, vrect);
                    } else {
                        f.render_widget(Paragraph::new(t.lines().join("\n")), vrect);
                    }
                }
                Kind::Check(v) => {
                    f.render_widget(Paragraph::new(if *v { "[x]" } else { "[ ]" }), vrect)
                }
                Kind::Choice { opts, idx } => f.render_widget(
                    Paragraph::new(format!("◂ {} ▸", opts.get(*idx).map_or("", String::as_str))),
                    vrect,
                ),
                Kind::Heading => {}
                Kind::Opt {
                    on,
                    text,
                    inherited,
                } => {
                    if !*on {
                        let shown = if inherited.is_empty() {
                            "(none)"
                        } else {
                            inherited
                        };
                        f.render_widget(
                            Paragraph::new(format!("[ ] inherits {shown}  (Space = override)"))
                                .style(Style::new().fg(Color::DarkGray)),
                            vrect,
                        );
                    } else {
                        let [mark, val] =
                            Layout::horizontal([Constraint::Length(4), Constraint::Min(1)])
                                .areas(vrect);
                        f.render_widget(Paragraph::new("[x] "), mark);
                        if focused {
                            f.render_widget(text, val);
                        } else {
                            f.render_widget(Paragraph::new(text.lines().join("\n")), val);
                        }
                    }
                }
            }
            y += h;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn type_str(f: &mut Form, s: &str) {
        for c in s.chars() {
            f.handle(key(KeyCode::Char(c)));
        }
    }
    fn demo() -> Form {
        Form::new(
            "demo",
            vec![
                Field::text("name", "Name", "ab"),
                Field::multi("prompt", "Prompt", "", 3),
                Field::check("flag", "Flag", false),
                Field::choice("mode", "Mode", vec!["a".into(), "b".into(), "c".into()], 0),
                Field::opt("limit", "Limit", None, "65"),
            ],
        )
    }

    #[test]
    fn typing_edits_text_and_marks_dirty() {
        let mut f = demo();
        assert!(!f.dirty);
        type_str(&mut f, "cd");
        assert_eq!(f.text("name"), "abcd");
        assert!(f.dirty);
        f.handle(key(KeyCode::Backspace));
        assert_eq!(f.text("name"), "abc");
    }

    #[test]
    fn enter_in_single_line_moves_focus_but_multiline_keeps_newlines() {
        let mut f = demo();
        f.handle(key(KeyCode::Enter));
        assert_eq!(f.focus, 1);
        type_str(&mut f, "x");
        f.handle(key(KeyCode::Enter));
        type_str(&mut f, "y");
        assert_eq!(f.text("prompt"), "x\ny");
        assert_eq!(f.focus, 1);
    }

    #[test]
    fn tab_cycles_focus_and_skips_hidden_fields() {
        let mut f = demo();
        f.set_visible("flag", false);
        f.handle(key(KeyCode::Tab));
        f.handle(key(KeyCode::Tab));
        assert_eq!(f.focus, 3, "flag is hidden");
        f.handle(key(KeyCode::BackTab));
        assert_eq!(f.focus, 1);
    }

    #[test]
    fn check_choice_and_override_fields() {
        let mut f = demo();
        f.focus = 2;
        f.handle(key(KeyCode::Char(' ')));
        assert!(f.checked("flag"));
        f.focus = 3;
        f.handle(key(KeyCode::Right));
        f.handle(key(KeyCode::Right));
        assert_eq!(f.choice_idx("mode"), 2);
        f.handle(key(KeyCode::Right));
        assert_eq!(f.choice_idx("mode"), 0, "wraps");
        f.handle(key(KeyCode::Left));
        assert_eq!(f.choice_idx("mode"), 2);
        f.focus = 4;
        assert_eq!(f.opt("limit"), None, "inherits by default");
        f.handle(key(KeyCode::Char(' ')));
        assert_eq!(
            f.opt("limit").as_deref(),
            Some("65"),
            "seeded with the inherited value"
        );
        type_str(&mut f, "5");
        assert_eq!(f.opt("limit").as_deref(), Some("655"));
        f.handle(ctrl('x'));
        assert_eq!(f.opt("limit"), None);
    }

    #[test]
    fn save_cancel_and_shortcuts_are_reported() {
        let mut f = demo();
        assert_eq!(f.handle(ctrl('s')), FormEvent::Save);
        assert_eq!(f.handle(key(KeyCode::Esc)), FormEvent::Cancel);
        assert_eq!(f.handle(ctrl('t')), FormEvent::Shortcut('t'));
    }

    #[test]
    fn paste_goes_into_the_focused_text_field() {
        let mut f = demo();
        f.paste("line1\nline2");
        assert_eq!(
            f.text("name"),
            "abline1 line2",
            "single line flattens newlines"
        );
        f.focus = 1;
        f.paste("a\nb");
        assert_eq!(f.text("prompt"), "a\nb");
    }

    #[test]
    fn renders_without_panicking_at_small_sizes() {
        let f = demo();
        for (w, h) in [(80, 24), (20, 3), (120, 40), (1, 1)] {
            let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            t.draw(|fr| f.render(fr, fr.area())).unwrap();
        }
    }
}
