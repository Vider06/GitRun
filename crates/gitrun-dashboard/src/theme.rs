//! GitRun visual design system.
//!
//! Goal: a dark, technical, monitoring-tool feel (think Grafana / k9s / lazydocker) —
//! not a flat default-egui grey box, but also not a "friendly consumer app" palette.
//! One accent hue (indigo/cyan) is used sparingly for interactive/primary elements;
//! status colors (green/amber/red) are reserved *only* for runner/health state so
//! they keep their meaning and never get diluted by decorative use elsewhere.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle};

/// Base surfaces, from furthest-back to closest-to-the-user.
#[allow(dead_code)]
pub struct Palette {
    pub bg_app: Color32,
    pub bg_panel: Color32,
    pub bg_card: Color32,
    pub bg_card_hover: Color32,
    pub bg_input: Color32,

    pub border: Color32,
    pub border_subtle: Color32,

    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,

    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_muted: Color32,

    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
    pub info: Color32,
}

pub const PALETTE: Palette = Palette {
    bg_app: Color32::from_rgb(15, 17, 21),
    bg_panel: Color32::from_rgb(19, 21, 27),
    bg_card: Color32::from_rgb(24, 27, 34),
    bg_card_hover: Color32::from_rgb(29, 32, 40),
    bg_input: Color32::from_rgb(12, 14, 18),

    border: Color32::from_rgb(38, 42, 51),
    border_subtle: Color32::from_rgb(28, 31, 38),

    text_primary: Color32::from_rgb(226, 229, 235),
    text_secondary: Color32::from_rgb(158, 165, 178),
    text_muted: Color32::from_rgb(102, 109, 122),

    // Indigo-cyan accent: reads as "technical" rather than "corporate blue".
    accent: Color32::from_rgb(99, 141, 249),
    accent_hover: Color32::from_rgb(130, 165, 251),
    accent_muted: Color32::from_rgb(56, 68, 98),

    success: Color32::from_rgb(74, 196, 129),
    warning: Color32::from_rgb(230, 174, 71),
    danger: Color32::from_rgb(233, 96, 96),
    info: Color32::from_rgb(99, 179, 237),
};

/// Applies the full visual theme to the egui context. Call this once at startup,
/// before the first frame (e.g. from `eframe::run_native`'s creation closure).
pub fn install(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let p = &PALETTE;

    // --- Visuals: colors, strokes, rounding ---
    let mut visuals = egui::Visuals::dark();

    visuals.override_text_color = Some(p.text_primary);
    visuals.panel_fill = p.bg_panel;
    visuals.window_fill = p.bg_panel;
    visuals.faint_bg_color = p.bg_card;
    visuals.extreme_bg_color = p.bg_input;
    visuals.code_bg_color = p.bg_input;

    visuals.widgets.noninteractive.bg_fill = p.bg_card;
    visuals.widgets.noninteractive.weak_bg_fill = p.bg_panel;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border_subtle);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text_secondary);
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(8);

    visuals.widgets.inactive.bg_fill = p.bg_card;
    visuals.widgets.inactive.weak_bg_fill = p.bg_card;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, p.text_secondary);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(8);

    visuals.widgets.hovered.bg_fill = p.bg_card_hover;
    visuals.widgets.hovered.weak_bg_fill = p.bg_card_hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent_muted);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, p.text_primary);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(8);

    visuals.widgets.active.bg_fill = p.accent_muted;
    visuals.widgets.active.weak_bg_fill = p.accent_muted;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, p.accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, p.text_primary);
    visuals.widgets.active.corner_radius = CornerRadius::same(8);

    visuals.widgets.open.bg_fill = p.bg_card_hover;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, p.accent_muted);
    visuals.widgets.open.corner_radius = CornerRadius::same(8);

    visuals.selection.bg_fill = p.accent_muted;
    visuals.selection.stroke = Stroke::new(1.0_f32, p.accent);

    visuals.hyperlink_color = p.accent;
    visuals.warn_fg_color = p.warning;
    visuals.error_fg_color = p.danger;

    visuals.window_corner_radius = CornerRadius::same(10);
    visuals.window_stroke = Stroke::new(1.0_f32, p.border);
    visuals.menu_corner_radius = CornerRadius::same(8);

    style.visuals = visuals;

    // --- Spacing: a bit more breathing room than egui's cramped defaults ---
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.window_margin = egui::Margin::same(16);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.indent = 18.0;
    style.spacing.interact_size.y = 28.0;

    // --- Typography: clear hierarchy, monospace where data density matters ---
    style.text_styles = [
        (TextStyle::Heading, FontId::new(20.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();

    ctx.set_style(style);
}

/// Maps a runner/service status to its semantic color. Centralized so "running"
/// always means the same green everywhere in the app, rather than each call site
/// picking its own `Color32::from_rgb(...)`.
pub fn status_color(running: bool) -> Color32 {
    if running { PALETTE.success } else { PALETTE.danger }
}

/// Small helper for the recurring "section card" look: a slightly raised panel
/// with a subtle border, used to group related widgets (a runner row, a config
/// section, a summary stat).
#[allow(dead_code)]
pub fn card(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(PALETTE.bg_card)
        .stroke(Stroke::new(1.0_f32, PALETTE.border_subtle))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(14))
        .show(ui, add_contents);
}

/// A compact colored status pill (e.g. "RUNNING" / "STOPPED"), replacing the
/// ad-hoc `ui.colored_label(Color32::from_rgb(...), "...")` calls scattered
/// through the dashboard.
pub fn status_pill(ui: &mut egui::Ui, label: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.16))
        .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.55)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.colored_label(color, label);
        });
}
