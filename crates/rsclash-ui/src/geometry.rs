//! Geometry translated from Clash Verge Rev's React, MUI, and SCSS layout.

pub(crate) const TITLE_BAR_HEIGHT: f32 = 36.0;
pub(crate) const LINUX_CONTENT_TOP: f32 = 5.0;

pub(crate) const NAV_WIDTH: f32 = 200.0;
pub(crate) const NAV_COLLAPSED_WIDTH: f32 = 72.0;
pub(crate) const NAV_COLLAPSED_ITEM_SIZE: f32 = 52.0;
pub(crate) const NAV_COLLAPSED_ITEM_MARGIN: f32 = 6.0;
pub(crate) const NAV_ITEM_HEIGHT: f32 = 48.0;
pub(crate) const NAV_ITEM_OUTER_HEIGHT: f32 = 56.0;
pub(crate) const NAV_ITEM_HORIZONTAL_MARGIN: f32 = 10.0;
pub(crate) const NAV_ITEM_RADIUS: f32 = 8.0;
pub(crate) const NAV_COLLAPSED_ITEM_RADIUS: f32 = 12.0;
pub(crate) const NAV_LOGO_HEIGHT: f32 = 68.0;
pub(crate) const NAV_LOGO_COLLAPSED_HEIGHT: f32 = 68.0;
pub(crate) const NAV_TRAFFIC_HEIGHT: f32 = 158.0;
pub(crate) const NAV_TRAFFIC_GRAPH_HEIGHT: f32 = 60.0;
pub(crate) const NAV_TRAFFIC_HORIZONTAL_PADDING: f32 = 20.0;

pub(crate) const PAGE_HEADER_HEIGHT: f32 = 58.0;
pub(crate) const PAGE_HEADER_HORIZONTAL_PADDING: f32 = 20.0;
pub(crate) const PAGE_CONTENT_HORIZONTAL_MARGIN: f32 = 10.0;
pub(crate) const PAGE_CONTENT_VERTICAL_PADDING: f32 = 10.0;
pub(crate) const GLOBAL_RADIUS: f32 = 8.0;

pub(crate) const MUI_SPACING: f32 = 8.0;
pub(crate) const GRID_GAP: f32 = MUI_SPACING * 1.5;
pub(crate) const TOOLBAR_GAP: f32 = MUI_SPACING;
pub(crate) const PROFILE_TOOLBAR_HEIGHT: f32 = 36.0;
pub(crate) const PROFILE_CONTENT_OFFSET: f32 = 48.0;
pub(crate) const PROFILE_CARD_HEIGHT: f32 = 98.0;
pub(crate) const PROFILE_CARD_LINE_HEIGHT: f32 = 26.0;
pub(crate) const PROFILE_PROGRESS_HEIGHT: f32 = 4.0;
pub(crate) const CONNECTION_TOOLBAR_MIN_HEIGHT: f32 = 36.0;
pub(crate) const RULE_TOOLBAR_HEIGHT: f32 = 36.0;
pub(crate) const LOG_TOOLBAR_HEIGHT: f32 = 39.0;
pub(crate) const CONNECTION_ROW_HEIGHT: f32 = 56.0;
pub(crate) const RULE_ROW_HEIGHT: f32 = 40.0;
pub(crate) const LOG_ROW_HEIGHT: f32 = 50.0;

pub(crate) const BREAKPOINT_SM: f32 = 650.0;
pub(crate) const BREAKPOINT_MD: f32 = 900.0;
pub(crate) const BREAKPOINT_LG: f32 = 1_200.0;
pub(crate) const BREAKPOINT_XL: f32 = 1_536.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ShellLayout {
  pub nav_width: f32,
  pub title_bar_height: f32,
  pub main_height: f32,
  pub content_width: f32,
  pub content_top: f32,
  pub page_header_height: f32,
  pub page_body_height: f32,
}

pub(crate) fn shell_layout(viewport_size: egui::Vec2, navigation_collapsed: bool) -> ShellLayout {
  let nav_width = if navigation_collapsed {
    NAV_COLLAPSED_WIDTH
  } else {
    NAV_WIDTH
  };
  let main_height = (viewport_size.y - TITLE_BAR_HEIGHT).max(0.0);
  let right_content_height = (main_height - LINUX_CONTENT_TOP).max(0.0);
  ShellLayout {
    nav_width,
    title_bar_height: TITLE_BAR_HEIGHT,
    main_height,
    content_width: (viewport_size.x - nav_width).max(0.0),
    content_top: LINUX_CONTENT_TOP,
    page_header_height: PAGE_HEADER_HEIGHT,
    page_body_height: (right_content_height - PAGE_HEADER_HEIGHT).max(0.0),
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Breakpoint {
  Xs,
  Sm,
  Md,
  Lg,
  Xl,
}

pub(crate) fn breakpoint(viewport_width: f32) -> Breakpoint {
  if viewport_width >= BREAKPOINT_XL {
    Breakpoint::Xl
  } else if viewport_width >= BREAKPOINT_LG {
    Breakpoint::Lg
  } else if viewport_width >= BREAKPOINT_MD {
    Breakpoint::Md
  } else if viewport_width >= BREAKPOINT_SM {
    Breakpoint::Sm
  } else {
    Breakpoint::Xs
  }
}

pub(crate) const fn home_grid_columns(value: Breakpoint) -> usize {
  match value {
    Breakpoint::Md | Breakpoint::Lg | Breakpoint::Xl => 2,
    Breakpoint::Xs | Breakpoint::Sm => 1,
  }
}

pub(crate) const fn profile_grid_columns(value: Breakpoint) -> usize {
  match value {
    Breakpoint::Lg | Breakpoint::Xl => 4,
    Breakpoint::Md => 3,
    Breakpoint::Sm => 2,
    Breakpoint::Xs => 1,
  }
}

pub(crate) const fn settings_grid_columns(value: Breakpoint) -> usize {
  match value {
    Breakpoint::Md | Breakpoint::Lg | Breakpoint::Xl => 2,
    Breakpoint::Xs | Breakpoint::Sm => 1,
  }
}

pub(crate) const fn unlock_grid_columns(value: Breakpoint) -> usize {
  match value {
    Breakpoint::Md | Breakpoint::Lg | Breakpoint::Xl => 3,
    Breakpoint::Sm => 2,
    Breakpoint::Xs => 1,
  }
}

pub(crate) const fn proxy_grid_columns(viewport_width: f32, configured: u8) -> usize {
  if configured > 0 && configured < 6 {
    return configured as usize;
  }
  if viewport_width > 1_920.0 {
    5
  } else if viewport_width > 1_450.0 {
    4
  } else if viewport_width > 1_024.0 {
    3
  } else if viewport_width >= 600.0 {
    2
  } else {
    1
  }
}

#[cfg(test)]
mod tests {
  use super::{
    Breakpoint, CONNECTION_ROW_HEIGHT, LINUX_CONTENT_TOP, LOG_ROW_HEIGHT, NAV_COLLAPSED_WIDTH,
    NAV_WIDTH, PAGE_HEADER_HEIGHT, PROFILE_CARD_HEIGHT, PROFILE_CONTENT_OFFSET,
    PROFILE_TOOLBAR_HEIGHT, RULE_ROW_HEIGHT, TITLE_BAR_HEIGHT, breakpoint, home_grid_columns,
    profile_grid_columns, proxy_grid_columns, settings_grid_columns, shell_layout,
    unlock_grid_columns,
  };

  #[test]
  fn cvr_window_geometry_is_stable_at_reference_viewports() {
    let cases = [
      (520.0, Breakpoint::Xs, 1, 1, 1),
      (650.0, Breakpoint::Sm, 1, 2, 1),
      (940.0, Breakpoint::Md, 2, 3, 2),
      (1_200.0, Breakpoint::Lg, 2, 4, 2),
      (1_536.0, Breakpoint::Xl, 2, 4, 2),
    ];

    for (width, expected, home, profiles, settings) in cases {
      let actual = breakpoint(width);
      assert_eq!(actual, expected);
      assert_eq!(home_grid_columns(actual), home);
      assert_eq!(profile_grid_columns(actual), profiles);
      assert_eq!(settings_grid_columns(actual), settings);
      assert_eq!(
        unlock_grid_columns(actual),
        match expected {
          Breakpoint::Xs => 1,
          Breakpoint::Sm => 2,
          Breakpoint::Md | Breakpoint::Lg | Breakpoint::Xl => 3,
        }
      );
    }
  }

  #[test]
  fn cvr_shell_uses_exact_css_dimensions() {
    assert_eq!(TITLE_BAR_HEIGHT, 36.0);
    assert_eq!(NAV_WIDTH, 200.0);
    assert_eq!(NAV_COLLAPSED_WIDTH, 72.0);
    assert_eq!(PAGE_HEADER_HEIGHT, 58.0);
    assert_eq!(LINUX_CONTENT_TOP, 5.0);
  }

  #[test]
  fn proxy_columns_follow_cvr_window_width_calculation() {
    assert_eq!(proxy_grid_columns(599.0, 6), 1);
    assert_eq!(proxy_grid_columns(600.0, 6), 2);
    assert_eq!(proxy_grid_columns(1_025.0, 6), 3);
    assert_eq!(proxy_grid_columns(1_451.0, 6), 4);
    assert_eq!(proxy_grid_columns(1_921.0, 6), 5);
    assert_eq!(proxy_grid_columns(520.0, 3), 3);
  }

  #[test]
  fn shell_rects_match_cvr_at_default_and_minimum_viewports() {
    let default = shell_layout(egui::vec2(940.0, 700.0), false);
    assert_eq!(default.nav_width, 200.0);
    assert_eq!(default.title_bar_height, 36.0);
    assert_eq!(default.main_height, 664.0);
    assert_eq!(default.content_width, 740.0);
    assert_eq!(default.content_top, 5.0);
    assert_eq!(default.page_header_height, 58.0);
    assert_eq!(default.page_body_height, 601.0);

    let minimum = shell_layout(egui::vec2(520.0, 520.0), false);
    assert_eq!(minimum.content_width, 320.0);
    assert_eq!(minimum.page_body_height, 421.0);

    let collapsed = shell_layout(egui::vec2(940.0, 700.0), true);
    assert_eq!(collapsed.nav_width, 72.0);
    assert_eq!(collapsed.content_width, 868.0);

    let wide = shell_layout(egui::vec2(1_600.0, 900.0), false);
    assert_eq!(wide.content_width, 1_400.0);
    assert_eq!(wide.main_height, 864.0);
    assert_eq!(wide.page_body_height, 801.0);
  }

  #[test]
  fn full_page_rows_match_cvr_component_sizes() {
    assert_eq!(PROFILE_CONTENT_OFFSET, 48.0);
    assert_eq!(PROFILE_TOOLBAR_HEIGHT, 36.0);
    assert_eq!(PROFILE_CARD_HEIGHT, 98.0);
    assert_eq!(CONNECTION_ROW_HEIGHT, 56.0);
    assert_eq!(RULE_ROW_HEIGHT, 40.0);
    assert_eq!(LOG_ROW_HEIGHT, 50.0);
  }
}
