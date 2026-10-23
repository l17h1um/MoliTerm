use egui::{Color32, Stroke, Visuals, vec2};
use moliterm_core::theme::{Rgb, Theme};

pub fn c(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.0, rgb.1, rgb.2)
}

pub fn visuals(t: &Theme) -> Visuals {
    let u = &t.ui;
    let mut v = if t.dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = c(u.panel);
    v.window_fill = c(u.surface);
    v.extreme_bg_color = c(u.bg);
    v.faint_bg_color = c(u.surface);
    v.code_bg_color = c(u.bg);
    v.window_stroke = Stroke::new(1.0, c(u.border));
    v.hyperlink_color = c(u.accent);
    v.warn_fg_color = c(u.warning);
    v.error_fg_color = c(u.danger);
    v.selection.bg_fill = c(u.selection);
    v.selection.stroke = Stroke::new(1.0, c(u.accent));
    v.text_cursor.stroke.color = c(u.accent);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = c(u.panel);
    w.noninteractive.weak_bg_fill = c(u.panel);
    w.noninteractive.bg_stroke = Stroke::new(1.0, c(u.border));
    w.noninteractive.fg_stroke = Stroke::new(1.0, c(u.fg));
    for (state, fill) in [
        (&mut w.inactive, c(u.surface)),
        (&mut w.hovered, c(u.border)),
        (&mut w.active, c(u.selection)),
        (&mut w.open, c(u.surface)),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.fg_stroke = Stroke::new(1.0, c(u.fg));
    }
    w.inactive.bg_stroke = Stroke::new(1.0, c(u.muted).gamma_multiply(0.55));
    w.hovered.bg_stroke = Stroke::new(1.0, c(u.muted));
    w.active.bg_stroke = Stroke::new(1.0, c(u.accent));
    v
}

/// Single-line text field with the mockup's padding.
pub fn field<'t>(text: &'t mut dyn egui::TextBuffer) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text).margin(egui::Margin::symmetric(8, 5))
}

/// Applies theme, zoom and density to the context.
pub fn apply(ctx: &egui::Context, theme: &Theme, zoom: f32, compact: bool) {
    let which = if theme.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    ctx.set_theme(which);
    ctx.set_visuals_of(which, visuals(theme));
    ctx.style_mut_of(which, |s| {
        // Roomier than egui's defaults, to match the mockups.
        let (pad, height, gap) = if compact {
            (vec2(9.0, 3.0), 22.0, vec2(6.0, 4.0))
        } else {
            (vec2(14.0, 6.0), 30.0, vec2(8.0, 7.0))
        };
        s.spacing.button_padding = pad;
        s.spacing.interact_size.y = height;
        s.spacing.item_spacing = gap;
        s.spacing.icon_width = 16.0;
        s.spacing.icon_width_inner = 9.0;
        s.spacing.icon_spacing = 7.0;
        s.spacing.menu_margin = egui::Margin::same(6);
        let w = &mut s.visuals.widgets;
        for state in [
            &mut w.noninteractive,
            &mut w.inactive,
            &mut w.hovered,
            &mut w.active,
            &mut w.open,
        ] {
            state.corner_radius = egui::CornerRadius::same(6);
        }
        s.visuals.menu_corner_radius = egui::CornerRadius::same(8);
        s.visuals.window_corner_radius = egui::CornerRadius::same(10);
    });
    ctx.set_zoom_factor(zoom);
}

/// Modal content width that still fits the window (SPEC-9 §9.2).
pub fn fit_width(ui: &mut egui::Ui, design: f32) {
    let max = ui.ctx().content_rect().width() - 64.0;
    ui.set_width(design.min(max).max(240.0));
}

/// Tallest a modal's scrolling body may be so the whole dialog stays in the window.
pub fn fit_height(ui: &egui::Ui, reserved: f32) -> f32 {
    (ui.ctx().content_rect().height() - reserved).max(120.0)
}
