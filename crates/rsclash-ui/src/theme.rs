use egui::{
  Color32, CornerRadius, FontId, Stroke, Style, TextStyle, Theme, ThemePreference, Visuals, vec2,
};
use rsclash_domain::ThemeMode;

pub(crate) fn install_styles(context: &egui::Context) {
  context.set_style_of(Theme::Light, adwaita_style(false));
  context.set_style_of(Theme::Dark, adwaita_style(true));
}

pub(crate) fn apply_preference(context: &egui::Context, mode: ThemeMode) {
  let preference = match mode {
    ThemeMode::System => ThemePreference::System,
    ThemeMode::Light => ThemePreference::Light,
    ThemeMode::Dark => ThemePreference::Dark,
  };
  context.set_theme(preference);
}

fn adwaita_style(dark: bool) -> Style {
  let mut style = Style::default();
  style.spacing.item_spacing = vec2(8.0, 8.0);
  style.spacing.button_padding = vec2(12.0, 7.0);
  style.spacing.interact_size = vec2(40.0, 34.0);
  style.spacing.window_margin = egui::Margin::same(12);
  style.animation_time = 0.12;
  style.visuals = adwaita_visuals(dark);
  style.text_styles.insert(
    TextStyle::Heading,
    FontId::new(26.0, egui::FontFamily::Proportional),
  );
  style.text_styles.insert(
    TextStyle::Body,
    FontId::new(14.5, egui::FontFamily::Proportional),
  );
  style.text_styles.insert(
    TextStyle::Button,
    FontId::new(14.0, egui::FontFamily::Proportional),
  );
  style
}

fn adwaita_visuals(dark: bool) -> Visuals {
  let mut visuals = if dark {
    Visuals::dark()
  } else {
    Visuals::light()
  };

  let (window, panel, raised, hovered, active, border, text, weak_text, accent) = if dark {
    (
      Color32::from_rgb(30, 30, 32),
      Color32::from_rgb(36, 36, 39),
      Color32::from_rgb(44, 44, 48),
      Color32::from_rgb(53, 53, 58),
      Color32::from_rgb(61, 61, 67),
      Color32::from_rgb(70, 70, 76),
      Color32::from_rgb(244, 244, 245),
      Color32::from_rgb(174, 174, 181),
      Color32::from_rgb(53, 132, 228),
    )
  } else {
    (
      Color32::from_rgb(246, 245, 244),
      Color32::from_rgb(250, 250, 250),
      Color32::WHITE,
      Color32::from_rgb(242, 242, 242),
      Color32::from_rgb(232, 232, 232),
      Color32::from_rgb(211, 210, 208),
      Color32::from_rgb(36, 31, 49),
      Color32::from_rgb(94, 92, 100),
      Color32::from_rgb(28, 113, 216),
    )
  };

  visuals.override_text_color = Some(text);
  visuals.weak_text_color = Some(weak_text);
  visuals.panel_fill = panel;
  visuals.window_fill = window;
  visuals.faint_bg_color = raised;
  visuals.extreme_bg_color = if dark {
    Color32::from_rgb(24, 24, 26)
  } else {
    Color32::from_rgb(238, 238, 238)
  };
  visuals.code_bg_color = visuals.extreme_bg_color;
  visuals.window_stroke = Stroke::new(1.0, border);
  visuals.window_corner_radius = CornerRadius::same(12);
  visuals.menu_corner_radius = CornerRadius::same(10);
  visuals.selection.bg_fill = accent;
  visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
  visuals.hyperlink_color = accent;
  visuals.warn_fg_color = Color32::from_rgb(230, 145, 56);
  visuals.error_fg_color = Color32::from_rgb(224, 79, 95);
  visuals.button_frame = true;
  visuals.collapsing_header_frame = false;
  visuals.striped = false;

  for widget in [
    &mut visuals.widgets.noninteractive,
    &mut visuals.widgets.inactive,
  ] {
    widget.bg_fill = raised;
    widget.weak_bg_fill = raised;
    widget.bg_stroke = Stroke::new(1.0, border);
    widget.corner_radius = CornerRadius::same(8);
    widget.fg_stroke = Stroke::new(1.0, text);
  }
  visuals.widgets.hovered.bg_fill = hovered;
  visuals.widgets.hovered.weak_bg_fill = hovered;
  visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, border);
  visuals.widgets.hovered.corner_radius = CornerRadius::same(8);
  visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, text);
  visuals.widgets.active.bg_fill = active;
  visuals.widgets.active.weak_bg_fill = active;
  visuals.widgets.active.bg_stroke = Stroke::new(1.0, accent);
  visuals.widgets.active.corner_radius = CornerRadius::same(8);
  visuals.widgets.active.fg_stroke = Stroke::new(1.0, text);
  visuals.widgets.open = visuals.widgets.active;

  visuals
}
