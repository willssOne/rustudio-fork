//! File > Studio Settings… (Alt+S): the user's editor preferences, kept
//! between runs in `$XDG_CONFIG_HOME/rustudio/studio-settings.json`.
//!
//! Setting names follow Studio's Settings window (Script Editor section)
//! where Studio has the same setting; the defaults are rustudio's look
//! before this window existed, not Studio's, so nothing changes until the
//! user changes it. "External Editor" is a rustudio addition: Studio has
//! no setting for it (people use Rojo for that).
//!
//! Only runs a person drives read or write the file
//! (`Harness::keeps_studio_settings`): tests, benches and screenshots keep
//! the defaults.
//!
//! Adding a setting: a field on one of the structs below (with
//! `#[serde(default)]` on the struct, older files still load), a row in
//! `settings_window`, and the code that reads it from `ed.settings`.

use egui::{Color32, RichText};
use serde::{Deserialize, Serialize};

use crate::app::Editor;
use crate::theme;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioSettings {
    pub script_editor: ScriptEditor,
    pub external_editor: ExternalEditor,
}

impl Default for StudioSettings {
    fn default() -> Self {
        StudioSettings { script_editor: ScriptEditor::default(), external_editor: ExternalEditor::default() }
    }
}

/// Studio Settings > Script Editor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScriptEditor {
    /// "Font" size, in points (rustudio's editor was 13 before this setting).
    pub font_size: f32,
    /// "Tab Width": spaces a Tab key press inserts with Indent Using Spaces.
    /// (egui always draws a tab character 4 spaces wide.)
    pub tab_width: u8,
    /// "Indent Using Spaces" (Studio's default is off: Tab inserts a tab).
    pub indent_using_spaces: bool,
    /// "Text Wrapping": long lines wrap instead of scrolling sideways.
    pub text_wrapping: bool,
    /// "Enable Autocomplete".
    pub enable_autocomplete: bool,
    /// "Show Whitespace": a dot for each space, an arrow for each tab.
    pub show_whitespace: bool,
    /// Typing `(`, `[`, `{` or a quote adds its closer after the caret;
    /// typing the closer steps over it, Backspace in an empty pair deletes
    /// both (Studio's "Auto Closing Brackets" / "Auto Closing Quotes", as one).
    pub auto_closing_brackets: bool,
    /// Enter after a line that opens a block (`then`, `do`, `function(…)`)
    /// adds the `end`, when the script is missing one, as Studio does.
    pub auto_insert_end: bool,
    /// Enter keeps the line's indentation, one level more after a block
    /// opener (Studio's "Auto Indent Rule", Relative).
    pub auto_indent: bool,
    pub colors: ScriptColors,
}

impl Default for ScriptEditor {
    fn default() -> Self {
        ScriptEditor {
            font_size: 13.0,
            tab_width: 4,
            indent_using_spaces: false,
            text_wrapping: false,
            enable_autocomplete: true,
            show_whitespace: false,
            auto_closing_brackets: true,
            auto_insert_end: true,
            auto_indent: true,
            colors: ScriptColors::default(),
        }
    }
}

/// Studio's "… Color" settings for the script editor, as sRGB.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScriptColors {
    pub background: [u8; 3],
    pub text: [u8; 3],
    pub keyword: [u8; 3],
    pub builtin_function: [u8; 3],
    pub string: [u8; 3],
    pub number: [u8; 3],
    pub comment: [u8; 3],
    pub operator: [u8; 3],
    /// "Whitespace Color" (Show Whitespace's dots and arrows).
    pub whitespace: [u8; 3],
}

impl Default for ScriptColors {
    /// rustudio's colours before they were settings (script_editor.rs).
    fn default() -> Self {
        let c = |c: Color32| [c.r(), c.g(), c.b()];
        ScriptColors {
            background: c(theme::PANEL),
            text: c(theme::TEXT),
            keyword: [242, 166, 60],
            builtin_function: [110, 190, 225],
            string: [152, 202, 118],
            number: [214, 150, 222],
            comment: [118, 126, 140],
            operator: [190, 196, 206],
            whitespace: [72, 76, 86],
        }
    }
}

pub fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// Script Editor > External Editor: open scripts in another program.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalEditor {
    /// Open scripts with the command below instead of a script tab.
    pub enabled: bool,
    /// The preset picked (`crate::external_editor::PRESETS` id), or "custom".
    pub preset: String,
    /// The command run: `{file}`, `{line}` and `{folder}` are filled in.
    pub command: String,
}

impl Default for ExternalEditor {
    fn default() -> Self {
        let p = &crate::external_editor::PRESETS[0];
        ExternalEditor { enabled: false, preset: p.id.into(), command: p.command.into() }
    }
}

impl crate::app::Harness {
    /// Studio Settings are the user's, like the in-game ones: kept for any
    /// run a person drives, never for tests, benches or screenshots.
    pub fn keeps_studio_settings(&self) -> bool {
        self.keeps_game_settings()
    }
}

fn path() -> std::path::PathBuf {
    crate::paths::config().join("rustudio").join("studio-settings.json")
}

pub fn load() -> StudioSettings {
    let Ok(text) = std::fs::read_to_string(path()) else { return StudioSettings::default() };
    serde_json::from_str(&text).unwrap_or_else(|e| {
        eprintln!("rustudio: ignoring {}: {e}", path().display());
        StudioSettings::default()
    })
}

pub fn save(s: &StudioSettings) -> Result<(), String> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    // Written beside it, then renamed: a crash mid-write keeps the old file.
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// The window's state while it's open.
#[derive(Default)]
pub struct SettingsWindow {
    pub open: bool,
    /// Changed since last written (written once the mouse is up).
    dirty: bool,
}

impl Editor {
    pub fn open_studio_settings(&mut self) {
        self.studio_settings.open = true;
    }

    /// Draws Studio Settings when open (once a frame). Changes apply at once.
    pub fn studio_settings_window(&mut self, ctx: &egui::Context) {
        if !self.studio_settings.open {
            return;
        }
        let mut open = true;
        let mut changed = false;
        let mut s = self.settings.clone();
        egui::Window::new("Studio Settings")
            .collapsible(false)
            .resizable(true)
            .default_width(480.0)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .open(&mut open)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(ctx.content_rect().height() * 0.75).show(ui, |ui| {
                    egui::CollapsingHeader::new(RichText::new("Script Editor").strong())
                        .default_open(true)
                        .show(ui, |ui| changed |= script_editor_ui(ui, &mut s.script_editor));
                    egui::CollapsingHeader::new(RichText::new("Script Editor Colors").strong())
                        .default_open(false)
                        .show(ui, |ui| changed |= colors_ui(ui, &mut s.script_editor.colors));
                    egui::CollapsingHeader::new(RichText::new("External Editor").strong())
                        .default_open(true)
                        .show(ui, |ui| changed |= external_editor_ui(ui, &mut s.external_editor));
                    ui.add_space(6.0);
                    ui.separator();
                    if ui.button("Reset all to defaults").clicked() {
                        s = StudioSettings::default();
                        changed = true;
                    }
                });
            });
        if changed {
            self.settings = s;
            self.studio_settings.dirty = true;
        }
        let mouse_up = !ctx.input(|i| i.pointer.any_down());
        if self.studio_settings.dirty && (mouse_up || !open) {
            self.studio_settings.dirty = false;
            if self.harness.keeps_studio_settings() {
                if let Err(e) = save(&self.settings) {
                    self.error(format!("Could not save Studio Settings to {}: {e}", path().display()));
                }
            }
        }
        self.studio_settings.open = open;
    }
}

fn script_editor_ui(ui: &mut egui::Ui, se: &mut ScriptEditor) -> bool {
    let mut changed = false;
    egui::Grid::new("studio_settings_script").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
        ui.label("Font Size");
        changed |= ui.add(egui::DragValue::new(&mut se.font_size).range(8.0..=32.0).speed(0.2)).changed();
        ui.end_row();
        ui.label("Tab Width");
        changed |= ui.add(egui::DragValue::new(&mut se.tab_width).range(1..=8)).changed();
        ui.end_row();
        ui.label("Indent Using Spaces");
        changed |= ui.checkbox(&mut se.indent_using_spaces, "").changed();
        ui.end_row();
        ui.label("Text Wrapping");
        changed |= ui.checkbox(&mut se.text_wrapping, "").changed();
        ui.end_row();
        ui.label("Enable Autocomplete");
        changed |= ui.checkbox(&mut se.enable_autocomplete, "").changed();
        ui.end_row();
        ui.label("Show Whitespace");
        changed |= ui.checkbox(&mut se.show_whitespace, "").changed();
        ui.end_row();
        ui.label("Auto Closing Brackets");
        changed |= ui.checkbox(&mut se.auto_closing_brackets, "").changed();
        ui.end_row();
        ui.label("Auto Insert End");
        changed |= ui.checkbox(&mut se.auto_insert_end, "").changed();
        ui.end_row();
        ui.label("Auto Indent");
        changed |= ui.checkbox(&mut se.auto_indent, "").changed();
        ui.end_row();
    });
    changed
}

fn colors_ui(ui: &mut egui::Ui, c: &mut ScriptColors) -> bool {
    let mut changed = false;
    egui::Grid::new("studio_settings_colors").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
        for (name, v) in [
            ("Background Color", &mut c.background),
            ("Text Color", &mut c.text),
            ("Keyword Color", &mut c.keyword),
            ("Built-in Function Color", &mut c.builtin_function),
            ("String Color", &mut c.string),
            ("Number Color", &mut c.number),
            ("Comment Color", &mut c.comment),
            ("Operator Color", &mut c.operator),
            ("Whitespace Color", &mut c.whitespace),
        ] {
            ui.label(name);
            changed |= ui.color_edit_button_srgb(v).changed();
            ui.end_row();
        }
    });
    changed
}

fn external_editor_ui(ui: &mut egui::Ui, ext: &mut ExternalEditor) -> bool {
    use crate::external_editor::{program_found, PRESETS};
    let mut changed = false;
    changed |= ui.checkbox(&mut ext.enabled, "Open scripts in an external editor").changed();
    ui.add_enabled_ui(ext.enabled, |ui| {
        egui::Grid::new("studio_settings_external").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
            ui.label("Editor");
            let current = PRESETS.iter().find(|p| p.id == ext.preset).map_or("Custom command", |p| p.name);
            egui::ComboBox::from_id_salt("external_editor_preset").selected_text(current).show_ui(ui, |ui| {
                for p in PRESETS {
                    let found = program_found(p.command);
                    let label = if found { p.name.to_string() } else { format!("{} (not found)", p.name) };
                    if ui.selectable_label(ext.preset == p.id, label).clicked() {
                        ext.preset = p.id.into();
                        ext.command = p.command.into();
                        changed = true;
                    }
                }
                if ui.selectable_label(ext.preset == "custom", "Custom command").clicked() {
                    ext.preset = "custom".into();
                    changed = true;
                }
            });
            ui.end_row();
            ui.label("Command");
            if ui.add(egui::TextEdit::singleline(&mut ext.command).desired_width(300.0)).changed() {
                ext.preset = PRESETS.iter().find(|p| p.command == ext.command).map_or("custom", |p| p.id).into();
                changed = true;
            }
            ui.end_row();
        });
        ui.label(
            RichText::new(
                "{file} is the script's file, {line} its line, {folder} the folder holding all the place's \
                 scripts (with a sourcemap.json for luau-lsp). Saving the file updates the script; Explorer's \
                 \"Edit Source in rustudio\" still opens the built-in editor.",
            )
            .small()
            .color(theme::TEXT_DIM),
        );
        if !ext.command.trim().is_empty() && !program_found(&ext.command) {
            ui.colored_label(theme::WARN, "That program isn't on PATH.");
        }
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_loads_partial_files() {
        let mut s = StudioSettings::default();
        s.script_editor.font_size = 17.0;
        s.external_editor.enabled = true;
        let back: StudioSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        // An older or hand-written file keeps the defaults for what it lacks.
        let partial: StudioSettings = serde_json::from_str(r#"{"script_editor":{"tab_width":2}}"#).unwrap();
        assert_eq!(partial.script_editor.tab_width, 2);
        assert_eq!(partial.script_editor.font_size, ScriptEditor::default().font_size);
        assert_eq!(partial.external_editor, ExternalEditor::default());
    }
}
