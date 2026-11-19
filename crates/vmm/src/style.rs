//! How the emulator's windows look: one dark theme, cards for groups of settings, and rows for
//! choosing a file or folder.

use egui::{Color32, CornerRadius, Frame, Margin, RichText, Stroke, Ui};

use crate::config::style as tune;

/// Sets the theme on `ctx`. Called once per window.
pub fn apply(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.visuals = egui::Visuals::dark();
        let accent = color(tune::ACCENT);
        let visuals = &mut style.visuals;
        visuals.panel_fill = color(tune::PANEL);
        visuals.window_fill = color(tune::PANEL);
        visuals.extreme_bg_color = color(tune::FIELD);
        visuals.faint_bg_color = color(tune::CARD);
        visuals.selection.bg_fill = accent;
        visuals.selection.stroke = Stroke::new(tune::STROKE, Color32::WHITE);
        visuals.hyperlink_color = accent;
        let radius = CornerRadius::same(tune::RADIUS);
        for widget in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.open,
        ] {
            widget.corner_radius = radius;
        }
        visuals.widgets.inactive.weak_bg_fill = color(tune::BUTTON);
        visuals.widgets.inactive.bg_fill = color(tune::BUTTON);
        visuals.widgets.hovered.weak_bg_fill = color(tune::BUTTON_HOVER);
        visuals.widgets.hovered.bg_fill = color(tune::BUTTON_HOVER);
        visuals.widgets.active.weak_bg_fill = accent;
        visuals.widgets.active.bg_fill = accent;
        let spacing = &mut style.spacing;
        spacing.item_spacing = egui::vec2(tune::GAP, tune::GAP);
        spacing.button_padding = egui::vec2(tune::BUTTON_PAD_X, tune::BUTTON_PAD_Y);
        spacing.interact_size.y = tune::ROW_HEIGHT;
        spacing.slider_width = tune::SLIDER_WIDTH;
        for (text_style, font) in style.text_styles.iter_mut() {
            font.size = match text_style {
                egui::TextStyle::Heading => tune::HEADING_SIZE,
                egui::TextStyle::Small => tune::SMALL_SIZE,
                _ => tune::BODY_SIZE,
            };
        }
    });
}

pub fn color(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

/// A titled card holding a group of controls.
pub fn section<R>(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.add_space(tune::SECTION_GAP);
    Frame::new()
        .fill(color(tune::CARD))
        .corner_radius(CornerRadius::same(tune::CARD_RADIUS))
        .inner_margin(Margin::same(tune::CARD_PAD))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong().color(color(tune::TITLE)));
            ui.add_space(tune::GAP);
            add(ui)
        })
        .inner
}

/// A setting's name on the left and its control on the right.
pub fn row(ui: &mut Ui, name: &str, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(tune::LABEL_WIDTH, tune::ROW_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| ui.label(RichText::new(name).color(color(tune::MUTED))),
        );
        add(ui);
    });
}

/// A setting's name on a line of its own, above a wide control.
pub fn label(ui: &mut Ui, name: &str) {
    ui.label(RichText::new(name).color(color(tune::MUTED)));
}

/// A short coloured word for a state: good (green) or idle (muted).
pub fn state(ui: &mut Ui, word: &str, good: bool) {
    let shade = if good { tune::GOOD } else { tune::MUTED };
    ui.label(RichText::new(word).small().color(color(shade)));
}

/// A line of quieter text under a setting.
pub fn hint(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).small().color(color(tune::MUTED)));
}

/// What a path field chooses.
#[derive(Clone, Copy)]
pub enum Pick {
    Folder,
    File,
}

/// A path field with a button that opens the system's chooser where there is one. True if the
/// path changed.
pub fn path(ui: &mut Ui, value: &mut String, pick: Pick, hint_text: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let button_width = tune::CHOOSE_WIDTH;
        let field_width = (ui.available_width() - button_width - tune::GAP).max(tune::GAP);
        changed |= ui
            .add(
                egui::TextEdit::singleline(value)
                    .hint_text(hint_text)
                    .desired_width(field_width),
            )
            .changed();
        if chooser_available()
            && ui.button(crate::copy::gui::CHOOSE).clicked()
            && let Some(chosen) = choose(pick)
        {
            *value = chosen.display().to_string();
            changed = true;
        }
    });
    changed
}

fn chooser_available() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(target_os = "macos")]
fn choose(pick: Pick) -> Option<std::path::PathBuf> {
    let dialog = rfd::FileDialog::new();
    match pick {
        Pick::Folder => dialog.pick_folder(),
        Pick::File => dialog.pick_file(),
    }
}

#[cfg(not(target_os = "macos"))]
fn choose(_pick: Pick) -> Option<std::path::PathBuf> {
    None
}

/// A button drawn in the accent colour, for the main action of a card.
pub fn primary(ui: &mut Ui, label: &str) -> bool {
    ui.add(
        egui::Button::new(RichText::new(label).color(Color32::WHITE).strong())
            .fill(color(tune::ACCENT)),
    )
    .clicked()
}

/// A button drawn in the warning colour, for actions that delete something.
pub fn danger(ui: &mut Ui, label: &str) -> bool {
    ui.add(egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(color(tune::DANGER)))
        .clicked()
}
