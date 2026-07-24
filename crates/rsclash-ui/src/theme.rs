use egui::{
  Color32, CornerRadius, FontId, Shadow, Stroke, Style, TextStyle, Theme, ThemePreference, Visuals,
  vec2,
};
use rsclash_domain::ThemeMode;

#[derive(Clone, Copy)]
pub(crate) struct Tokens {
  pub canvas: Color32,
  pub sidebar: Color32,
  pub surface: Color32,
  pub surface_raised: Color32,
  pub border: Color32,
  pub text_muted: Color32,
  pub accent: Color32,
  pub accent_soft: Color32,
  pub success: Color32,
  pub warning: Color32,
  pub danger: Color32,
}

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

pub(crate) fn tokens(ui: &egui::Ui) -> Tokens {
  if ui.visuals().dark_mode {
    Tokens {
      canvas: Color32::from_rgb(24, 25, 27),
      sidebar: Color32::from_rgb(30, 31, 34),
      surface: Color32::from_rgb(35, 36, 39),
      surface_raised: Color32::from_rgb(42, 43, 47),
      border: Color32::from_rgb(59, 60, 65),
      text_muted: Color32::from_rgb(166, 167, 174),
      accent: Color32::from_rgb(120, 174, 237),
      accent_soft: Color32::from_rgb(39, 62, 87),
      success: Color32::from_rgb(87, 200, 132),
      warning: Color32::from_rgb(235, 174, 73),
      danger: Color32::from_rgb(238, 111, 121),
    }
  } else {
    Tokens {
      canvas: Color32::from_rgb(247, 247, 248),
      sidebar: Color32::from_rgb(242, 242, 244),
      surface: Color32::from_rgb(255, 255, 255),
      surface_raised: Color32::from_rgb(249, 249, 250),
      border: Color32::from_rgb(220, 221, 224),
      text_muted: Color32::from_rgb(101, 102, 109),
      accent: Color32::from_rgb(45, 112, 187),
      accent_soft: Color32::from_rgb(225, 237, 250),
      success: Color32::from_rgb(38, 150, 90),
      warning: Color32::from_rgb(190, 117, 24),
      danger: Color32::from_rgb(192, 55, 67),
    }
  }
}

fn adwaita_style(dark: bool) -> Style {
  let mut style = Style::default();
  style.spacing.item_spacing = vec2(8.0, 9.0);
  style.spacing.button_padding = vec2(12.0, 8.0);
  style.spacing.interact_size = vec2(40.0, 36.0);
  style.spacing.window_margin = egui::Margin::same(16);
  let mut scroll = egui::style::ScrollStyle::solid();
  scroll.bar_width = 8.0;
  scroll.bar_inner_margin = 0.0;
  scroll.handle_min_length = 24.0;
  scroll.fade.strength = 0.0;
  style.spacing.scroll = scroll;
  style.animation_time = 0.1;
  style.visuals = adwaita_visuals(dark);
  style.text_styles.insert(
    TextStyle::Heading,
    FontId::new(24.0, egui::FontFamily::Proportional),
  );
  style.text_styles.insert(
    TextStyle::Body,
    FontId::new(14.0, egui::FontFamily::Proportional),
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
  let token = if dark {
    Tokens {
      canvas: Color32::from_rgb(24, 25, 27),
      sidebar: Color32::from_rgb(30, 31, 34),
      surface: Color32::from_rgb(35, 36, 39),
      surface_raised: Color32::from_rgb(42, 43, 47),
      border: Color32::from_rgb(59, 60, 65),
      text_muted: Color32::from_rgb(166, 167, 174),
      accent: Color32::from_rgb(120, 174, 237),
      accent_soft: Color32::from_rgb(39, 62, 87),
      success: Color32::from_rgb(87, 200, 132),
      warning: Color32::from_rgb(235, 174, 73),
      danger: Color32::from_rgb(238, 111, 121),
    }
  } else {
    Tokens {
      canvas: Color32::from_rgb(247, 247, 248),
      sidebar: Color32::from_rgb(242, 242, 244),
      surface: Color32::WHITE,
      surface_raised: Color32::from_rgb(249, 249, 250),
      border: Color32::from_rgb(220, 221, 224),
      text_muted: Color32::from_rgb(101, 102, 109),
      accent: Color32::from_rgb(45, 112, 187),
      accent_soft: Color32::from_rgb(225, 237, 250),
      success: Color32::from_rgb(38, 150, 90),
      warning: Color32::from_rgb(190, 117, 24),
      danger: Color32::from_rgb(192, 55, 67),
    }
  };

  let text = if dark {
    Color32::from_rgb(242, 242, 244)
  } else {
    Color32::from_rgb(32, 33, 37)
  };

  visuals.override_text_color = Some(text);
  visuals.weak_text_color = Some(token.text_muted);
  visuals.panel_fill = token.sidebar;
  visuals.window_fill = token.canvas;
  visuals.faint_bg_color = token.surface_raised;
  visuals.extreme_bg_color = token.canvas;
  visuals.code_bg_color = visuals.extreme_bg_color;
  visuals.window_stroke = Stroke::new(1.0, token.border);
  visuals.window_corner_radius = CornerRadius::same(4);
  visuals.menu_corner_radius = CornerRadius::same(4);
  visuals.window_shadow = Shadow {
    offset: [0, 5],
    blur: 18,
    spread: 0,
    color: Color32::from_black_alpha(if dark { 70 } else { 22 }),
  };
  visuals.selection.bg_fill = token.accent;
  visuals.selection.stroke = Stroke::new(1.0, token.surface);
  visuals.hyperlink_color = token.accent;
  visuals.warn_fg_color = token.warning;
  visuals.error_fg_color = token.danger;
  visuals.button_frame = true;
  visuals.collapsing_header_frame = false;
  visuals.striped = false;

  for widget in [
    &mut visuals.widgets.noninteractive,
    &mut visuals.widgets.inactive,
  ] {
    widget.bg_fill = token.surface;
    widget.weak_bg_fill = token.surface;
    widget.bg_stroke = Stroke::new(1.0, token.border);
    widget.corner_radius = CornerRadius::same(4);
    widget.fg_stroke = Stroke::new(1.0, text);
  }
  visuals.widgets.hovered.bg_fill = token.surface_raised;
  visuals.widgets.hovered.weak_bg_fill = token.surface_raised;
  visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, token.border);
  visuals.widgets.hovered.corner_radius = CornerRadius::same(4);
  visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, text);
  visuals.widgets.active.bg_fill = token.accent_soft;
  visuals.widgets.active.weak_bg_fill = token.accent_soft;
  visuals.widgets.active.bg_stroke = Stroke::new(1.0, token.accent);
  visuals.widgets.active.corner_radius = CornerRadius::same(4);
  visuals.widgets.active.fg_stroke = Stroke::new(1.0, text);
  visuals.widgets.open = visuals.widgets.active;

  visuals
}
