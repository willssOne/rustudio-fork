//! Script editor tabs: Luau syntax highlighting, line numbers and a live
//! compile check that underlines the line of the first syntax error.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, RichText, Stroke};
use rbx_dom_weak::types::{Ref, Variant};

use crate::app::Editor;
use crate::icons;
use crate::mech;
use crate::runtime;
use crate::script_complete::{complete, CompletionCtx, Item, ItemKind};
use crate::studio_settings::{rgb, ScriptColors};
use crate::theme;

pub(crate) const KEYWORDS: &[&str] = &[
    "and", "break", "const", "continue", "do", "else", "elseif", "end", "export", "false", "for",
    "function", "if", "in", "local", "nil", "not", "or", "repeat", "return", "then", "true",
    "type", "until", "while",
];

pub(crate) const GLOBALS: &[&str] = &[
    "game",
    "workspace",
    "Workspace",
    "script",
    "Instance",
    "Vector3",
    "Vector2",
    "CFrame",
    "Color3",
    "BrickColor",
    "UDim",
    "UDim2",
    "Enum",
    "task",
    "print",
    "warn",
    "error",
    "require",
    "typeof",
    "type",
    "math",
    "string",
    "table",
    "pcall",
    "xpcall",
    "pairs",
    "ipairs",
    "next",
    "select",
    "tostring",
    "tonumber",
    "setmetatable",
    "getmetatable",
    "assert",
    "wait",
    "spawn",
    "delay",
    "tick",
    "time",
    "coroutine",
    "buffer",
    "bit32",
    "utf8",
    "os",
    "debug",
    "self",
    "shared",
    "_G",
    "rawget",
    "rawset",
    "rawequal",
    "unpack",
    "rawlen",
    "settings",
    "UserSettings",
    "elapsedTime",
    "gcinfo",
    "collectgarbage",
    "getfenv",
    "setfenv",
    "loadstring",
    "newproxy",
    "version",
    "plugin",
    "Game",
    "Ray",
    "Region3",
    "Region3int16",
    "Vector3int16",
    "Vector2int16",
    "NumberRange",
    "NumberSequence",
    "NumberSequenceKeypoint",
    "ColorSequence",
    "ColorSequenceKeypoint",
    "TweenInfo",
    "PhysicalProperties",
    "Rect",
    "Faces",
    "Axes",
    "Font",
    "DateTime",
    "Random",
    "RaycastParams",
    "OverlapParams",
    "PathWaypoint",
    "SharedTable",
    "CatalogSearchParams",
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Tok {
    Plain,
    Keyword,
    Global,
    Str,
    Num,
    Comment,
    Op,
}

/// Splits Luau source into coloured spans (byte ranges).
pub(crate) fn tokenize(src: &str) -> Vec<(std::ops::Range<usize>, Tok)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    // `[[ ... ]]` / `[==[ ... ]==]` long brackets; returns the end index.
    let long_bracket = |at: usize| -> Option<usize> {
        if b.get(at) != Some(&b'[') {
            return None;
        }
        let mut j = at + 1;
        let mut level = 0;
        while b.get(j) == Some(&b'=') {
            level += 1;
            j += 1;
        }
        if b.get(j) != Some(&b'[') {
            return None;
        }
        let close: String = format!("]{}]", "=".repeat(level));
        Some(
            src[j + 1..]
                .find(&close)
                .map(|p| j + 1 + p + close.len())
                .unwrap_or(src.len()),
        )
    };
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            i = long_bracket(i + 2)
                .unwrap_or_else(|| src[i..].find('\n').map(|p| i + p).unwrap_or(src.len()));
            out.push((start..i, Tok::Comment));
        } else if c == b'"' || c == b'\'' || c == b'`' {
            i += 1;
            while i < b.len() && b[i] != c && b[i] != b'\n' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(b.len());
            out.push((start..i, Tok::Str));
        } else if let Some(end) = (c == b'[').then(|| long_bracket(i)).flatten() {
            i = end;
            out.push((start..i, Tok::Str));
        } else if c.is_ascii_digit()
            || (c == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            out.push((start..i, Tok::Num));
        } else if c.is_ascii_alphabetic() || c == b'_' {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &src[start..i];
            let after_dot = start > 0 && b[start - 1] == b'.' || start > 0 && b[start - 1] == b':';
            let t = if KEYWORDS.contains(&word) {
                Tok::Keyword
            } else if GLOBALS.contains(&word) && !after_dot {
                Tok::Global
            } else {
                Tok::Plain
            };
            out.push((start..i, t));
        } else if c.is_ascii_punctuation() {
            i += 1;
            out.push((start..i, Tok::Op));
        } else {
            // Whitespace and anything else (including multi-byte UTF-8).
            i += 1;
            while i < b.len() && !src.is_char_boundary(i) {
                i += 1;
            }
            out.push((start..i, Tok::Plain));
        }
    }
    out
}

/// Byte range of a 1-based line.
fn line_range(src: &str, line: usize) -> Option<std::ops::Range<usize>> {
    let mut start = 0;
    for (n, l) in src.split('\n').enumerate() {
        if n + 1 == line {
            return Some(start..start + l.len());
        }
        start += l.len() + 1;
    }
    None
}

/// A highlighted layout job in the Studio Settings colours; the error line
/// gets a red wavy-looking underline.
pub fn highlight(src: &str, error_line: Option<usize>, font: FontId, colors: &ScriptColors) -> LayoutJob {
    let mut job = LayoutJob::default();
    let err = error_line.and_then(|l| line_range(src, l));
    for (range, tok) in tokenize(src) {
        let color = rgb(match tok {
            Tok::Plain => colors.text,
            Tok::Keyword => colors.keyword,
            Tok::Global => colors.builtin_function,
            Tok::Str => colors.string,
            Tok::Num => colors.number,
            Tok::Comment => colors.comment,
            Tok::Op => colors.operator,
        });
        // Split the token where it crosses into / out of the error line.
        let mut cuts = vec![range.start, range.end];
        if let Some(e) = &err {
            for p in [e.start, e.end] {
                if p > range.start && p < range.end {
                    cuts.push(p);
                }
            }
        }
        cuts.sort_unstable();
        for w in cuts.windows(2) {
            let (a, z) = (w[0], w[1]);
            if a == z {
                continue;
            }
            let on_err = err
                .as_ref()
                .is_some_and(|e| a >= e.start && z <= e.end && e.start != e.end);
            let fmt = TextFormat {
                font_id: font.clone(),
                color,
                italics: tok == Tok::Comment,
                underline: if on_err {
                    Stroke::new(1.5, theme::ERROR)
                } else {
                    Stroke::NONE
                },
                background: if on_err {
                    Color32::from_rgba_unmultiplied(240, 96, 86, 28)
                } else {
                    Color32::TRANSPARENT
                },
                ..Default::default()
            };
            job.append(&src[a..z], 0.0, fmt);
        }
    }
    job
}

pub struct ScriptBuf {
    pub text: String,
    pub error: Option<(usize, String)>,
    checked: Option<String>,
    edited: Instant,
    /// The suggestion popup.
    ac: Popup,
}

/// The suggestion popup of one script tab.
#[derive(Default)]
struct Popup {
    /// What the list was worked out for: caret byte, text length, last
    /// edit, Ctrl+Space (unchanged: the list is reused).
    key: Option<(usize, usize, Instant, bool)>,
    /// The byte range a choice replaces, and the list (empty: no popup).
    replace: Range<usize>,
    items: Vec<Item>,
    /// The highlighted row.
    sel: usize,
    /// Esc closed it; it stays closed until the text changes.
    closed: bool,
    /// Ctrl+Space opened it: it lists even before anything is typed.
    forced: bool,
    /// Scroll the highlighted row into view (the keyboard moved it).
    scroll: bool,
}

impl Popup {
    fn open(&self) -> bool {
        !self.closed && !self.items.is_empty()
    }
}

/// Puts row `sel` of the popup in place of the word being typed and moves
/// the caret to its end. False if there was nothing to insert.
fn accept_completion(buf: &mut ScriptBuf, ctx: &egui::Context, id: egui::Id, sel: usize) -> bool {
    let ac = std::mem::take(&mut buf.ac);
    let Some(item) = ac.items.get(sel) else { return false };
    let (range, label) = (ac.replace, &item.label);
    // The range is from last frame; skip it if the text no longer fits it.
    if range.end > buf.text.len()
        || !buf.text.is_char_boundary(range.start)
        || !buf.text.is_char_boundary(range.end)
    {
        return false;
    }
    buf.text.replace_range(range.clone(), label);
    // egui's caret counts characters, not bytes.
    let caret = buf.text[..range.start + label.len()].chars().count();
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(caret))));
    state.store(ctx, id);
    buf.edited = Instant::now();
    true
}

/// The list's small icon: the class's for instances and classes, else a
/// letter on the kind's colour.
fn paint_kind_icon(p: &egui::Painter, r: egui::Rect, item: &Item) {
    if let Some(class) = &item.class {
        icons::paint_class_icon(p, r, class);
        return;
    }
    let (letter, color) = match item.kind {
        ItemKind::Keyword => ("K", Color32::from_rgb(150, 152, 160)),
        ItemKind::Global => ("G", Color32::from_rgb(110, 190, 225)),
        ItemKind::Library => ("L", Color32::from_rgb(120, 195, 120)),
        ItemKind::Local => ("V", Color32::from_rgb(90, 200, 200)),
        ItemKind::Function | ItemKind::Method => ("M", Color32::from_rgb(180, 120, 235)),
        ItemKind::Property => ("P", Color32::from_rgb(80, 150, 245)),
        ItemKind::Event => ("E", Color32::from_rgb(240, 190, 60)),
        ItemKind::Enum | ItemKind::EnumItem => ("#", Color32::from_rgb(235, 145, 75)),
        ItemKind::Child | ItemKind::Class => ("C", Color32::from_rgb(200, 200, 205)),
    };
    p.rect_filled(r, 3.0, color.gamma_multiply(0.22));
    p.rect_stroke(r, 3.0, Stroke::new(1.0, color), egui::StrokeKind::Inside);
    p.text(r.center(), egui::Align2::CENTER_CENTER, letter, FontId::monospace(10.0), color);
}

fn kind_name(k: ItemKind) -> &'static str {
    match k {
        ItemKind::Keyword => "Keyword",
        ItemKind::Global => "Global",
        ItemKind::Library => "Library",
        ItemKind::Local => "Variable",
        ItemKind::Function => "Function",
        ItemKind::Property => "Property",
        ItemKind::Method => "Method",
        ItemKind::Event => "Event",
        ItemKind::Child => "Child",
        ItemKind::Class => "Class",
        ItemKind::Enum => "Enum",
        ItemKind::EnumItem => "Enum item",
    }
}

/// Draws the list at `pos` (under the caret), the highlighted row's
/// signature beside it; returns the row clicked.
fn autocomplete_popup(ctx: &egui::Context, id: egui::Id, pos: egui::Pos2, ac: &mut Popup) -> Option<usize> {
    let font = FontId::proportional(13.0);
    let (row_h, icon) = (20.0, 14.0);
    let mut clicked = None;
    egui::Area::new(id).order(egui::Order::Foreground).fixed_pos(pos).constrain(true).show(ctx, |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let text_w = ac
                .items
                .iter()
                .map(|i| ui.fonts_mut(|f| f.layout_no_wrap(i.label.clone(), font.clone(), theme::TEXT).size().x))
                .fold(100.0f32, f32::max);
            let w = text_w + icon + 18.0;
            egui::Frame::popup(ui.style()).inner_margin(egui::Margin::same(3)).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                egui::ScrollArea::vertical()
                    .id_salt("rows")
                    .max_height(row_h * mech::SCRIPT_AUTOCOMPLETE_ROWS as f32)
                    .auto_shrink([true, true])
                    .show(ui, |ui| ui.vertical(|ui| {
                        for (n, item) in ac.items.iter().enumerate() {
                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, row_h), egui::Sense::click());
                            let p = ui.painter();
                            if n == ac.sel {
                                p.rect_filled(rect, 3.0, theme::SELECT_BG);
                            } else if resp.hovered() {
                                p.rect_filled(rect, 3.0, theme::HOVER_BG);
                            }
                            let ir = egui::Rect::from_center_size(
                                egui::pos2(rect.min.x + 4.0 + icon / 2.0, rect.center().y),
                                egui::vec2(icon, icon),
                            );
                            paint_kind_icon(p, ir, item);
                            p.text(
                                egui::pos2(ir.max.x + 6.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                &item.label,
                                font.clone(),
                                theme::TEXT,
                            );
                            if n == ac.sel && ac.scroll {
                                ui.scroll_to_rect(rect, None);
                            }
                            if resp.clicked() {
                                clicked = Some(n);
                            }
                        }
                    }));
            });
            if let Some(item) = ac.items.get(ac.sel).filter(|i| !i.detail.is_empty()) {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(RichText::new(kind_name(item.kind)).small().color(theme::TEXT_DIM));
                    ui.add(egui::Label::new(RichText::new(&item.detail).monospace().color(theme::TEXT)).wrap());
                });
            }
        });
    });
    ac.scroll = false;
    clicked
}

/// Replaces the selection (or inserts at the caret) with spaces up to the
/// caret's next multiple of `width` columns. False if there's no caret.
fn insert_indent(buf: &mut ScriptBuf, ctx: &egui::Context, id: egui::Id, width: usize) -> bool {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    let Some(range) = state.cursor.char_range() else { return false };
    let [a, z] = [range.primary.index.0, range.secondary.index.0];
    let (a, z) = (a.min(z), a.max(z));
    let byte = |c: usize| buf.text.char_indices().nth(c).map_or(buf.text.len(), |(b, _)| b);
    let (start, end) = (byte(a), byte(z));
    let line_start = buf.text[..start].rfind('\n').map_or(0, |i| i + 1);
    let n = width - buf.text[line_start..start].chars().count() % width;
    buf.text.replace_range(start..end, &" ".repeat(n));
    let caret = egui::text::CCursor::new(a + n);
    state.cursor.set_char_range(Some(egui::text::CCursorRange::one(caret)));
    state.store(ctx, id);
    buf.edited = Instant::now();
    true
}

/// The caret / selection as a byte range, from the text box's saved state.
fn selection_bytes(buf: &ScriptBuf, ctx: &egui::Context, id: egui::Id) -> Option<Range<usize>> {
    let state = egui::TextEdit::load_state(ctx, id)?;
    let range = state.cursor.char_range()?;
    let [a, z] = [range.primary.index.0, range.secondary.index.0];
    let byte = |c: usize| buf.text.char_indices().nth(c).map_or(buf.text.len(), |(b, _)| b);
    Some(byte(a.min(z))..byte(a.max(z)))
}

/// Makes an edit from src/script_typing.rs and puts the caret where it says.
fn apply_edit(buf: &mut ScriptBuf, ctx: &egui::Context, id: egui::Id, e: crate::script_typing::Edit) {
    buf.text.replace_range(e.range, &e.insert);
    let caret = buf.text[..e.caret.min(buf.text.len())].chars().count();
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(caret))));
    state.store(ctx, id);
    buf.edited = Instant::now();
}

/// This frame's first typed character, Enter or Backspace, done the
/// Studio Settings way (src/script_typing.rs) instead of the text box's
/// when one applies: the event is then taken so the text box doesn't see it.
fn typing_edit(
    ui: &mut egui::Ui,
    buf: &mut ScriptBuf,
    id: egui::Id,
    se: &crate::studio_settings::ScriptEditor,
) -> bool {
    use crate::script_typing as typing;
    if !(se.auto_closing_brackets || se.auto_insert_end || se.auto_indent) {
        return false;
    }
    let Some(sel) = selection_bytes(buf, ui.ctx(), id) else { return false };
    let unit = if se.indent_using_spaces { " ".repeat(se.tab_width.max(1) as usize) } else { "\t".into() };
    let text = &buf.text;
    let edit = ui.input_mut(|i| {
        // Other key events (Shift, the key behind a typed character) are
        // passed over: the first event that edits text decides.
        let at = i.events.iter().position(|e| match e {
            egui::Event::Text(_) => true,
            egui::Event::Key { key: egui::Key::Enter | egui::Key::Backspace, pressed: true, .. } => true,
            _ => false,
        })?;
        let edit = match &i.events[at] {
            egui::Event::Text(t) if se.auto_closing_brackets => {
                let mut chars = t.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => typing::typed(text, sel.clone(), c),
                    _ => None,
                }
            }
            egui::Event::Key { key: egui::Key::Enter, modifiers, .. } if modifiers.is_none() => {
                typing::enter(text, sel.clone(), se.auto_insert_end, se.auto_indent, &unit)
            }
            egui::Event::Key { key: egui::Key::Backspace, modifiers, .. }
                if modifiers.is_none() && se.auto_closing_brackets =>
            {
                typing::backspace(text, sel.clone())
            }
            _ => None,
        }?;
        i.events.remove(at);
        Some(edit)
    });
    match edit {
        Some(e) => {
            apply_edit(buf, ui.ctx(), id, e);
            true
        }
        None => false,
    }
}

/// Show Whitespace: a dot for each space and an arrow for each tab, over
/// the rows in view.
fn paint_whitespace(ui: &egui::Ui, galley: &egui::Galley, at: egui::Pos2, color: Color32) {
    let clip = ui.clip_rect();
    let p = ui.painter();
    for row in &galley.rows {
        let origin = at + row.pos.to_vec2();
        let rect = row.rect().translate(at.to_vec2());
        if rect.max.y < clip.min.y || rect.min.y > clip.max.y {
            continue;
        }
        for g in &row.row.glyphs {
            let r = g.logical_rect().translate(origin.to_vec2());
            match g.chr {
                ' ' => {
                    p.circle_filled(r.center(), (r.height() * 0.07).max(1.0), color);
                }
                '\t' => {
                    let y = r.center().y;
                    let (x0, x1) = (r.min.x + 2.0, r.max.x - 2.0);
                    let h = (r.height() * 0.15).max(2.0);
                    let s = Stroke::new(1.0, color);
                    p.line_segment([egui::pos2(x0, y), egui::pos2(x1, y)], s);
                    p.line_segment([egui::pos2(x1 - h, y - h), egui::pos2(x1, y)], s);
                    p.line_segment([egui::pos2(x1 - h, y + h), egui::pos2(x1, y)], s);
                }
                _ => {}
            }
        }
    }
}

#[derive(Default)]
pub struct ScriptTabs {
    pub bufs: HashMap<Ref, ScriptBuf>,
}

impl Editor {
    pub fn script_title(&self, r: Ref) -> String {
        match self.doc.dom.get_by_ref(r) {
            Some(i) => i.name.clone(),
            None => "Script".to_string(),
        }
    }

    pub fn script_tab_ui(&mut self, ui: &mut egui::Ui, r: Ref) {
        self.live_draft_toolbar(ui,r);
        let Some(inst) = self.doc.dom.get_by_ref(r) else {
            ui.add_space(12.0);
            ui.label(RichText::new("This script is not in the DataModel being shown (it may belong to the edit copy while playing).").color(theme::TEXT_DIM));
            return;
        };
        let class = inst.class.to_string();
        let source = match self.doc.prop(r, "Source") {
            Some(Variant::String(s)) => s,
            _ => String::new(),
        };
        let full_name = self.full_name(r);
        let se = self.settings.script_editor.clone();
        let follow_live_source=self.live_script_has_draft(r)==Some(false);
        let buf = self.scripts.bufs.entry(r).or_insert_with(|| ScriptBuf {
            text: source.clone(),
            error: None,
            checked: None,
            edited: Instant::now(),
            ac: Popup::default(),
        });
        let editor_id = egui::Id::new(("script_text", r));
        let focused = ui.ctx().memory(|m| m.has_focus(editor_id));
        // Undo/redo or a script change elsewhere: follow the document.
        if (!focused || follow_live_source) && buf.text != source {
            buf.text = source.clone();
            buf.checked = None;
        }
        // Compile check once typing pauses.
        if buf.checked.as_deref() != Some(buf.text.as_str())
            && buf.edited.elapsed() > Duration::from_millis(250)
        {
            buf.error = runtime::check_syntax(&buf.text, &full_name);
            buf.checked = Some(buf.text.clone());
        }

        ui.horizontal(|ui| {
            icons::class_icon(ui, &class, 16.0);
            ui.label(RichText::new(&full_name).strong());
            ui.label(
                RichText::new(format!("{} lines", buf.text.lines().count().max(1)))
                    .small()
                    .color(theme::TEXT_DIM),
            );
            match &buf.error {
                Some((_, msg)) => {
                    ui.label(RichText::new(format!("⚠ {msg}")).color(theme::ERROR));
                }
                None => {
                    ui.label(
                        RichText::new("✓ no syntax errors")
                            .small()
                            .color(Color32::from_rgb(130, 200, 120)),
                    );
                }
            }
        });
        ui.separator();

        let font = FontId::monospace(se.font_size.clamp(6.0, 48.0));
        let row_h = ui.fonts_mut(|f| f.row_height(&font));
        let error_line = buf.error.as_ref().map(|e| e.0);
        let mut changed = false;
        let scroll = if se.text_wrapping { egui::ScrollArea::vertical() } else { egui::ScrollArea::both() };
        egui::Frame::new().fill(rgb(se.colors.background)).show(ui, |ui| {
        scroll
            .auto_shrink([false, false])
            .id_salt(("script_scroll", r))
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    // Line numbers.
                    let lines = buf.text.split('\n').count().max(1);
                    let digits = lines.to_string().len().max(2);
                    let mut gutter = LayoutJob::default();
                    for n in 1..=lines {
                        let c = if Some(n) == error_line {
                            theme::ERROR
                        } else {
                            theme::TEXT_DIM
                        };
                        let nl = if n == lines { "" } else { "\n" };
                        gutter.append(
                            &format!("{n:>digits$}{nl}"),
                            0.0,
                            TextFormat {
                                font_id: font.clone(),
                                color: c,
                                ..Default::default()
                            },
                        );
                    }
                    let galley = ui.fonts_mut(|f| f.layout_job(gutter));
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(galley.size().x + 10.0, galley.size().y.max(row_h)),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(rect, 0.0, theme::PANEL_DARK);
                    ui.painter()
                        .galley(rect.min + egui::vec2(4.0, 2.0), galley, theme::TEXT_DIM);

                    let mut layouter = |ui: &egui::Ui,
                                        text: &dyn egui::TextBuffer,
                                        wrap: f32|
                     -> Arc<egui::Galley> {
                        let mut job = highlight(text.as_str(), error_line, font.clone(), &se.colors);
                        if se.text_wrapping {
                            job.wrap.max_width = wrap;
                        }
                        ui.fonts_mut(|f| f.layout_job(job))
                    };
                    // Esc: egui drops the text box's focus before this runs, so
                    // with the popup open, take focus back and only close the popup.
                    if buf.ac.open() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        ui.memory_mut(|m| m.request_focus(editor_id));
                        buf.ac.closed = true;
                        buf.ac.forced = false;
                    }
                    let has_focus = ui.memory(|m| m.has_focus(editor_id));
                    // Ctrl+Space: the list for the caret, even before typing.
                    if has_focus
                        && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Space))
                    {
                        buf.ac.forced = true;
                        buf.ac.closed = false;
                        buf.ac.key = None;
                    }
                    // While the popup is open, its keys are taken before the
                    // text box sees them (else ↑/↓ move the caret, Enter adds a line).
                    let rows = if buf.ac.open() { buf.ac.items.len() } else { 0 };
                    if has_focus && rows > 0 {
                        let page = mech::SCRIPT_AUTOCOMPLETE_ROWS;
                        let mut accept = false;
                        let ac = &mut buf.ac;
                        ui.input_mut(|i| {
                            let none = egui::Modifiers::NONE;
                            let sel = ac.sel;
                            if i.consume_key(none, egui::Key::ArrowDown) {
                                ac.sel = (sel + 1) % rows;
                            }
                            if i.consume_key(none, egui::Key::ArrowUp) {
                                ac.sel = (sel + rows - 1) % rows;
                            }
                            if i.consume_key(none, egui::Key::PageDown) {
                                ac.sel = (sel + page).min(rows - 1);
                            }
                            if i.consume_key(none, egui::Key::PageUp) {
                                ac.sel = sel.saturating_sub(page);
                            }
                            ac.scroll |= ac.sel != sel;
                            if i.consume_key(none, egui::Key::Enter) || i.consume_key(none, egui::Key::Tab) {
                                accept = true;
                            }
                        });
                        if accept && accept_completion(buf, ui.ctx(), editor_id, buf.ac.sel) {
                            changed = true;
                        }
                    }
                    // Indent Using Spaces (Studio Settings): Tab puts spaces up
                    // to the next tab stop instead of a tab.
                    if se.indent_using_spaces
                        && has_focus
                        && rows == 0
                        && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab))
                        && insert_indent(buf, ui.ctx(), editor_id, se.tab_width.max(1) as usize)
                    {
                        changed = true;
                    }
                    // Closing brackets, `end` and indentation as you type.
                    if has_focus && rows == 0 && typing_edit(ui, buf, editor_id, &se) {
                        changed = true;
                    }

                    let out =
                        egui::TextEdit::multiline(&mut buf.text)
                            .id(editor_id)
                            .code_editor()
                            .frame(egui::Frame::NONE)
                            .desired_width(f32::INFINITY)
                            .desired_rows(lines.max(24))
                            .lock_focus(true)
                            .layouter(&mut layouter)
                            .show(ui);

                    if se.show_whitespace {
                        paint_whitespace(ui, &out.galley, out.galley_pos, rgb(se.colors.whitespace));
                    }

                    if out.response.changed() {
                        changed = true;
                        buf.edited = Instant::now();
                        buf.ac.closed = false;
                        buf.ac.forced = false;
                    }

                    // The list for where the caret is now (no selection), while
                    // the box has focus; worked out again only when the caret
                    // or the text moved.
                    // With Enable Autocomplete off, only Ctrl+Space shows it.
                    let caret = out.cursor_range.filter(|c| {
                        out.response.has_focus() && c.is_empty() && (se.enable_autocomplete || buf.ac.forced)
                    });
                    match caret {
                        Some(range) => {
                            let char_pos = range.primary.index.0;
                            let cursor = buf
                                .text
                                .char_indices()
                                .nth(char_pos)
                                .map_or(buf.text.len(), |(b, _)| b);
                            let key = (cursor, buf.text.len(), buf.edited, buf.ac.forced);
                            if buf.ac.key != Some(key) {
                                let ctx = CompletionCtx {
                                    dom: &self.doc.dom,
                                    script: r,
                                    api: crate::api::get(),
                                    force: buf.ac.forced,
                                };
                                let (replace, items) = complete(&buf.text, cursor, &ctx);
                                // The same list keeps its highlighted row; a new one starts at the top.
                                let same = items.len() == buf.ac.items.len()
                                    && items.iter().zip(&buf.ac.items).all(|(a, b)| a.label == b.label);
                                if !same {
                                    buf.ac.sel = 0;
                                    buf.ac.scroll = true;
                                }
                                if items.is_empty() {
                                    buf.ac.forced = false;
                                }
                                buf.ac.items = items;
                                buf.ac.replace = replace;
                                buf.ac.key = Some(key);
                            }
                            if buf.ac.open() {
                                let at = out.galley.pos_from_cursor(range.primary);
                                let pos = out.galley_pos + at.left_bottom().to_vec2();
                                let id = egui::Id::new(("autocomplete", r));
                                // A click takes focus from the text box: insert, then give it back.
                                if let Some(n) = autocomplete_popup(ui.ctx(), id, pos, &mut buf.ac) {
                                    if accept_completion(buf, ui.ctx(), editor_id, n) {
                                        changed = true;
                                    }
                                    ui.memory_mut(|m| m.request_focus(editor_id));
                                }
                            }
                        }
                        None => {
                            buf.ac.items.clear();
                            buf.ac.key = None;
                            buf.ac.forced = false;
                        }
                    }
                });
            });
        });
        if changed {
            let text = buf.text.clone();
            self.doc.set_props(
                "Edit script",
                vec![(r, rbx_dom_weak::ustr("Source"), Variant::String(text))],
                Some(format!("source:{r:?}")),
            );
        }
        if buf_needs_repaint(self.scripts.bufs.get(&r)) {
            ui.ctx().request_repaint_after(Duration::from_millis(300));
        }
    }

    fn full_name(&self, r: Ref) -> String {
        let root = self.doc.dom.root_ref();
        let mut parts = Vec::new();
        let mut cur = r;
        while let Some(i) = self.doc.dom.get_by_ref(cur) {
            if cur == root {
                break;
            }
            parts.push(i.name.clone());
            cur = i.parent();
        }
        parts.reverse();
        parts.join(".")
    }
}

fn buf_needs_repaint(b: Option<&ScriptBuf>) -> bool {
    b.is_some_and(|b| b.checked.as_deref() != Some(b.text.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        let t = tokenize("local x = 5 -- hi\nprint(\"a\")");
        let kinds: Vec<Tok> = t
            .iter()
            .filter(|(_, k)| *k != Tok::Plain || false)
            .map(|(_, k)| *k)
            .collect();
        assert_eq!(kinds[0], Tok::Keyword);
        assert!(
            kinds.contains(&Tok::Num)
                && kinds.contains(&Tok::Comment)
                && kinds.contains(&Tok::Str)
                && kinds.contains(&Tok::Global)
        );
    }

    #[test]
    fn const_is_a_keyword() {
        assert_eq!(tokenize("const")[0].1, Tok::Keyword);
    }

    #[test]
    fn syntax_error_line() {
        let e = runtime::check_syntax("local a = 1\nlocal = 2\n", "Workspace.S").unwrap();
        assert_eq!(e.0, 2, "{e:?}");
    }
}
