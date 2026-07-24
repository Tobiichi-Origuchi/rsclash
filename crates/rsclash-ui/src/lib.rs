//! Native egui presentation layer. This crate only talks to the application protocol.

mod geometry;
mod theme;

use std::{
  collections::{BTreeMap, BTreeSet},
  hash::{DefaultHasher, Hash as _, Hasher as _},
  path::PathBuf,
  sync::Arc,
  time::{SystemTime, UNIX_EPOCH},
};

use egui::{Align, Color32, Frame, Layout, RichText, ScrollArea, Stroke, Ui};
use rsclash_app::{AppClient, AppEventReceiver, ClientError};
use rsclash_domain::{
  AppEvent, AppSettings, AppSnapshot, AppStatus, ApplicationDirectory, ConnectionSnapshot,
  CoreChannel, CoreRunMode, CoreState, DnsEnhancedMode, LogSnapshot, MetricPoint, MihomoConnection,
  MihomoSnapshot, NavigationLayout, Page, ProfileDiagnosticStage, ProfileDiagnostics,
  ProfileDownloadProxy, ProfileOperationKind, ProfileQrCode, ProfileSourceKind, ProfileSummary,
  ProxyCapabilities, ProxyGroupLayout, ProxyGroupView, ProxyMemberSnapshot,
  ProxyMemberUnresolvedReason, ProxyMode, ProxyNodeSnapshot, ProxyNodeSource, ProxyViewV1,
  RemoteProfileOptions, RuleSnapshot, SensitiveString, StreamLogLevel, SystemProxyView, ThemeMode,
  TrayClickAction, TunStack, UiCommand,
};

struct ProfileEditor {
  uid: String,
  name: String,
  content: String,
  dirty: bool,
  highlighter: YamlHighlightCache,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SequenceEditorKind {
  Rules,
  Proxies,
  Groups,
}

impl SequenceEditorKind {
  const fn label(self) -> &'static str {
    match self {
      Self::Rules => "规则",
      Self::Proxies => "代理",
      Self::Groups => "代理组",
    }
  }

  fn default_item(self) -> String {
    match self {
      Self::Rules => "DOMAIN-SUFFIX,example.com,DIRECT".to_string(),
      Self::Proxies => "name: New proxy\ntype: direct".to_string(),
      Self::Groups => "name: New group\ntype: select\nproxies: []".to_string(),
    }
  }
}

struct SequenceEditor {
  uid: String,
  name: String,
  kind: SequenceEditorKind,
  prepend: Vec<String>,
  append: Vec<String>,
  delete: Vec<String>,
  dirty: bool,
  error: Option<String>,
}

struct PendingSequenceEditor {
  uid: String,
  name: String,
  kind: SequenceEditorKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ProxySort {
  #[default]
  Configuration,
  Name,
  Delay,
}

struct ProxyDisplayItem {
  name: String,
  kind: String,
  record_id: Option<String>,
  alive: bool,
  delay_ms: Option<u32>,
  source: String,
  capabilities: ProxyCapabilities,
  unresolved: Option<ProxyMemberUnresolvedReason>,
  chain_eligible: bool,
}

#[derive(Clone)]
struct ProxyChainDrag(usize);

#[derive(Clone, Copy)]
enum ProxyChainAction {
  MoveUp(usize),
  MoveDown(usize),
  Remove(usize),
  Drop { from: usize, to: usize },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ConnectionSort {
  #[default]
  Traffic,
  Destination,
  Process,
  Started,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SettingsSection {
  #[default]
  General,
  Proxy,
  Mihomo,
  DnsTun,
  Interface,
  Maintenance,
}

impl SettingsSection {
  const fn label(self) -> &'static str {
    match self {
      Self::General => "常规",
      Self::Proxy => "代理控制",
      Self::Mihomo => "Mihomo",
      Self::DnsTun => "DNS 与 TUN",
      Self::Interface => "界面与行为",
      Self::Maintenance => "维护",
    }
  }
}

enum SettingsUiAction {
  ToggleSystemProxy(bool),
  InstallService,
  UninstallService,
  RegisterDeepLinks,
  OpenDirectory(ApplicationDirectory),
  OpenWebUi,
  RestartCore(CoreChannel),
}

struct RuleDraft {
  kind: String,
  payload: String,
  target: String,
  no_resolve: bool,
}

impl Default for RuleDraft {
  fn default() -> Self {
    Self {
      kind: "DOMAIN-SUFFIX".to_string(),
      payload: "example.com".to_string(),
      target: "DIRECT".to_string(),
      no_resolve: false,
    }
  }
}

#[derive(Default)]
struct YamlHighlightCache {
  source_hash: u64,
  dark_mode: bool,
  initialized: bool,
  job: egui::text::LayoutJob,
}

pub struct RsClashUi {
  client: AppClient,
  events: AppEventReceiver,
  snapshot: Arc<AppSnapshot>,
  applied_theme: Option<ThemeMode>,
  applied_window_visibility: Option<bool>,
  local_error: Option<String>,
  close_to_tray: bool,
  home_settings_dialog: bool,
  local_profile_name: String,
  local_profile_path: String,
  remote_profile_name: String,
  remote_profile_url: String,
  remote_profile_options: RemoteProfileOptions,
  qr_profile_name: String,
  qr_profile_path: String,
  profile_qr: Option<ProfileQrCode>,
  renaming_profile: Option<String>,
  profile_name_edits: BTreeMap<String, String>,
  pending_profile_delete: Option<String>,
  profile_create_dialog: bool,
  profile_batch_mode: bool,
  selected_profiles: BTreeSet<String>,
  pending_batch_delete: bool,
  editing_profile_options: Option<String>,
  profile_options_edits: BTreeMap<String, RemoteProfileOptions>,
  profile_editor: Option<ProfileEditor>,
  pending_profile_editor_name: Option<(String, String)>,
  sequence_editor: Option<SequenceEditor>,
  pending_sequence_editor: Option<PendingSequenceEditor>,
  pending_editor_close: bool,
  proxy_search: String,
  proxy_regex: bool,
  proxy_whole_word: bool,
  proxy_detailed: bool,
  proxy_filter_visible: bool,
  proxy_sort: ProxySort,
  expanded_proxy_groups: BTreeSet<String>,
  locate_proxy: Option<(String, String)>,
  proxy_provider_dialog: bool,
  proxy_chain_mode: bool,
  proxy_chain_group: String,
  proxy_chain_nodes: Vec<String>,
  rule_search: String,
  rule_draft: RuleDraft,
  rule_editor_dialog: bool,
  pending_rule_append: Option<String>,
  connection_search: String,
  show_closed_connections: bool,
  connection_table_layout: bool,
  connection_sort: ConnectionSort,
  connection_show_process: bool,
  connection_show_rule: bool,
  connection_show_chains: bool,
  selected_connection: Option<String>,
  log_search: String,
  log_reverse: bool,
  log_level: StreamLogLevel,
  navigation_collapsed: bool,
  settings_dialog: Option<SettingsSection>,
  settings_draft: AppSettings,
  settings_dirty: bool,
}

impl RsClashUi {
  pub fn new(context: &egui::Context, client: AppClient, close_to_tray: bool) -> Self {
    theme::install_styles(context);
    let snapshot = client.current_snapshot();
    theme::apply_preference(context, snapshot.theme);
    let settings_draft = snapshot.settings.value.clone();
    let proxy_detailed = settings_draft.proxy_group_layout == ProxyGroupLayout::Cards;
    let connection_show_process = settings_draft
      .connection_columns
      .iter()
      .any(|column| column == "process");
    let connection_show_rule = settings_draft
      .connection_columns
      .iter()
      .any(|column| column == "rule");
    let connection_show_chains = settings_draft
      .connection_columns
      .iter()
      .any(|column| column == "chains");

    Self {
      events: client.subscribe_events(),
      client,
      snapshot,
      applied_theme: None,
      applied_window_visibility: None,
      local_error: None,
      close_to_tray,
      home_settings_dialog: false,
      local_profile_name: String::new(),
      local_profile_path: String::new(),
      remote_profile_name: String::new(),
      remote_profile_url: String::new(),
      remote_profile_options: RemoteProfileOptions::default(),
      qr_profile_name: String::new(),
      qr_profile_path: String::new(),
      profile_qr: None,
      renaming_profile: None,
      profile_name_edits: BTreeMap::new(),
      pending_profile_delete: None,
      profile_create_dialog: false,
      profile_batch_mode: false,
      selected_profiles: BTreeSet::new(),
      pending_batch_delete: false,
      editing_profile_options: None,
      profile_options_edits: BTreeMap::new(),
      profile_editor: None,
      pending_profile_editor_name: None,
      sequence_editor: None,
      pending_sequence_editor: None,
      pending_editor_close: false,
      proxy_search: String::new(),
      proxy_regex: false,
      proxy_whole_word: false,
      proxy_detailed,
      proxy_filter_visible: false,
      proxy_sort: ProxySort::default(),
      expanded_proxy_groups: BTreeSet::new(),
      locate_proxy: None,
      proxy_provider_dialog: false,
      proxy_chain_mode: false,
      proxy_chain_group: String::new(),
      proxy_chain_nodes: Vec::new(),
      rule_search: String::new(),
      rule_draft: RuleDraft::default(),
      rule_editor_dialog: false,
      pending_rule_append: None,
      connection_search: String::new(),
      show_closed_connections: false,
      connection_table_layout: false,
      connection_sort: ConnectionSort::default(),
      connection_show_process,
      connection_show_rule,
      connection_show_chains,
      selected_connection: None,
      log_search: String::new(),
      log_reverse: false,
      log_level: StreamLogLevel::Info,
      navigation_collapsed: false,
      settings_dialog: None,
      settings_draft,
      settings_dirty: false,
    }
  }

  /// Synchronize background state without painting. This is called even when the root viewport is hidden.
  pub fn logic(&mut self, context: &egui::Context) {
    if let Some(snapshot) = self.client.take_snapshot_if_changed() {
      let was_chain_connected = self.snapshot.mihomo.proxy_chain.connected;
      if snapshot.mihomo.proxy_chain.connected {
        self.proxy_chain_group = snapshot
          .mihomo
          .proxy_chain
          .group
          .clone()
          .unwrap_or_default();
        self.proxy_chain_nodes = snapshot.mihomo.proxy_chain.nodes.clone();
      } else if was_chain_connected {
        self.proxy_chain_nodes.clear();
      }
      if !self.settings_dirty {
        self.settings_draft = snapshot.settings.value.clone();
      }
      self.snapshot = snapshot;
    }

    while let Some(event) = self.events.try_recv() {
      match event {
        AppEvent::ProfileContentLoaded { uid, content } => {
          if let Some(pending) = self
            .pending_sequence_editor
            .take()
            .filter(|pending| pending.uid == uid)
          {
            match parse_sequence_editor(pending.uid, pending.name, pending.kind, content.expose()) {
              Ok(mut editor) => {
                if let Some(rule) = self.pending_rule_append.take() {
                  editor.append.push(rule);
                  editor.dirty = true;
                }
                self.profile_editor = None;
                self.sequence_editor = Some(editor);
                self.pending_editor_close = false;
              },
              Err(error) => {
                self.local_error = Some(error);
              },
            }
            continue;
          }
          let name = self
            .pending_profile_editor_name
            .take()
            .filter(|(pending_uid, _)| pending_uid == &uid)
            .map(|(_, name)| name)
            .or_else(|| {
              self
                .snapshot
                .profiles
                .items
                .iter()
                .find(|profile| profile.uid == uid)
                .map(|profile| profile.name.clone())
            })
            .unwrap_or_else(|| uid.clone());
          self.profile_editor = Some(ProfileEditor {
            uid,
            name,
            content: content.into_inner(),
            dirty: false,
            highlighter: YamlHighlightCache::default(),
          });
          self.sequence_editor = None;
          self.pending_editor_close = false;
        },
        AppEvent::ProfileContentSaved { uid } => {
          if let Some(editor) = self
            .profile_editor
            .as_mut()
            .filter(|editor| editor.uid == uid)
          {
            editor.dirty = false;
          }
          if let Some(editor) = self
            .sequence_editor
            .as_mut()
            .filter(|editor| editor.uid == uid)
          {
            editor.dirty = false;
            editor.error = None;
          }
        },
        AppEvent::ProfileQrReady(qr) => {
          self.profile_qr = Some(qr);
        },
        AppEvent::SettingsChanged => {
          if !self.snapshot.settings.busy {
            self.settings_dirty = false;
            self.settings_draft = self.snapshot.settings.value.clone();
            self.sync_presentation_settings();
          }
        },
        _ => {},
      }
    }

    if self.snapshot.page == Page::Profiles {
      let dropped = context.input(|input| {
        input
          .raw
          .dropped_files
          .iter()
          .filter_map(|file| file.path.clone())
          .collect::<Vec<_>>()
      });
      for path in dropped {
        self.import_dropped_profile(path);
      }
    }

    if self.applied_theme != Some(self.snapshot.theme) {
      theme::apply_preference(context, self.snapshot.theme);
      self.applied_theme = Some(self.snapshot.theme);
    }

    if self.applied_window_visibility != Some(self.snapshot.window_visible) {
      context.send_viewport_cmd(egui::ViewportCommand::Visible(self.snapshot.window_visible));
      self.applied_window_visibility = Some(self.snapshot.window_visible);
    }

    let close_requested = context.input(|input| input.viewport().close_requested());
    if close_requested && self.snapshot.status != AppStatus::ShuttingDown {
      if self.tray_is_available() {
        context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        self.command(UiCommand::SetWindowVisible(false));
      } else {
        self.command(UiCommand::Shutdown);
      }
    }

    if self.snapshot.status == AppStatus::ShuttingDown {
      context.send_viewport_cmd(egui::ViewportCommand::Close);
    }
  }

  fn tray_is_available(&self) -> bool {
    self.close_to_tray && self.snapshot.settings.value.show_tray
  }

  fn sync_presentation_settings(&mut self) {
    let settings = &self.snapshot.settings.value;
    self.proxy_detailed = settings.proxy_group_layout == ProxyGroupLayout::Cards;
    self.connection_show_process = settings
      .connection_columns
      .iter()
      .any(|column| column == "process");
    self.connection_show_rule = settings
      .connection_columns
      .iter()
      .any(|column| column == "rule");
    self.connection_show_chains = settings
      .connection_columns
      .iter()
      .any(|column| column == "chains");
  }

  pub fn ui(&mut self, root: &mut Ui) {
    let compact_navigation = match self.snapshot.settings.value.navigation_layout {
      NavigationLayout::Automatic => self.navigation_collapsed,
      NavigationLayout::Expanded => false,
      NavigationLayout::Compact => true,
    };
    let shell = geometry::shell_layout(root.max_rect().size(), compact_navigation);
    debug_assert!(shell.content_width >= 0.0);
    debug_assert!(shell.main_height >= shell.page_body_height);
    self.title_bar(root, shell.title_bar_height);
    egui::Panel::left("navigation")
      .exact_size(shell.nav_width)
      .frame(
        Frame::side_top_panel(root.style())
          .fill(root.visuals().panel_fill)
          .stroke(Stroke::NONE)
          .inner_margin(egui::Margin::ZERO),
      )
      .show(root, |ui| {
        self.navigation(ui, compact_navigation);
        if !compact_navigation {
          let rect = ui.max_rect();
          ui.painter().line_segment(
            [rect.right_top(), rect.right_bottom()],
            Stroke::new(1.0, theme::tokens(ui).border),
          );
        }
      });

    egui::CentralPanel::default()
      .frame(
        Frame::central_panel(root.style())
          .fill(theme::tokens(root).canvas)
          .inner_margin(egui::Margin::same(0)),
      )
      .show(root, |ui| {
        ui.add_space(shell.content_top);
        self.header(ui, shell.page_header_height);
        self.page_container(ui);
      });
    self.window_resize_handles(root);
  }

  fn title_bar(&self, root: &mut Ui, height: f32) {
    let panel = egui::Panel::top("cvr-title-bar")
      .exact_size(height)
      .frame(
        Frame::new()
          .fill(theme::tokens(root).surface)
          .stroke(Stroke::NONE)
          .inner_margin(egui::Margin::symmetric(10, 3)),
      )
      .show(root, |ui| {
        let maximized = ui
          .ctx()
          .input(|input| input.viewport().maximized.unwrap_or(false));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
          if ui
            .add_sized(
              [28.0, 28.0],
              egui::Button::new(RichText::new("×").size(18.0)).frame(false),
            )
            .on_hover_text("关闭")
            .clicked()
          {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
          }
          if ui
            .add_sized(
              [28.0, 28.0],
              egui::Button::new(if maximized { "❐" } else { "□" }).frame(false),
            )
            .on_hover_text(if maximized { "还原" } else { "最大化" })
            .clicked()
          {
            ui.ctx()
              .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
          }
          if ui
            .add_sized([28.0, 28.0], egui::Button::new("−").frame(false))
            .on_hover_text("最小化")
            .clicked()
          {
            ui.ctx()
              .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
          }

          let drag = ui.interact(
            ui.available_rect_before_wrap(),
            ui.id().with("window-drag-region"),
            egui::Sense::click_and_drag(),
          );
          if drag.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
          }
          if drag.double_clicked() {
            ui.ctx()
              .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
          }
        });
      });
    root.painter().line_segment(
      [
        panel.response.rect.left_bottom(),
        panel.response.rect.right_bottom(),
      ],
      Stroke::new(1.0, theme::tokens(root).border),
    );
  }

  fn window_resize_handles(&self, root: &Ui) {
    let maximized = root
      .ctx()
      .input(|input| input.viewport().maximized.unwrap_or(false));
    if maximized {
      return;
    }
    let rect = root.max_rect();
    let edge = 6.0;
    let corner = 12.0;
    let handles = [
      (
        "north",
        egui::Rect::from_min_max(
          egui::pos2(rect.left() + corner, rect.top()),
          egui::pos2(rect.right() - corner, rect.top() + edge),
        ),
        egui::ResizeDirection::North,
        egui::CursorIcon::ResizeNorth,
      ),
      (
        "south",
        egui::Rect::from_min_max(
          egui::pos2(rect.left() + corner, rect.bottom() - edge),
          egui::pos2(rect.right() - corner, rect.bottom()),
        ),
        egui::ResizeDirection::South,
        egui::CursorIcon::ResizeSouth,
      ),
      (
        "west",
        egui::Rect::from_min_max(
          egui::pos2(rect.left(), rect.top() + corner),
          egui::pos2(rect.left() + edge, rect.bottom() - corner),
        ),
        egui::ResizeDirection::West,
        egui::CursorIcon::ResizeWest,
      ),
      (
        "east",
        egui::Rect::from_min_max(
          egui::pos2(rect.right() - edge, rect.top() + corner),
          egui::pos2(rect.right(), rect.bottom() - corner),
        ),
        egui::ResizeDirection::East,
        egui::CursorIcon::ResizeEast,
      ),
      (
        "north-west",
        egui::Rect::from_min_size(rect.left_top(), egui::Vec2::splat(corner)),
        egui::ResizeDirection::NorthWest,
        egui::CursorIcon::ResizeNorthWest,
      ),
      (
        "north-east",
        egui::Rect::from_min_size(
          egui::pos2(rect.right() - corner, rect.top()),
          egui::Vec2::splat(corner),
        ),
        egui::ResizeDirection::NorthEast,
        egui::CursorIcon::ResizeNorthEast,
      ),
      (
        "south-west",
        egui::Rect::from_min_size(
          egui::pos2(rect.left(), rect.bottom() - corner),
          egui::Vec2::splat(corner),
        ),
        egui::ResizeDirection::SouthWest,
        egui::CursorIcon::ResizeSouthWest,
      ),
      (
        "south-east",
        egui::Rect::from_min_size(
          rect.right_bottom() - egui::Vec2::splat(corner),
          egui::Vec2::splat(corner),
        ),
        egui::ResizeDirection::SouthEast,
        egui::CursorIcon::ResizeSouthEast,
      ),
    ];
    for (name, handle, direction, cursor) in handles {
      let response = root
        .interact(
          handle,
          root.id().with(("window-resize", name)),
          egui::Sense::drag(),
        )
        .on_hover_cursor(cursor);
      if response.drag_started() {
        root
          .ctx()
          .send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
      }
    }
  }

  fn page_container(&mut self, ui: &mut Ui) {
    if self.snapshot.page == Page::Proxies {
      ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
          ui.set_min_width(ui.available_width());
          self.page(ui);
        });
      return;
    }
    let full = matches!(
      self.snapshot.page,
      Page::Profiles | Page::Connections | Page::Rules | Page::Logs
    );
    if full {
      ui.scope(|ui| {
        ui.set_min_size(ui.available_size());
        self.page(ui);
      });
    } else {
      ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
          ui.set_min_width(ui.available_width());
          Frame::new()
            .inner_margin(egui::Margin {
              left: geometry::PAGE_CONTENT_HORIZONTAL_MARGIN as i8,
              right: geometry::PAGE_CONTENT_HORIZONTAL_MARGIN as i8,
              top: geometry::PAGE_CONTENT_VERTICAL_PADDING as i8,
              bottom: geometry::PAGE_CONTENT_VERTICAL_PADDING as i8,
            })
            .show(ui, |ui| self.page(ui));
        });
    }
  }

  fn navigation(&mut self, ui: &mut Ui, compact: bool) {
    let tokens = theme::tokens(ui);
    ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
    let logo_height = if compact {
      geometry::NAV_LOGO_COLLAPSED_HEIGHT
    } else {
      geometry::NAV_LOGO_HEIGHT
    };
    ui.allocate_ui_with_layout(
      egui::vec2(ui.available_width(), logo_height),
      Layout::left_to_right(Align::Center),
      |ui| {
        ui.add_space(if compact { 18.0 } else { 20.0 });
        let (logo, _) = ui.allocate_exact_size(egui::Vec2::splat(36.0), egui::Sense::hover());
        ui.painter()
          .rect_filled(logo, geometry::GLOBAL_RADIUS, tokens.accent);
        ui.painter().text(
          logo.center(),
          egui::Align2::CENTER_CENTER,
          "R",
          egui::FontId::proportional(18.0),
          Color32::WHITE,
        );
        if !compact {
          ui.add_space(5.0);
          ui.label(RichText::new("rsclash").size(20.0).strong());
        }
      },
    );

    let traffic_height = if compact {
      0.0
    } else {
      geometry::NAV_TRAFFIC_HEIGHT
    };
    let menu_height = (ui.available_height() - traffic_height - 8.0).max(0.0);
    ScrollArea::vertical()
      .id_salt("cvr-navigation")
      .max_height(menu_height)
      .auto_shrink([false, false])
      .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
      .show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add_space(4.0);
        for page in Page::ALL {
          self.navigation_item(ui, page, compact);
        }
      });

    if !compact {
      self.navigation_traffic(ui);
    }

    let response = ui.interact(
      ui.max_rect(),
      ui.id().with("navigation-context"),
      egui::Sense::hover(),
    );
    response.context_menu(|ui| {
      let label = if compact {
        "展开导航栏"
      } else {
        "折叠导航栏"
      };
      if ui.button(label).clicked() {
        self.navigation_collapsed = !compact;
        ui.close();
      }
    });
  }

  fn navigation_item(&mut self, ui: &mut Ui, page: Page, compact: bool) {
    let selected = self.snapshot.page == page;
    let tokens = theme::tokens(ui);
    let outer_height = if compact {
      geometry::NAV_COLLAPSED_ITEM_SIZE + geometry::NAV_COLLAPSED_ITEM_MARGIN * 2.0
    } else {
      geometry::NAV_ITEM_OUTER_HEIGHT
    };
    let (outer, _) = ui.allocate_exact_size(
      egui::vec2(ui.available_width(), outer_height),
      egui::Sense::hover(),
    );
    let button = if compact {
      egui::Rect::from_center_size(
        outer.center(),
        egui::Vec2::splat(geometry::NAV_COLLAPSED_ITEM_SIZE),
      )
    } else {
      let vertical_margin = (geometry::NAV_ITEM_OUTER_HEIGHT - geometry::NAV_ITEM_HEIGHT) / 2.0;
      egui::Rect::from_min_max(
        outer.min + egui::vec2(geometry::NAV_ITEM_HORIZONTAL_MARGIN, vertical_margin),
        outer.max - egui::vec2(geometry::NAV_ITEM_HORIZONTAL_MARGIN, vertical_margin),
      )
    };
    let response = ui.interact(
      button,
      ui.id().with(("navigation-item", page.label())),
      egui::Sense::click(),
    );
    let fill = if selected {
      tokens.accent_soft
    } else if response.hovered() {
      tokens.surface_raised
    } else {
      Color32::TRANSPARENT
    };
    ui.painter().rect_filled(
      button,
      if compact {
        geometry::NAV_COLLAPSED_ITEM_RADIUS
      } else {
        geometry::NAV_ITEM_RADIUS
      },
      fill,
    );
    let text_color = ui.visuals().text_color();
    if compact {
      paint_navigation_icon(ui.painter(), page, button.center(), text_color);
    } else {
      paint_navigation_icon(
        ui.painter(),
        page,
        egui::pos2(button.left() + 28.0, button.center().y),
        text_color,
      );
      ui.painter().text(
        egui::pos2(button.left() + 110.0, button.center().y),
        egui::Align2::CENTER_CENTER,
        page.label(),
        egui::FontId::proportional(14.0),
        text_color,
      );
    }
    if response.clicked() {
      self.command(UiCommand::Navigate(page));
    }
    if compact {
      response.on_hover_text(page.label());
    }
  }

  fn navigation_traffic(&self, ui: &mut Ui) {
    let tokens = theme::tokens(ui);
    let width = (ui.available_width() - geometry::NAV_TRAFFIC_HORIZONTAL_PADDING * 2.0).max(0.0);
    ui.add_space(6.0);
    ui.allocate_ui_with_layout(
      egui::vec2(width, geometry::NAV_TRAFFIC_GRAPH_HEIGHT),
      Layout::top_down(Align::Min),
      |ui| {
        ui.set_width(width);
        let (response, painter) = ui.allocate_painter(
          egui::vec2(width, geometry::NAV_TRAFFIC_GRAPH_HEIGHT),
          egui::Sense::hover(),
        );
        painter.line_segment(
          [
            egui::pos2(response.rect.left(), response.rect.bottom() - 1.0),
            egui::pos2(response.rect.right(), response.rect.bottom() - 1.0),
          ],
          Stroke::new(1.0, tokens.border),
        );
        paint_navigation_metric(
          &painter,
          response.rect,
          &self.snapshot.mihomo.metrics,
          false,
          tokens.accent,
        );
        paint_navigation_metric(
          &painter,
          response.rect,
          &self.snapshot.mihomo.metrics,
          true,
          tokens.warning,
        );
      },
    );
    ui.add_space(6.0);
    let traffic = &self.snapshot.mihomo.traffic;
    navigation_metric_row(
      ui,
      "↑",
      &format_rate(traffic.upload_bytes_per_second),
      tokens.warning,
    );
    navigation_metric_row(
      ui,
      "↓",
      &format_rate(traffic.download_bytes_per_second),
      tokens.accent,
    );
    navigation_metric_row(
      ui,
      "▣",
      &format_bytes(self.snapshot.mihomo.memory_bytes),
      tokens.text_muted,
    );
  }

  fn header(&mut self, ui: &mut Ui, height: f32) {
    let tokens = theme::tokens(ui);
    let panel = egui::Panel::top("cvr-page-header")
      .exact_size(height)
      .frame(
        Frame::new()
          .fill(tokens.surface)
          .stroke(Stroke::NONE)
          .inner_margin(egui::Margin::symmetric(
            geometry::PAGE_HEADER_HORIZONTAL_PADDING as i8,
            0,
          )),
      )
      .show(ui, |ui| {
        ui.allocate_ui_with_layout(
          egui::vec2(ui.available_width(), height),
          Layout::left_to_right(Align::Center),
          |ui| {
            ui.label(
              RichText::new(self.snapshot.page.label())
                .size(20.0)
                .strong(),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
              self.page_header_actions(ui);
            });
          },
        );
      });
    ui.painter().line_segment(
      [
        panel.response.rect.left_bottom(),
        panel.response.rect.right_bottom(),
      ],
      Stroke::new(1.0, tokens.border),
    );
  }

  fn page_header_actions(&mut self, ui: &mut Ui) {
    match self.snapshot.page {
      Page::Home => {
        if header_icon_button(ui, "⚙", "首页卡片设置").clicked() {
          self.home_settings_dialog = true;
        }
        header_icon_button(ui, "?", "使用说明");
        if header_icon_button(ui, "♧", "轻量模式").clicked() {
          if self.tray_is_available() {
            self.command(UiCommand::SetWindowVisible(false));
          } else {
            self.local_error = Some("当前桌面没有可用的系统托盘。".to_string());
          }
        }
      },
      Page::Proxies => {
        let current = self.snapshot.mihomo.mode.clone();
        if ui
          .add_sized(
            [84.0, 30.0],
            egui::Button::new("链式代理").selected(self.proxy_chain_mode),
          )
          .clicked()
        {
          self.proxy_chain_mode = !self.proxy_chain_mode;
        }
        for (mode, label) in [
          (ProxyMode::Direct, "直连"),
          (ProxyMode::Global, "全局"),
          (ProxyMode::Rule, "规则"),
        ] {
          if ui
            .add_sized(
              [50.0, 30.0],
              egui::Button::new(label).selected(current == mode),
            )
            .clicked()
          {
            self.command(UiCommand::SetProxyMode(mode));
          }
        }
        if ui
          .add_sized([76.0, 30.0], egui::Button::new("代理集合"))
          .clicked()
        {
          self.proxy_provider_dialog = true;
        }
      },
      Page::Profiles => self.profile_header_actions(ui),
      Page::Connections => {
        if ui
          .add_sized([68.0, 30.0], egui::Button::new("关闭全部"))
          .clicked()
        {
          self.command(UiCommand::CloseAllConnections);
        }
        if header_icon_button(
          ui,
          if self.connection_table_layout {
            "☷"
          } else {
            "▦"
          },
          if self.connection_table_layout {
            "列表视图"
          } else {
            "表格视图"
          },
        )
        .clicked()
        {
          self.connection_table_layout = !self.connection_table_layout;
        }
        ui.label(format!(
          "↑ {}  ↓ {}",
          format_bytes(self.snapshot.mihomo.traffic.upload_total),
          format_bytes(self.snapshot.mihomo.traffic.download_total)
        ));
      },
      Page::Rules => {
        if ui
          .add_sized([82.0, 30.0], egui::Button::new("规则集合"))
          .clicked()
        {
          self.rule_editor_dialog = true;
        }
      },
      Page::Logs => {
        if ui
          .add_sized([50.0, 30.0], egui::Button::new("清空"))
          .clicked()
        {
          self.command(UiCommand::ClearLogs);
        }
        if header_icon_button(ui, "⇅", "切换日志顺序").clicked() {
          self.log_reverse = !self.log_reverse;
        }
        let paused = self.snapshot.mihomo.logs_paused;
        if header_icon_button(ui, if paused { "▶" } else { "Ⅱ" }, "暂停或继续").clicked() {
          self.command(UiCommand::SetLogsPaused(!paused));
        }
      },
      Page::Settings => {
        let busy = self.snapshot.settings.busy;
        if ui
          .add_enabled(
            !busy && self.settings_dirty,
            egui::Button::new("保存").min_size(egui::vec2(52.0, 30.0)),
          )
          .clicked()
        {
          self.command(UiCommand::ApplySettings(Box::new(
            self.settings_draft.clone(),
          )));
        }
        if ui
          .add_enabled(
            !busy && self.settings_dirty,
            egui::Button::new("放弃").min_size(egui::vec2(52.0, 30.0)),
          )
          .clicked()
        {
          self.settings_draft = self.snapshot.settings.value.clone();
          self.settings_dirty = false;
        }
        if header_icon_button(ui, "↻", "刷新设置").clicked() {
          self.command(UiCommand::RefreshSettings);
        }
        if busy {
          ui.spinner();
        }
      },
      Page::Unlock => {
        if ui
          .add_sized([92.0, 30.0], egui::Button::new("↻  全部测试"))
          .clicked()
        {
          self.local_error = Some("流媒体解锁检测将在 P11 接入网络探测后可用。".to_string());
        }
      },
    }
  }

  fn profile_header_actions(&mut self, ui: &mut Ui) {
    if self.profile_batch_mode {
      ui.label(format!("已选择 {} 项", self.selected_profiles.len()));
      if ui
        .add_sized([52.0, 30.0], egui::Button::new("完成"))
        .clicked()
      {
        self.profile_batch_mode = false;
        self.selected_profiles.clear();
        self.pending_batch_delete = false;
      }
      let delete_label = if self.pending_batch_delete {
        "确认"
      } else {
        "删除"
      };
      if ui
        .add_enabled(
          !self.selected_profiles.is_empty(),
          egui::Button::new(delete_label).min_size(egui::vec2(52.0, 30.0)),
        )
        .clicked()
      {
        if self.pending_batch_delete {
          self.command(UiCommand::DeleteProfiles {
            uids: self.selected_profiles.iter().cloned().collect(),
          });
          self.profile_batch_mode = false;
          self.selected_profiles.clear();
          self.pending_batch_delete = false;
        } else {
          self.pending_batch_delete = true;
        }
      }
      if header_icon_button(ui, "☑", "全选或取消全选").clicked() {
        if self.selected_profiles.len() == self.snapshot.profiles.items.len() {
          self.selected_profiles.clear();
        } else {
          self.selected_profiles = self
            .snapshot
            .profiles
            .items
            .iter()
            .map(|profile| profile.uid.clone())
            .collect();
        }
      }
      return;
    }
    if header_icon_button(ui, "♨", "重新激活当前配置").clicked()
      && let Some(profile) = self.snapshot.profiles.current()
    {
      self.command(UiCommand::ActivateProfile {
        uid: profile.uid.clone(),
      });
    }
    if header_icon_button(ui, "▤", "查看运行时配置").clicked() {
      self.local_error = Some("运行时配置查看器尚未接入原生编辑器。".to_string());
    }
    if header_icon_button(ui, "↻", "更新全部订阅").clicked() {
      self.command(UiCommand::UpdateAllProfiles);
    }
    if header_icon_button(ui, "☐", "批量管理").clicked() {
      self.profile_batch_mode = true;
      self.selected_profiles.clear();
    }
  }

  fn page(&mut self, ui: &mut Ui) {
    self.error_notices(ui.ctx());

    match self.snapshot.page {
      Page::Home => self.home(ui),
      Page::Proxies => self.proxies(ui),
      Page::Profiles => self.profiles(ui),
      Page::Connections => self.connections(ui),
      Page::Rules => self.rules(ui),
      Page::Logs => self.logs(ui),
      Page::Unlock => self.unlock(ui),
      Page::Settings => self.settings(ui),
    }
  }

  fn error_notices(&mut self, context: &egui::Context) {
    let backend_error = self.snapshot.last_error.clone();
    let local_error = self.local_error.clone();
    if backend_error.is_none() && local_error.is_none() {
      return;
    }
    egui::Area::new(egui::Id::new("cvr-error-notices"))
      .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0))
      .order(egui::Order::Foreground)
      .show(context, |ui| {
        ui.set_max_width(420.0);
        if let Some(error) = backend_error {
          notice_frame(ui, &error.title, &error.detail, || {
            self.command(UiCommand::ClearError);
          });
        }
        if let Some(error) = local_error {
          notice_frame(ui, "操作未完成", &error, || {
            self.local_error = None;
          });
        }
      });
  }

  fn home(&mut self, ui: &mut Ui) {
    self.home_settings_dialog(ui.ctx());
    let core = self.snapshot.core.clone();
    let mihomo = self.snapshot.mihomo.clone();
    let system_proxy = self.snapshot.system_proxy.clone();
    let cards = normalized_home_cards(&self.snapshot.settings.value.home_cards);
    let viewport_width = ui.ctx().content_rect().width();
    let two_columns = geometry::home_grid_columns(geometry::breakpoint(viewport_width)) == 2;

    if cards.iter().any(|card| card == "profile") || cards.iter().any(|card| card == "proxy") {
      if two_columns {
        ui.scope(|ui| {
          ui.spacing_mut().item_spacing.x = geometry::GRID_GAP;
          ui.columns(2, |columns| {
            self.home_profile(&mut columns[0], &core);
            self.home_current_proxy(&mut columns[1], &mihomo);
          });
        });
      } else {
        self.home_profile(ui, &core);
        ui.add_space(geometry::GRID_GAP);
        self.home_current_proxy(ui, &mihomo);
      }
      ui.add_space(geometry::GRID_GAP);
    }

    if cards.iter().any(|card| card == "network") {
      if two_columns {
        ui.scope(|ui| {
          ui.spacing_mut().item_spacing.x = geometry::GRID_GAP;
          ui.columns(2, |columns| {
            self.home_network(&mut columns[0], &core, &mihomo, &system_proxy);
            self.home_proxy(&mut columns[1], &mihomo);
          });
        });
      } else {
        self.home_network(ui, &core, &mihomo, &system_proxy);
        ui.add_space(geometry::GRID_GAP);
        self.home_proxy(ui, &mihomo);
      }
      ui.add_space(geometry::GRID_GAP);
    }

    if cards.iter().any(|card| card == "traffic") {
      self.home_traffic(ui, &mihomo);
    }
  }

  fn home_settings_dialog(&mut self, context: &egui::Context) {
    if !self.home_settings_dialog {
      return;
    }
    let mut open = self.home_settings_dialog;
    let mut cards = self.snapshot.settings.value.home_cards.clone();
    let mut save = false;
    egui::Window::new("首页设置")
      .open(&mut open)
      .default_width(360.0)
      .show(context, |ui| {
        for (key, label) in [
          ("profile", "当前配置"),
          ("proxy", "当前代理"),
          ("network", "网络设置与代理模式"),
          ("traffic", "流量统计"),
        ] {
          setting_membership_checkbox(ui, &mut cards, key, label);
        }
        ui.add_space(geometry::MUI_SPACING);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
          save = ui.button("保存").clicked();
        });
      });
    if save {
      let mut settings = self.snapshot.settings.value.clone();
      settings.home_cards = cards;
      self.command(UiCommand::ApplySettings(Box::new(settings)));
      open = false;
    }
    self.home_settings_dialog = open;
  }

  fn home_profile(&mut self, ui: &mut Ui, core: &CoreState) {
    enhanced_card(ui, "配置", "☷", |ui| {
      if let Some(profile) = self.snapshot.profiles.current() {
        ui.label(RichText::new(&profile.name).size(18.0).strong());
        ui.label(
          RichText::new(match profile.source {
            ProfileSourceKind::Local => "本地配置",
            ProfileSourceKind::Remote => "远程订阅",
            ProfileSourceKind::Merge
            | ProfileSourceKind::Rules
            | ProfileSourceKind::Proxies
            | ProfileSourceKind::Groups
            | ProfileSourceKind::Other => "扩展配置",
          })
          .small()
          .weak(),
        );
      } else {
        ui.label(RichText::new("未选择配置").size(18.0).strong());
        ui.label(RichText::new("请先导入并激活一个订阅").small().weak());
      }
      ui.add_space(geometry::MUI_SPACING);
      ui.separator();
      ui.add_space(geometry::MUI_SPACING);
      self.core_controls(ui, core);
    });
  }

  fn home_current_proxy(&self, ui: &mut Ui, mihomo: &MihomoSnapshot) {
    enhanced_card(ui, "当前代理", "◉", |ui| {
      ui.label(
        RichText::new(mihomo.current_proxy().unwrap_or("尚未选择"))
          .size(18.0)
          .strong(),
      );
      ui.label(
        RichText::new(format!(
          "{} · {}",
          proxy_mode_label(&mihomo.mode),
          mihomo.version.as_deref().unwrap_or("版本未知")
        ))
        .weak(),
      );
      ui.add_space(geometry::MUI_SPACING);
      mihomo_connection_pill(ui, mihomo.connection);
    });
  }

  fn home_proxy(&mut self, ui: &mut Ui, mihomo: &MihomoSnapshot) {
    enhanced_card(ui, "代理模式", "⑂", |ui| {
      ui.vertical_centered(|ui| {
        self.mode_controls(ui, &mihomo.mode);
      });
      ui.add_space(geometry::MUI_SPACING);
      ui.label(
        RichText::new(match mihomo.mode {
          ProxyMode::Rule => "按配置规则决定每个连接使用的策略。",
          ProxyMode::Global => "全部连接使用全局代理组。",
          ProxyMode::Direct => "全部连接直接访问网络。",
          ProxyMode::Unknown(_) => "当前核心返回了未知代理模式。",
        })
        .small()
        .weak(),
      );
      if let Some(error) = mihomo.last_error.as_deref() {
        ui.add_space(geometry::MUI_SPACING);
        ui.label(
          RichText::new(format!("控制器暂时不可用：{error}"))
            .small()
            .color(ui.visuals().warn_fg_color),
        );
      }
    });
  }

  fn home_network(
    &mut self,
    ui: &mut Ui,
    core: &CoreState,
    mihomo: &MihomoSnapshot,
    system_proxy: &SystemProxyView,
  ) {
    let can_enable_system_proxy = system_proxy.available
      && (self.snapshot.settings.value.pac_url.is_some()
        || (mihomo.connection == MihomoConnection::Connected && mihomo.mixed_port.is_some()));
    let (tun_status, _, tun_available) = tun_capability(core, mihomo.tun_enabled);
    enhanced_card(ui, "网络设置", "⌁", |ui| {
      ui.horizontal(|ui| {
        if ui
          .add_enabled(
            !system_proxy.busy && (system_proxy.enabled || can_enable_system_proxy),
            egui::Button::new("系统代理").selected(system_proxy.enabled),
          )
          .clicked()
        {
          self.command(UiCommand::SetSystemProxy(!system_proxy.enabled));
        }
        let enabled = self.snapshot.settings.value.tun_enabled;
        if ui
          .add_enabled(
            tun_available || enabled,
            egui::Button::new("TUN 模式").selected(enabled),
          )
          .clicked()
        {
          let mut settings = self.snapshot.settings.value.clone();
          settings.tun_enabled = !enabled;
          self.command(UiCommand::ApplySettings(Box::new(settings)));
        }
        if system_proxy.busy {
          ui.spinner();
        }
      });
      ui.add_space(geometry::MUI_SPACING);
      let backend = system_proxy
        .backend
        .as_deref()
        .unwrap_or("正在检测 Linux 系统代理后端");
      ui.label(RichText::new(backend).small().weak());
      ui.label(RichText::new(tun_status).small().weak());
      if !system_proxy.available {
        ui.label(
          RichText::new(
            system_proxy
              .detail
              .as_deref()
              .unwrap_or("当前桌面环境不支持系统代理控制"),
          )
          .small()
          .color(ui.visuals().warn_fg_color),
        );
      }
    });
  }

  fn home_traffic(&self, ui: &mut Ui, mihomo: &MihomoSnapshot) {
    enhanced_card(ui, "流量统计", "⇅", |ui| {
      if !mihomo.metrics.is_empty() && self.snapshot.settings.value.traffic_graph {
        ui.allocate_ui_with_layout(
          egui::vec2(ui.available_width(), 130.0),
          Layout::top_down(Align::Min),
          |ui| metric_chart(ui, &mihomo.metrics),
        );
        ui.add_space(geometry::MUI_SPACING);
      }
      let memory = if self.snapshot.settings.value.memory_usage {
        format_bytes(mihomo.memory_bytes)
      } else {
        "已隐藏".to_string()
      };
      ui.columns(3, |columns| {
        stat_pair(
          &mut columns[0],
          "上传",
          &format_rate(mihomo.traffic.upload_bytes_per_second),
          "下载",
          &format_rate(mihomo.traffic.download_bytes_per_second),
        );
        stat_pair(
          &mut columns[1],
          "累计上传",
          &format_bytes(mihomo.traffic.upload_total),
          "累计下载",
          &format_bytes(mihomo.traffic.download_total),
        );
        stat_pair(
          &mut columns[2],
          "内存",
          &memory,
          "连接",
          &mihomo.connection_count.to_string(),
        );
      });
    });
  }

  fn proxies(&mut self, ui: &mut Ui) {
    let mihomo = self.snapshot.mihomo.clone();
    let view = Arc::clone(&mihomo.proxy_view);
    self.proxy_provider_dialog(ui.ctx(), &view, mihomo.proxy_busy);

    if mihomo.connection == MihomoConnection::Offline {
      empty_state(
        ui,
        "Mihomo 尚未运行",
        "启动核心后即可读取代理组并选择节点。",
      );
      return;
    }
    if mihomo.mode == ProxyMode::Direct {
      empty_state(ui, "直连模式", "直连模式不使用代理组。");
      return;
    }
    if view.groups.is_empty() && view.global.is_none() {
      empty_state(
        ui,
        "没有可用代理组",
        "当前配置尚未提供 Selector、URL-Test 等代理组。",
      );
      return;
    }

    if self.proxy_chain_mode {
      Frame::new()
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
          self.proxy_chain_editor(
            ui,
            &view,
            mihomo.proxy_busy || self.snapshot.profiles.busy,
            mihomo.proxy_chain.connected,
          );
        });
      ui.add_space(geometry::TOOLBAR_GAP);
    }

    let regex = if self.proxy_regex && !self.proxy_search.is_empty() {
      let pattern = if self.proxy_whole_word {
        format!("^(?:{})$", self.proxy_search)
      } else {
        self.proxy_search.clone()
      };
      match regex_lite::RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()
      {
        Ok(regex) => Some(regex),
        Err(error) => {
          ui.label(
            RichText::new(format!("正则表达式无效：{error}"))
              .small()
              .color(ui.visuals().error_fg_color),
          );
          None
        },
      }
    } else {
      None
    };

    for group in proxy_groups(&view) {
      self.proxy_group_view(ui, group, &view, regex.as_ref(), mihomo.proxy_busy);
    }
  }

  fn proxy_provider_dialog(&mut self, context: &egui::Context, view: &ProxyViewV1, busy: bool) {
    if !self.proxy_provider_dialog {
      return;
    }
    let mut open = self.proxy_provider_dialog;
    egui::Window::new("代理集合")
      .open(&mut open)
      .default_width(520.0)
      .show(context, |ui| {
        ui.horizontal(|ui| {
          ui.label(format!("{} 个 Provider", view.providers.len()));
          if ui
            .add_enabled(!busy, egui::Button::new("更新全部"))
            .clicked()
          {
            self.command(UiCommand::UpdateAllProxyProviders);
          }
        });
        for provider in &view.providers {
          ui.separator();
          ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&provider.name).strong());
            ui.label(
              RichText::new(format!(
                "{} · {} · {} 个节点",
                provider.kind,
                provider.vehicle_type,
                provider.proxy_record_ids.len()
              ))
              .small()
              .weak(),
            );
            if ui.add_enabled(!busy, egui::Button::new("更新")).clicked() {
              self.command(UiCommand::UpdateProxyProvider {
                name: provider.name.clone(),
              });
            }
            if ui
              .add_enabled(!busy, egui::Button::new("健康检查"))
              .clicked()
            {
              self.command(UiCommand::HealthcheckProxyProvider {
                name: provider.name.clone(),
              });
            }
          });
        }
      });
    self.proxy_provider_dialog = open;
  }

  fn proxy_chain_editor(&mut self, ui: &mut Ui, view: &ProxyViewV1, busy: bool, connected: bool) {
    if self.proxy_chain_group.is_empty()
      && let Some(group) = proxy_groups(view).next()
    {
      self.proxy_chain_group = group.name.clone();
    }
    let mut connect = false;
    let mut disconnect = false;
    let mut action = None;
    let node_count = self.proxy_chain_nodes.len();
    card(ui, "代理链", |ui| {
      ui.label(
        RichText::new(
          "链按入口→出口排列；仅接受 runtime 中可修改的 core 节点，连接和断开都会重新校验并原子部署。",
        )
        .small()
        .weak(),
      );
      ui.horizontal(|ui| {
        ui.label("目标代理组");
        egui::ComboBox::from_id_salt("proxy-chain-group")
          .selected_text(if self.proxy_chain_group.is_empty() {
            "选择代理组"
          } else {
            &self.proxy_chain_group
          })
          .show_ui(ui, |ui| {
            for group in proxy_groups(view) {
              ui.selectable_value(&mut self.proxy_chain_group, group.name.clone(), &group.name);
            }
          });
        if connected {
          ui.label(
            RichText::new("已连接")
              .strong()
              .color(Color32::from_rgb(38, 162, 105)),
          );
        }
      });
      ui.add_space(6.0);
      if self.proxy_chain_nodes.is_empty() {
        ui.label(RichText::new("从下方代理组成员中加入至少两个 core 节点。").weak());
      }
      for (index, node) in self.proxy_chain_nodes.iter().enumerate() {
        let (_, dropped) = ui.dnd_drop_zone::<ProxyChainDrag, _>(
          Frame::group(ui.style()).inner_margin(egui::Margin::symmetric(8, 5)),
          |ui| {
            ui.horizontal(|ui| {
              ui.dnd_drag_source(
                egui::Id::new(("proxy-chain-drag", index)),
                ProxyChainDrag(index),
                |ui| {
                  ui.label("⠿");
                },
              );
              ui.label(if index == 0 {
                format!("入口 · {node}")
              } else if index + 1 == node_count {
                format!("出口 · {node}")
              } else {
                format!("跳点 {} · {node}", index)
              });
              ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                  .add_enabled(!busy && !connected, egui::Button::new("删除"))
                  .clicked()
                {
                  action = Some(ProxyChainAction::Remove(index));
                }
                if ui
                  .add_enabled(
                    !busy && !connected && index + 1 < node_count,
                    egui::Button::new("↓"),
                  )
                  .clicked()
                {
                  action = Some(ProxyChainAction::MoveDown(index));
                }
                if ui
                  .add_enabled(!busy && !connected && index > 0, egui::Button::new("↑"))
                  .clicked()
                {
                  action = Some(ProxyChainAction::MoveUp(index));
                }
              });
            });
          },
        );
        if let Some(payload) = dropped {
          action = Some(ProxyChainAction::Drop {
            from: payload.0,
            to: index,
          });
        }
      }
      ui.horizontal(|ui| {
        if connected {
          disconnect = ui
            .add_enabled(!busy, egui::Button::new("断开并恢复原配置"))
            .clicked();
        } else {
          connect = ui
            .add_enabled(
              !busy && self.proxy_chain_nodes.len() >= 2 && !self.proxy_chain_group.is_empty(),
              egui::Button::new("连接代理链"),
            )
            .clicked();
        }
        ui.label(
          RichText::new(format!("{} 个节点", self.proxy_chain_nodes.len()))
            .small()
            .weak(),
        );
        if busy {
          ui.spinner();
        }
      });
    });
    match action {
      Some(ProxyChainAction::MoveUp(index)) => {
        self.proxy_chain_nodes.swap(index, index - 1);
      },
      Some(ProxyChainAction::Drop { from, to }) => {
        if from < node_count && to < node_count {
          self.proxy_chain_nodes.swap(from, to);
        }
      },
      Some(ProxyChainAction::MoveDown(index)) => {
        self.proxy_chain_nodes.swap(index, index + 1);
      },
      Some(ProxyChainAction::Remove(index)) => {
        self.proxy_chain_nodes.remove(index);
      },
      None => {},
    }
    if connect {
      self.command(UiCommand::SetProxyChain {
        group: self.proxy_chain_group.clone(),
        nodes: self.proxy_chain_nodes.clone(),
      });
    } else if disconnect {
      let group = self
        .snapshot
        .mihomo
        .proxy_chain
        .group
        .clone()
        .unwrap_or_else(|| self.proxy_chain_group.clone());
      self.command(UiCommand::SetProxyChain {
        group,
        nodes: Vec::new(),
      });
    }
  }

  fn proxy_group_view(
    &mut self,
    ui: &mut Ui,
    group: &ProxyGroupView,
    view: &ProxyViewV1,
    regex: Option<&regex_lite::Regex>,
    busy: bool,
  ) {
    let expanded = self.expanded_proxy_groups.contains(&group.name);
    let (outer, _) =
      ui.allocate_exact_size(egui::vec2(ui.available_width(), 76.0), egui::Sense::hover());
    let header = outer.shrink2(egui::vec2(8.0, 4.0));
    let tools_width = 250.0_f32.min((header.width() - 48.0).max(0.0));
    let tools = egui::Rect::from_min_max(
      egui::pos2(header.right() - tools_width, header.top()),
      header.right_bottom(),
    );
    let toggle_rect =
      egui::Rect::from_min_max(header.min, egui::pos2(tools.left() - 4.0, header.bottom()));
    let toggle = ui.interact(
      toggle_rect,
      ui.id().with(("proxy-group", &group.name)),
      egui::Sense::click(),
    );
    let tokens = theme::tokens(ui);
    ui.painter()
      .rect_filled(header, geometry::GLOBAL_RADIUS, tokens.surface);
    if toggle.hovered() {
      ui.painter()
        .rect_filled(header, geometry::GLOBAL_RADIUS, tokens.surface_raised);
    }
    ui.painter().text(
      egui::pos2(header.left() + 16.0, header.center().y - 11.0),
      egui::Align2::LEFT_CENTER,
      &group.name,
      egui::FontId::proportional(16.0),
      ui.visuals().text_color(),
    );
    ui.painter().text(
      egui::pos2(header.left() + 16.0, header.center().y + 12.0),
      egui::Align2::LEFT_CENTER,
      format!(
        "{}    {}",
        group.kind,
        group.selected.as_deref().unwrap_or("未选择")
      ),
      egui::FontId::proportional(13.0),
      tokens.text_muted,
    );

    let mut tool_x = header.right() - 244.0;
    let mut tool_response = |name: &'static str, symbol: &'static str, tooltip: &'static str| {
      let rect = egui::Rect::from_min_size(
        egui::pos2(tool_x, header.center().y - 15.0),
        egui::Vec2::splat(30.0),
      );
      tool_x += 34.0;
      proxy_tool_button(ui, rect, (&group.name, name), symbol, tooltip)
    };
    let locate = tool_response("locate", "⌾", "定位当前代理");
    let test = tool_response("test", "⌁", "测速本组");
    let sort = tool_response(
      "sort",
      match self.proxy_sort {
        ProxySort::Configuration => "⇅",
        ProxySort::Name => "A",
        ProxySort::Delay => "◷",
      },
      "切换排序",
    );
    let _url = tool_response("url", "◉", "延迟测试地址");
    let detail = tool_response(
      "detail",
      if self.proxy_detailed { "◉" } else { "○" },
      "显示代理详情",
    );
    let filter = tool_response(
      "filter",
      if self.proxy_filter_visible {
        "▼"
      } else {
        "▽"
      },
      "过滤代理",
    );
    let count_rect = egui::Rect::from_center_size(
      egui::pos2(header.right() - 54.0, header.center().y),
      egui::vec2(42.0, 24.0),
    );
    ui.painter()
      .rect_filled(count_rect, 12.0, tokens.accent_soft);
    ui.painter().text(
      count_rect.center(),
      egui::Align2::CENTER_CENTER,
      group.members.len().to_string(),
      egui::FontId::proportional(12.0),
      tokens.accent,
    );
    ui.painter().text(
      egui::pos2(header.right() - 18.0, header.center().y),
      egui::Align2::CENTER_CENTER,
      if expanded { "⌃" } else { "⌄" },
      egui::FontId::proportional(20.0),
      ui.visuals().text_color(),
    );
    if toggle.clicked() {
      if expanded {
        self.expanded_proxy_groups.remove(&group.name);
      } else {
        self.expanded_proxy_groups.insert(group.name.clone());
      }
    }
    if locate.clicked() {
      self.expanded_proxy_groups.insert(group.name.clone());
      if let Some(selected) = group.selected.as_ref() {
        self.locate_proxy = Some((group.name.clone(), selected.clone()));
      }
    }
    if test.clicked() && !busy {
      self.expanded_proxy_groups.insert(group.name.clone());
      self.command(UiCommand::TestProxyGroup {
        name: group.name.clone(),
      });
    }
    if sort.clicked() {
      self.expanded_proxy_groups.insert(group.name.clone());
      self.proxy_sort = match self.proxy_sort {
        ProxySort::Configuration => ProxySort::Delay,
        ProxySort::Delay => ProxySort::Name,
        ProxySort::Name => ProxySort::Configuration,
      };
    }
    if detail.clicked() {
      self.expanded_proxy_groups.insert(group.name.clone());
      self.proxy_detailed = !self.proxy_detailed;
    }
    if filter.clicked() {
      self.expanded_proxy_groups.insert(group.name.clone());
      self.proxy_filter_visible = !self.proxy_filter_visible;
    }
    if !expanded {
      return;
    }

    ui.allocate_ui_with_layout(
      egui::vec2(ui.available_width(), geometry::PROFILE_CONTENT_OFFSET),
      Layout::left_to_right(Align::Center),
      |ui| {
        ui.add_space(16.0);
        if self.proxy_filter_visible {
          let filter_width = (ui.available_width() - 236.0).max(72.0);
          ui.add_sized(
            [filter_width, 36.0],
            egui::TextEdit::singleline(&mut self.proxy_search).hint_text("过滤"),
          );
        }
        if header_icon_button(ui, "⌾", "定位当前代理").clicked()
          && let Some(selected) = group.selected.as_ref()
        {
          self.locate_proxy = Some((group.name.clone(), selected.clone()));
        }
        if header_icon_button(ui, "⌁", "测速本组").clicked() && !busy {
          self.command(UiCommand::TestProxyGroup {
            name: group.name.clone(),
          });
        }
        if header_icon_button(
          ui,
          match self.proxy_sort {
            ProxySort::Configuration => "⇅",
            ProxySort::Name => "A",
            ProxySort::Delay => "◷",
          },
          "切换排序",
        )
        .clicked()
        {
          self.proxy_sort = match self.proxy_sort {
            ProxySort::Configuration => ProxySort::Delay,
            ProxySort::Delay => ProxySort::Name,
            ProxySort::Name => ProxySort::Configuration,
          };
        }
        header_icon_button(ui, "◉", "延迟测试地址");
        if header_icon_button(
          ui,
          if self.proxy_detailed { "◉" } else { "○" },
          "显示代理详情",
        )
        .clicked()
        {
          self.proxy_detailed = !self.proxy_detailed;
        }
        if header_icon_button(
          ui,
          if self.proxy_filter_visible {
            "▼"
          } else {
            "▽"
          },
          "过滤代理",
        )
        .clicked()
        {
          self.proxy_filter_visible = !self.proxy_filter_visible;
        }
      },
    );

    let mut items = group
      .members
      .iter()
      .map(|member| proxy_display_item(member, view))
      .filter(|item| {
        proxy_item_matches(
          item,
          &self.proxy_search,
          regex,
          self.proxy_regex,
          self.proxy_whole_word,
        )
      })
      .collect::<Vec<_>>();
    sort_proxy_items(&mut items, self.proxy_sort);
    if items.is_empty() {
      empty_state(ui, "没有代理", "本组没有符合过滤条件的节点。");
      return;
    }
    let viewport_width = ui.ctx().content_rect().width();
    let columns = geometry::proxy_grid_columns(
      viewport_width,
      self.snapshot.settings.value.proxy_layout_columns,
    )
    .min(items.len())
    .max(1);
    for row in items.chunks(columns) {
      ui.columns(columns, |column_uis| {
        for (column, item) in column_uis.iter_mut().zip(row) {
          self.proxy_item_row(column, group, item, busy);
        }
      });
      ui.add_space(8.0);
    }
  }

  fn proxy_item_row(
    &mut self,
    ui: &mut Ui,
    group: &ProxyGroupView,
    item: &ProxyDisplayItem,
    busy: bool,
  ) {
    let selected = group.selected.as_deref() == Some(item.name.as_str());
    let (rect, response) =
      ui.allocate_exact_size(egui::vec2(ui.available_width(), 56.0), egui::Sense::click());
    let tokens = theme::tokens(ui);
    ui.painter().rect_filled(
      rect,
      6.0,
      if selected {
        tokens.accent_soft
      } else if response.hovered() {
        tokens.surface_raised
      } else {
        tokens.surface
      },
    );
    if selected {
      ui.painter().rect_filled(
        egui::Rect::from_min_max(rect.min, egui::pos2(rect.left() + 3.0, rect.bottom())),
        6.0,
        tokens.accent,
      );
    }
    ui.painter().text(
      egui::pos2(rect.left() + 12.0, rect.center().y - 9.0),
      egui::Align2::LEFT_CENTER,
      &item.name,
      egui::FontId::proportional(14.0),
      ui.visuals().text_color(),
    );
    paint_proxy_tags(ui, rect, item, self.proxy_detailed);
    ui.painter().text(
      egui::pos2(rect.right() - 12.0, rect.center().y),
      egui::Align2::RIGHT_CENTER,
      item
        .delay_ms
        .map_or_else(|| "—".to_string(), |delay| format!("{delay} ms")),
      egui::FontId::proportional(12.0),
      proxy_delay_color(ui, item.delay_ms, item.alive),
    );
    if response.clicked() && !busy && item.unresolved.is_none() && (item.alive || selected) {
      self.command(UiCommand::SelectProxy {
        group: group.name.clone(),
        proxy: item.name.clone(),
      });
    }
    if self.locate_proxy.as_ref() == Some(&(group.name.clone(), item.name.clone())) {
      response.scroll_to_me(Some(Align::Center));
      self.locate_proxy = None;
    }
    response.context_menu(|ui| {
      if let Some(record_id) = item.record_id.as_ref()
        && ui.add_enabled(!busy, egui::Button::new("测速")).clicked()
      {
        self.command(UiCommand::TestProxy {
          record_id: record_id.clone(),
        });
        ui.close();
      }
      if item.chain_eligible
        && ui
          .add_enabled(
            !busy && !self.snapshot.mihomo.proxy_chain.connected,
            egui::Button::new("加入代理链"),
          )
          .clicked()
      {
        if self.proxy_chain_nodes.contains(&item.name) {
          self.local_error = Some(format!("代理链中已经包含节点 {}。", item.name));
        } else {
          self.proxy_chain_nodes.push(item.name.clone());
        }
        ui.close();
      }
      if let Some(reason) = item.unresolved {
        ui.label(
          RichText::new(proxy_unresolved_label(reason))
            .small()
            .color(ui.visuals().warn_fg_color),
        );
      }
    });
  }

  fn rules(&mut self, ui: &mut Ui) {
    let mihomo = self.snapshot.mihomo.clone();
    self.rule_tools_dialog(ui.ctx(), &mihomo);
    ui.allocate_ui_with_layout(
      egui::vec2(ui.available_width(), geometry::PROFILE_CONTENT_OFFSET),
      Layout::left_to_right(Align::Center),
      |ui| {
        ui.add_space(geometry::PAGE_CONTENT_HORIZONTAL_MARGIN);
        ui.add_sized(
          [
            (ui.available_width() - geometry::PAGE_CONTENT_HORIZONTAL_MARGIN).max(120.0),
            geometry::RULE_TOOLBAR_HEIGHT,
          ],
          egui::TextEdit::singleline(&mut self.rule_search).hint_text("搜索"),
        );
      },
    );

    let query = self.rule_search.trim().to_ascii_lowercase();
    let filtered = mihomo
      .rules
      .iter()
      .filter(|rule| rule_matches(rule, &query))
      .collect::<Vec<_>>();
    if filtered.is_empty() {
      empty_state(ui, "没有规则", "当前配置没有符合搜索条件的规则。");
      return;
    }

    ScrollArea::vertical()
      .id_salt("cvr-rules-list")
      .auto_shrink([false, false])
      .show_rows(ui, geometry::RULE_ROW_HEIGHT, filtered.len(), |ui, rows| {
        for rule in &filtered[rows] {
          let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), geometry::RULE_ROW_HEIGHT),
            egui::Sense::click(),
          );
          if response.hovered() {
            ui.painter()
              .rect_filled(rect, 0.0, theme::tokens(ui).surface_raised);
          }
          ui.painter().text(
            egui::pos2(rect.left() + 31.0, rect.center().y),
            egui::Align2::CENTER_CENTER,
            format!("{}", rule.index + 1),
            egui::FontId::proportional(13.0),
            theme::tokens(ui).text_muted,
          );
          ui.painter().text(
            egui::pos2(rect.left() + 64.0, rect.center().y - 8.0),
            egui::Align2::LEFT_CENTER,
            if rule.payload.is_empty() {
              "-"
            } else {
              &rule.payload
            },
            egui::FontId::proportional(14.0),
            ui.visuals().text_color(),
          );
          ui.painter().text(
            egui::pos2(rect.left() + 64.0, rect.center().y + 9.0),
            egui::Align2::LEFT_CENTER,
            format!("{}    {}", rule.kind, rule.proxy),
            egui::FontId::proportional(12.0),
            theme::tokens(ui).text_muted,
          );
          ui.painter().line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            Stroke::new(1.0, theme::tokens(ui).border),
          );
        }
      });
  }

  fn rule_tools_dialog(&mut self, context: &egui::Context, mihomo: &MihomoSnapshot) {
    if !self.rule_editor_dialog {
      return;
    }
    let mut open = self.rule_editor_dialog;
    egui::Window::new("规则集合")
      .open(&mut open)
      .default_width(560.0)
      .show(context, |ui| {
        ui.horizontal(|ui| {
          ui.label(format!("当前加载 {} 条规则", mihomo.rules.len()));
          if ui.button("刷新").clicked() {
            self.command(UiCommand::RefreshMihomo);
          }
        });
        if !mihomo.rule_providers.is_empty() {
          ui.add_space(geometry::GRID_GAP);
          ui.label(RichText::new("Rule Providers").strong());
          for provider in mihomo.rule_providers.iter() {
            ui.horizontal_wrapped(|ui| {
              ui.label(&provider.name);
              ui.label(
                RichText::new(format!(
                  "{} · {} · {} 条",
                  provider.behavior, provider.format, provider.rule_count
                ))
                .small()
                .weak(),
              );
              if ui.button("更新").clicked() {
                self.command(UiCommand::UpdateRuleProvider {
                  name: provider.name.clone(),
                });
              }
            });
          }
        }
        ui.add_space(geometry::GRID_GAP);
        ui.separator();
        ui.add_space(geometry::GRID_GAP);
        ui.label(RichText::new("规则生成器").strong());
        ui.horizontal_wrapped(|ui| {
          egui::ComboBox::from_id_salt("rule-draft-kind")
            .selected_text(&self.rule_draft.kind)
            .show_ui(ui, |ui| {
              for kind in RULE_KINDS {
                ui.selectable_value(&mut self.rule_draft.kind, kind.to_string(), *kind);
              }
            });
          if self.rule_draft.kind != "MATCH" {
            ui.add(
              egui::TextEdit::singleline(&mut self.rule_draft.payload)
                .hint_text(rule_payload_hint(&self.rule_draft.kind)),
            );
          }
          ui.add(egui::TextEdit::singleline(&mut self.rule_draft.target).hint_text("目标策略"));
          if rule_supports_no_resolve(&self.rule_draft.kind) {
            ui.checkbox(&mut self.rule_draft.no_resolve, "no-resolve");
          }
        });
        if ui.button("加入当前配置的规则扩展").clicked() {
          match build_rule_draft(&self.rule_draft) {
            Ok(rule) => {
              let target = self.snapshot.profiles.current().and_then(|profile| {
                profile
                  .enhancements
                  .rules
                  .as_ref()
                  .map(|uid| (uid.clone(), format!("{} · 规则", profile.name)))
              });
              if let Some((uid, name)) = target {
                self.pending_rule_append = Some(rule);
                self.open_sequence_editor(uid, name, SequenceEditorKind::Rules);
                self.command(UiCommand::Navigate(Page::Profiles));
              } else {
                self.local_error = Some("当前没有可编辑的活动 profile。".to_string());
              }
            },
            Err(error) => self.local_error = Some(error),
          }
        }
      });
    self.rule_editor_dialog = open;
  }

  fn connections(&mut self, ui: &mut Ui) {
    let mihomo = self.snapshot.mihomo.clone();
    let source = if self.show_closed_connections {
      mihomo.closed_connections.as_ref()
    } else {
      mihomo.connections.as_ref()
    };
    let active_count = mihomo.connections.len();
    let closed_count = mihomo.closed_connections.len();

    ui.allocate_ui_with_layout(
      egui::vec2(ui.available_width(), geometry::PROFILE_CONTENT_OFFSET),
      Layout::left_to_right(Align::Center),
      |ui| {
        ui.add_space(geometry::PAGE_CONTENT_HORIZONTAL_MARGIN);
        if ui
          .add_sized(
            [72.0, 30.0],
            egui::Button::new(format!("活动 {active_count}"))
              .selected(!self.show_closed_connections),
          )
          .clicked()
        {
          self.show_closed_connections = false;
        }
        if ui
          .add_sized(
            [72.0, 30.0],
            egui::Button::new(format!("已关闭 {closed_count}"))
              .selected(self.show_closed_connections),
          )
          .clicked()
        {
          self.show_closed_connections = true;
        }
        if !self.connection_table_layout {
          egui::ComboBox::from_id_salt("connection-order")
            .width(90.0)
            .selected_text(match self.connection_sort {
              ConnectionSort::Traffic => "默认排序",
              ConnectionSort::Destination => "目标",
              ConnectionSort::Process => "进程",
              ConnectionSort::Started => "时间",
            })
            .show_ui(ui, |ui| {
              ui.selectable_value(
                &mut self.connection_sort,
                ConnectionSort::Traffic,
                "默认排序",
              );
              ui.selectable_value(
                &mut self.connection_sort,
                ConnectionSort::Destination,
                "目标",
              );
              ui.selectable_value(&mut self.connection_sort, ConnectionSort::Process, "进程");
              ui.selectable_value(&mut self.connection_sort, ConnectionSort::Started, "时间");
            });
        }
        ui.add_sized(
          [
            (ui.available_width() - geometry::PAGE_CONTENT_HORIZONTAL_MARGIN).max(40.0),
            geometry::CONNECTION_TOOLBAR_MIN_HEIGHT,
          ],
          egui::TextEdit::singleline(&mut self.connection_search).hint_text("搜索"),
        );
      },
    );

    let query = self.connection_search.trim().to_ascii_lowercase();
    let mut connections = source
      .iter()
      .filter(|connection| connection_matches(connection, &query))
      .collect::<Vec<_>>();
    sort_connections(&mut connections, self.connection_sort);
    if connections.is_empty() {
      empty_state(ui, "没有连接", "当前没有符合条件的连接。");
      return;
    }

    if self.connection_table_layout {
      self.connection_table(ui, &connections, self.show_closed_connections);
    } else {
      ScrollArea::vertical()
        .id_salt("cvr-connections-list")
        .auto_shrink([false, false])
        .show_rows(
          ui,
          geometry::CONNECTION_ROW_HEIGHT,
          connections.len(),
          |ui, rows| {
            for connection in &connections[rows] {
              self.connection_row(ui, connection, self.show_closed_connections);
            }
          },
        );
    }
    if self.show_closed_connections && !connections.is_empty() {
      egui::Area::new(egui::Id::new("clear-closed-connections"))
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0))
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
          if ui
            .add_sized([94.0, 40.0], egui::Button::new("⌫  清空").corner_radius(20))
            .clicked()
          {
            self.command(UiCommand::ClearClosedConnections);
          }
        });
    }
    if let Some(id) = self.selected_connection.as_deref()
      && let Some(connection) = source.iter().find(|connection| connection.id == id)
    {
      let mut open = true;
      egui::Window::new("连接详情")
        .open(&mut open)
        .default_width(460.0)
        .show(ui.ctx(), |ui| connection_detail(ui, connection));
      if !open {
        self.selected_connection = None;
      }
    }
  }

  fn connection_table(&mut self, ui: &mut Ui, connections: &[&ConnectionSnapshot], closed: bool) {
    let (header, _) =
      ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::hover());
    let tokens = theme::tokens(ui);
    ui.painter().rect_filled(header, 0.0, tokens.surface);
    let columns = connection_table_columns(header);
    for (rect, label) in columns
      .iter()
      .zip(["目标", "网络", "进程", "代理链", "流量", ""])
    {
      ui.painter().text(
        egui::pos2(rect.left() + 8.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(12.0),
        tokens.text_muted,
      );
    }
    ui.painter().line_segment(
      [header.left_bottom(), header.right_bottom()],
      Stroke::new(1.0, tokens.border),
    );
    ScrollArea::vertical()
      .id_salt("cvr-connections-table")
      .auto_shrink([false, false])
      .show_rows(
        ui,
        geometry::CONNECTION_ROW_HEIGHT,
        connections.len(),
        |ui, rows| {
          for connection in &connections[rows] {
            self.connection_table_row(ui, connection, closed);
          }
        },
      );
  }

  fn connection_table_row(&mut self, ui: &mut Ui, connection: &ConnectionSnapshot, closed: bool) {
    let (rect, response) = ui.allocate_exact_size(
      egui::vec2(ui.available_width(), geometry::CONNECTION_ROW_HEIGHT),
      egui::Sense::click(),
    );
    let tokens = theme::tokens(ui);
    if response.hovered() {
      ui.painter().rect_filled(rect, 0.0, tokens.surface_raised);
    }
    if response.clicked() {
      self.selected_connection = Some(connection.id.clone());
    }
    let values = [
      if connection.host.is_empty() {
        connection.destination.as_str()
      } else {
        connection.host.as_str()
      },
      connection.network.as_str(),
      connection.process.as_str(),
      connection.chains.last().map_or("", String::as_str),
      "",
      "",
    ];
    let columns = connection_table_columns(rect);
    for (column, value) in columns.iter().zip(values) {
      let painter = ui
        .painter()
        .with_clip_rect(column.shrink2(egui::vec2(8.0, 0.0)));
      painter.text(
        egui::pos2(column.left() + 8.0, column.center().y),
        egui::Align2::LEFT_CENTER,
        value,
        egui::FontId::proportional(12.0),
        ui.visuals().text_color(),
      );
    }
    ui.painter().text(
      egui::pos2(columns[4].left() + 8.0, columns[4].center().y),
      egui::Align2::LEFT_CENTER,
      format!(
        "{} / {}",
        format_bytes(connection.upload),
        format_bytes(connection.download)
      ),
      egui::FontId::proportional(11.0),
      tokens.text_muted,
    );
    if !closed {
      let close_rect = columns[5].shrink(10.0);
      let close = ui.interact(
        close_rect,
        ui.id().with(("table-close-connection", &connection.id)),
        egui::Sense::click(),
      );
      ui.painter().text(
        close_rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::proportional(18.0),
        ui.visuals().text_color(),
      );
      if close.clicked() {
        self.command(UiCommand::CloseConnection {
          id: connection.id.clone(),
        });
      }
    }
    ui.painter().line_segment(
      [rect.left_bottom(), rect.right_bottom()],
      Stroke::new(1.0, tokens.border),
    );
  }

  fn connection_row(&mut self, ui: &mut Ui, connection: &ConnectionSnapshot, closed: bool) {
    let (rect, _) = ui.allocate_exact_size(
      egui::vec2(ui.available_width(), geometry::CONNECTION_ROW_HEIGHT),
      egui::Sense::hover(),
    );
    let content_rect =
      egui::Rect::from_min_max(rect.min, egui::pos2(rect.right() - 48.0, rect.bottom()));
    let response = ui.interact(
      content_rect,
      ui.id().with(("connection-row", &connection.id)),
      egui::Sense::click(),
    );
    if response.hovered() {
      ui.painter()
        .rect_filled(rect, 0.0, theme::tokens(ui).surface_raised);
    }
    if response.clicked() {
      self.selected_connection = Some(connection.id.clone());
    }
    ui.painter().text(
      egui::pos2(rect.left() + 12.0, rect.center().y - 9.0),
      egui::Align2::LEFT_CENTER,
      if connection.host.is_empty() {
        &connection.destination
      } else {
        &connection.host
      },
      egui::FontId::proportional(14.0),
      ui.visuals().text_color(),
    );
    let mut tags = vec![connection.network.clone()];
    if self.connection_show_process && !connection.process.is_empty() {
      tags.push(connection.process.clone());
    }
    if self.connection_show_rule && !connection.rule.is_empty() {
      tags.push(format!("{} {}", connection.rule, connection.rule_payload));
    }
    if self.connection_show_chains && !connection.chains.is_empty() {
      tags.push(
        connection
          .chains
          .iter()
          .rev()
          .cloned()
          .collect::<Vec<_>>()
          .join(" → "),
      );
    }
    tags.push(format!(
      "↑ {} / ↓ {}",
      format_bytes(connection.upload),
      format_bytes(connection.download)
    ));
    paint_tag_row(
      ui,
      egui::Rect::from_min_max(
        egui::pos2(rect.left() + 12.0, rect.center().y + 2.0),
        egui::pos2(rect.right() - 48.0, rect.bottom() - 4.0),
      ),
      &tags,
    );
    if !closed {
      let close_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 24.0, rect.center().y),
        egui::vec2(32.0, 32.0),
      );
      let close = ui.interact(
        close_rect,
        ui.id().with(("close-connection", &connection.id)),
        egui::Sense::click(),
      );
      if close.clicked() {
        self.command(UiCommand::CloseConnection {
          id: connection.id.clone(),
        });
      }
      ui.painter().text(
        close_rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::proportional(18.0),
        ui.visuals().text_color(),
      );
    }
    ui.painter().line_segment(
      [rect.left_bottom(), rect.right_bottom()],
      Stroke::new(1.0, theme::tokens(ui).border),
    );
  }

  fn logs(&mut self, ui: &mut Ui) {
    let mihomo = self.snapshot.mihomo.clone();
    ui.allocate_ui_with_layout(
      egui::vec2(ui.available_width(), geometry::PROFILE_CONTENT_OFFSET),
      Layout::left_to_right(Align::Center),
      |ui| {
        ui.add_space(geometry::PAGE_CONTENT_HORIZONTAL_MARGIN);
        egui::ComboBox::from_id_salt("log-level")
          .width(88.0)
          .selected_text(stream_log_level_label(self.log_level))
          .show_ui(ui, |ui| {
            for level in [
              StreamLogLevel::Debug,
              StreamLogLevel::Info,
              StreamLogLevel::Warning,
              StreamLogLevel::Error,
              StreamLogLevel::Silent,
            ] {
              if ui
                .selectable_value(&mut self.log_level, level, stream_log_level_label(level))
                .changed()
              {
                self.command(UiCommand::SetLogLevel(level));
              }
            }
          });
        ui.add_sized(
          [
            (ui.available_width() - geometry::PAGE_CONTENT_HORIZONTAL_MARGIN).max(120.0),
            geometry::LOG_TOOLBAR_HEIGHT,
          ],
          egui::TextEdit::singleline(&mut self.log_search).hint_text("搜索"),
        );
      },
    );

    let query = self.log_search.trim().to_ascii_lowercase();
    let filtered = mihomo
      .logs
      .iter()
      .filter(|log| log_matches(log, &query))
      .collect::<Vec<_>>();
    if filtered.is_empty() {
      empty_state(ui, "没有日志", "Mihomo 尚未产生符合条件的日志。");
      return;
    }
    ScrollArea::vertical()
      .id_salt("cvr-logs-list")
      .stick_to_bottom(!self.log_reverse && query.is_empty())
      .auto_shrink([false, false])
      .show_rows(ui, geometry::LOG_ROW_HEIGHT, filtered.len(), |ui, rows| {
        for visual_index in rows {
          let index = if self.log_reverse {
            filtered.len() - 1 - visual_index
          } else {
            visual_index
          };
          log_row(ui, filtered[index]);
        }
      });
  }

  fn profiles(&mut self, ui: &mut Ui) {
    let profiles = self.snapshot.profiles.clone();
    self.profile_import_toolbar(ui, profiles.busy);
    self.profile_create_dialog(ui.ctx(), profiles.busy);
    self.profile_metadata_dialogs(ui.ctx(), profiles.busy);

    ScrollArea::vertical()
      .id_salt("cvr-profiles-content")
      .auto_shrink([false, false])
      .show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        Frame::new()
          .inner_margin(egui::Margin {
            left: geometry::PAGE_CONTENT_HORIZONTAL_MARGIN as i8,
            right: geometry::PAGE_CONTENT_HORIZONTAL_MARGIN as i8,
            top: 0,
            bottom: geometry::PAGE_CONTENT_VERTICAL_PADDING as i8,
          })
          .show(ui, |ui| {
            if self.profile_editor.is_some() {
              self.profile_yaml_editor(ui, profiles.busy);
              ui.add_space(geometry::GRID_GAP);
            }
            if self.sequence_editor.is_some() {
              self.profile_sequence_editor(ui, profiles.busy);
              ui.add_space(geometry::GRID_GAP);
            }
            if self.profile_qr.is_some() {
              self.profile_qr_viewer(ui);
              ui.add_space(geometry::GRID_GAP);
            }

            if profiles.items.is_empty() {
              empty_state(
                ui,
                "还没有配置",
                "从本地文件或 HTTPS 订阅导入第一个 Mihomo 配置。",
              );
              return;
            }

            let viewport_width = ui.ctx().content_rect().width();
            let column_count =
              geometry::profile_grid_columns(geometry::breakpoint(viewport_width)).max(1);
            for (row_index, row) in profiles.items.chunks(column_count).enumerate() {
              ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = geometry::MUI_SPACING;
                ui.columns(column_count, |columns| {
                  for (offset, (column, profile)) in columns.iter_mut().zip(row).enumerate() {
                    let profile_index = row_index * column_count + offset;
                    self.profile_card(
                      column,
                      profile,
                      profile_index,
                      profiles.items.len(),
                      profiles.busy,
                    );
                  }
                });
              });
              ui.add_space(geometry::MUI_SPACING);
            }

            ui.add_space(4.0);
            let divider_width = (ui.available_width() - 32.0).max(0.0);
            let (divider, _) =
              ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
            ui.painter().line_segment(
              [
                egui::pos2(divider.center().x - divider_width / 2.0, divider.center().y),
                egui::pos2(divider.center().x + divider_width / 2.0, divider.center().y),
              ],
              Stroke::new(1.0, theme::tokens(ui).border),
            );
            ui.add_space(geometry::GRID_GAP);
            self.profile_extension_cards(ui, profiles.current());
          });
      });
  }

  fn profile_card(
    &mut self,
    ui: &mut Ui,
    profile: &ProfileSummary,
    profile_index: usize,
    profile_count: usize,
    busy: bool,
  ) {
    let (rect, response) = ui.allocate_exact_size(
      egui::vec2(ui.available_width(), geometry::PROFILE_CARD_HEIGHT),
      egui::Sense::click(),
    );
    let tokens = theme::tokens(ui);
    let fill = if response.hovered() {
      tokens.surface_raised
    } else {
      tokens.surface
    };
    ui.painter()
      .rect_filled(rect, geometry::GLOBAL_RADIUS, fill);
    if profile.active {
      let accent = egui::Rect::from_min_max(rect.min, egui::pos2(rect.left() + 3.0, rect.bottom()));
      ui.painter()
        .rect_filled(accent, geometry::GLOBAL_RADIUS, tokens.accent);
    }

    let content_left = rect.left() + 16.0;
    let content_right = rect.right() - 16.0;
    let header_y = rect.top() + 8.0 + geometry::PROFILE_CARD_LINE_HEIGHT / 2.0;
    let second_y = header_y + geometry::PROFILE_CARD_LINE_HEIGHT;
    let third_y = second_y + geometry::PROFILE_CARD_LINE_HEIGHT;
    let mut title_left = content_left + 18.0;
    let checkbox = if self.profile_batch_mode {
      let checkbox_rect = egui::Rect::from_center_size(
        egui::pos2(content_left + 4.0, header_y),
        egui::Vec2::splat(26.0),
      );
      let checkbox_response = ui.interact(
        checkbox_rect,
        ui.id().with(("profile-checkbox", &profile.uid)),
        egui::Sense::click(),
      );
      ui.painter().text(
        checkbox_rect.center(),
        egui::Align2::CENTER_CENTER,
        if self.selected_profiles.contains(&profile.uid) {
          "☑"
        } else {
          "☐"
        },
        egui::FontId::proportional(18.0),
        if self.selected_profiles.contains(&profile.uid) {
          tokens.accent
        } else {
          tokens.text_muted
        },
      );
      title_left += 24.0;
      Some(checkbox_response)
    } else {
      None
    };
    ui.painter().text(
      egui::pos2(title_left - 12.0, header_y),
      egui::Align2::CENTER_CENTER,
      "⠿",
      egui::FontId::proportional(17.0),
      ui.visuals().text_color(),
    );

    let update_rect = egui::Rect::from_center_size(
      egui::pos2(content_right - 2.0, header_y),
      egui::Vec2::splat(30.0),
    );
    let update = ui
      .interact(
        update_rect,
        ui.id().with(("update-profile", &profile.uid)),
        egui::Sense::click(),
      )
      .on_hover_text("更新订阅");
    if update.hovered() {
      ui.painter()
        .rect_filled(update_rect, 15.0, tokens.accent_soft);
    }
    if profile.source == ProfileSourceKind::Remote {
      ui.painter().text(
        update_rect.center(),
        egui::Align2::CENTER_CENTER,
        "↻",
        egui::FontId::proportional(18.0),
        tokens.text_muted,
      );
      if update.clicked() && !busy {
        self.command(UiCommand::UpdateProfile {
          uid: profile.uid.clone(),
        });
      }
    }

    let title_right = if profile.source == ProfileSourceKind::Remote {
      update_rect.left() - 2.0
    } else {
      content_right
    };
    let title_painter = ui.painter().with_clip_rect(egui::Rect::from_min_max(
      egui::pos2(title_left, rect.top()),
      egui::pos2(title_right, rect.top() + 34.0),
    ));
    title_painter.text(
      egui::pos2(title_left, header_y),
      egui::Align2::LEFT_CENTER,
      &profile.name,
      egui::FontId::proportional(18.0),
      if profile.active {
        tokens.accent
      } else {
        ui.visuals().text_color()
      },
    );

    let origin = profile_origin(profile);
    let updated = profile
      .updated_at
      .map_or_else(String::new, format_update_age);
    let line_clip = ui.painter().with_clip_rect(egui::Rect::from_min_max(
      egui::pos2(content_left, rect.top()),
      egui::pos2(content_right, rect.bottom()),
    ));
    line_clip.text(
      egui::pos2(content_left, second_y),
      egui::Align2::LEFT_CENTER,
      origin,
      egui::FontId::proportional(14.0),
      tokens.text_muted,
    );
    line_clip.text(
      egui::pos2(content_right, second_y),
      egui::Align2::RIGHT_CENTER,
      &updated,
      egui::FontId::proportional(14.0),
      tokens.text_muted,
    );

    let (usage_label, expire_label, progress) = profile.usage.map_or_else(
      || {
        (
          String::new(),
          profile_source_label(profile.source).to_string(),
          0.0,
        )
      },
      |usage| {
        let used = usage.upload.saturating_add(usage.download);
        let progress = if usage.total == 0 {
          0.0
        } else {
          used as f32 / usage.total as f32
        };
        (
          format!("{} / {}", format_bytes(used), format_bytes(usage.total)),
          if usage.expire == 0 {
            "长期有效".to_string()
          } else {
            format!("到期 {}", usage.expire)
          },
          progress.clamp(0.0, 1.0),
        )
      },
    );
    line_clip.text(
      egui::pos2(content_left, third_y),
      egui::Align2::LEFT_CENTER,
      usage_label,
      egui::FontId::proportional(14.0),
      tokens.text_muted,
    );
    line_clip.text(
      egui::pos2(content_right, third_y),
      egui::Align2::RIGHT_CENTER,
      expire_label,
      egui::FontId::proportional(12.0),
      tokens.text_muted,
    );
    let progress_rect = egui::Rect::from_min_max(
      egui::pos2(
        content_left,
        rect.bottom() - geometry::PROFILE_PROGRESS_HEIGHT,
      ),
      egui::pos2(content_right, rect.bottom()),
    );
    ui.painter()
      .rect_filled(progress_rect, 0.0, tokens.surface_raised);
    if progress > 0.0 {
      ui.painter().rect_filled(
        egui::Rect::from_min_max(
          progress_rect.min,
          egui::pos2(
            progress_rect.left() + progress_rect.width() * progress,
            progress_rect.bottom(),
          ),
        ),
        0.0,
        tokens.accent,
      );
    }

    if checkbox.is_some_and(|response| response.clicked())
      || (self.profile_batch_mode && response.clicked())
    {
      if !self.selected_profiles.remove(&profile.uid) {
        self.selected_profiles.insert(profile.uid.clone());
      }
      self.pending_batch_delete = false;
    } else if response.clicked()
      && !busy
      && !profile.active
      && matches!(
        profile.source,
        ProfileSourceKind::Local | ProfileSourceKind::Remote
      )
    {
      self.command(UiCommand::ActivateProfile {
        uid: profile.uid.clone(),
      });
    }
    response.context_menu(|ui| {
      self.profile_context_menu(ui, profile, profile_index, profile_count, busy);
    });
  }

  fn profile_context_menu(
    &mut self,
    ui: &mut Ui,
    profile: &ProfileSummary,
    profile_index: usize,
    profile_count: usize,
    busy: bool,
  ) {
    let source_profile = matches!(
      profile.source,
      ProfileSourceKind::Local | ProfileSourceKind::Remote
    );
    if ui
      .add_enabled(
        !busy && source_profile && !profile.active,
        egui::Button::new("选择"),
      )
      .clicked()
    {
      self.command(UiCommand::ActivateProfile {
        uid: profile.uid.clone(),
      });
      ui.close();
    }
    if profile.source == ProfileSourceKind::Remote {
      if ui
        .add_enabled(!busy, egui::Button::new("分享二维码"))
        .clicked()
      {
        self.command(UiCommand::RequestProfileQr {
          uid: profile.uid.clone(),
        });
        ui.close();
      }
      if ui.add_enabled(!busy, egui::Button::new("更新")).clicked() {
        self.command(UiCommand::UpdateProfile {
          uid: profile.uid.clone(),
        });
        ui.close();
      }
      if let Some(options) = profile.remote_options.as_ref()
        && ui
          .add_enabled(!busy, egui::Button::new("编辑信息"))
          .clicked()
      {
        self
          .profile_options_edits
          .insert(profile.uid.clone(), options.clone());
        self.editing_profile_options = Some(profile.uid.clone());
        ui.close();
      }
    }
    ui.separator();
    if ui
      .add_enabled(
        !busy && profile.source != ProfileSourceKind::Other,
        egui::Button::new("编辑文件"),
      )
      .clicked()
    {
      self.open_profile_editor(profile.uid.clone(), profile.name.clone());
      ui.close();
    }
    for (label, uid, kind) in [
      (
        "编辑规则",
        profile.enhancements.rules.as_deref(),
        SequenceEditorKind::Rules,
      ),
      (
        "编辑代理",
        profile.enhancements.proxies.as_deref(),
        SequenceEditorKind::Proxies,
      ),
      (
        "编辑代理组",
        profile.enhancements.groups.as_deref(),
        SequenceEditorKind::Groups,
      ),
    ] {
      if ui
        .add_enabled(!busy && uid.is_some(), egui::Button::new(label))
        .clicked()
      {
        if let Some(uid) = uid {
          self.open_sequence_editor(
            uid.to_string(),
            format!("{} · {}", profile.name, kind.label()),
            kind,
          );
        }
        ui.close();
      }
    }
    if ui
      .add_enabled(
        !busy && profile.enhancements.merge.is_some(),
        egui::Button::new("扩展配置"),
      )
      .clicked()
    {
      if let Some(uid) = profile.enhancements.merge.as_deref() {
        self.open_profile_editor(uid.to_string(), format!("{} · 合并配置", profile.name));
      }
      ui.close();
    }
    ui.separator();
    if ui
      .add_enabled(
        !busy && profile.source != ProfileSourceKind::Other,
        egui::Button::new("复制"),
      )
      .clicked()
    {
      self.command(UiCommand::DuplicateProfile {
        uid: profile.uid.clone(),
      });
      ui.close();
    }
    if ui.add_enabled(!busy, egui::Button::new("重命名")).clicked() {
      self
        .profile_name_edits
        .insert(profile.uid.clone(), profile.name.clone());
      self.renaming_profile = Some(profile.uid.clone());
      ui.close();
    }
    if ui
      .add_enabled(!busy && profile_index > 0, egui::Button::new("上移"))
      .clicked()
    {
      self.command(UiCommand::ReorderProfile {
        uid: profile.uid.clone(),
        new_index: profile_index - 1,
      });
      ui.close();
    }
    if ui
      .add_enabled(
        !busy && profile_index + 1 < profile_count,
        egui::Button::new("下移"),
      )
      .clicked()
    {
      self.command(UiCommand::ReorderProfile {
        uid: profile.uid.clone(),
        new_index: profile_index + 1,
      });
      ui.close();
    }
    let pending = self.pending_profile_delete.as_deref() == Some(profile.uid.as_str());
    if ui
      .add_enabled(
        !busy,
        egui::Button::new(if pending { "确认删除" } else { "删除" }),
      )
      .clicked()
    {
      if pending {
        self.command(UiCommand::DeleteProfiles {
          uids: vec![profile.uid.clone()],
        });
        self.pending_profile_delete = None;
      } else {
        self.pending_profile_delete = Some(profile.uid.clone());
      }
      ui.close();
    }
  }

  fn profile_extension_cards(&mut self, ui: &mut Ui, current: Option<&ProfileSummary>) {
    let columns =
      if geometry::breakpoint(ui.ctx().content_rect().width()) == geometry::Breakpoint::Xs {
        1
      } else {
        2
      };
    let mut edit_merge = false;
    ui.scope(|ui| {
      ui.spacing_mut().item_spacing.x = geometry::MUI_SPACING;
      ui.columns(columns, |columns| {
        edit_merge = profile_more_card(
          &mut columns[0],
          "全局合并配置",
          "Merge",
          "用于组合当前配置的确定性 YAML 增强。",
        )
        .double_clicked();
        if columns.len() > 1 {
          profile_more_card(
            &mut columns[1],
            "原生兼容增强",
            "Rust",
            &format!(
              "{} 项内置转换",
              self.snapshot.profiles.diagnostics.native_transforms.len()
            ),
          );
        }
      });
    });
    if columns == 1 {
      ui.add_space(geometry::MUI_SPACING);
      profile_more_card(
        ui,
        "原生兼容增强",
        "Rust",
        &format!(
          "{} 项内置转换",
          self.snapshot.profiles.diagnostics.native_transforms.len()
        ),
      );
    }
    if edit_merge
      && let Some(profile) = current
      && let Some(uid) = profile.enhancements.merge.as_deref()
    {
      self.open_profile_editor(uid.to_string(), format!("{} · 合并配置", profile.name));
    }
  }

  fn profile_metadata_dialogs(&mut self, context: &egui::Context, busy: bool) {
    if let Some(uid) = self.renaming_profile.clone() {
      let mut open = true;
      let mut save = false;
      let mut cancel = false;
      egui::Window::new("编辑配置")
        .open(&mut open)
        .collapsible(false)
        .default_width(420.0)
        .show(context, |ui| {
          let edit = self.profile_name_edits.entry(uid.clone()).or_default();
          ui.add(
            egui::TextEdit::singleline(edit)
              .hint_text("配置名称")
              .desired_width(f32::INFINITY),
          );
          ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            save = ui
              .add_enabled(!busy && !edit.trim().is_empty(), egui::Button::new("保存"))
              .clicked();
            cancel = ui.button("取消").clicked();
          });
        });
      if save {
        self.command(UiCommand::RenameProfile {
          uid: uid.clone(),
          name: self
            .profile_name_edits
            .get(&uid)
            .cloned()
            .unwrap_or_default(),
        });
      }
      if save || cancel || !open {
        self.renaming_profile = None;
      }
    }

    if let Some(uid) = self.editing_profile_options.clone() {
      let mut open = true;
      let mut save = false;
      let mut cancel = false;
      egui::Window::new("订阅设置")
        .open(&mut open)
        .collapsible(false)
        .default_width(460.0)
        .show(context, |ui| {
          if let Some(edit) = self.profile_options_edits.get_mut(&uid) {
            remote_profile_options_editor(ui, edit);
          }
          ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            save = ui.add_enabled(!busy, egui::Button::new("保存")).clicked();
            cancel = ui.button("取消").clicked();
          });
        });
      if save && let Some(options) = self.profile_options_edits.get(&uid).cloned() {
        self.command(UiCommand::SetRemoteProfileOptions {
          uid: uid.clone(),
          options,
        });
      }
      if save || cancel || !open {
        self.editing_profile_options = None;
      }
    }
  }

  fn profile_import_toolbar(&mut self, ui: &mut Ui, busy: bool) {
    ui.allocate_ui_with_layout(
      egui::vec2(ui.available_width(), geometry::PROFILE_CONTENT_OFFSET),
      Layout::left_to_right(Align::Center),
      |ui| {
        ui.add_space(geometry::PAGE_CONTENT_HORIZONTAL_MARGIN);
        let field_width = (ui.available_width() - 132.0).max(120.0);
        ui.add_sized(
          [field_width, geometry::PROFILE_TOOLBAR_HEIGHT],
          egui::TextEdit::singleline(&mut self.remote_profile_url)
            .password(true)
            .hint_text("订阅链接"),
        );
        if ui
          .add_enabled(
            !busy && !self.remote_profile_url.trim().is_empty(),
            egui::Button::new("导入").min_size(egui::vec2(52.0, 30.0)),
          )
          .clicked()
        {
          let name = if self.remote_profile_name.trim().is_empty() {
            "Remote profile".to_string()
          } else {
            self.remote_profile_name.trim().to_string()
          };
          self.command(UiCommand::ImportRemoteProfile {
            name,
            url: self.remote_profile_url.trim().to_string(),
            options: self.remote_profile_options.clone(),
          });
          self.remote_profile_url.clear();
        }
        if ui
          .add_enabled(
            !busy,
            egui::Button::new("新建").min_size(egui::vec2(52.0, 30.0)),
          )
          .clicked()
        {
          self.profile_create_dialog = true;
        }
      },
    );
  }

  fn profile_create_dialog(&mut self, context: &egui::Context, busy: bool) {
    if !self.profile_create_dialog {
      return;
    }
    let mut open = self.profile_create_dialog;
    let mut close = false;
    egui::Window::new("新建配置")
      .open(&mut open)
      .collapsible(false)
      .default_width(460.0)
      .show(context, |ui| {
        ui.label(RichText::new("本地配置").strong());
        ui.add(egui::TextEdit::singleline(&mut self.local_profile_name).hint_text("配置名称"));
        ui.add(
          egui::TextEdit::singleline(&mut self.local_profile_path)
            .hint_text("/path/to/profile.yaml"),
        );
        if ui
          .add_enabled(
            !busy && !self.local_profile_path.trim().is_empty(),
            egui::Button::new("导入本地文件"),
          )
          .clicked()
        {
          self.command(UiCommand::ImportLocalProfile {
            name: self.local_profile_name.trim().to_string(),
            path: self.local_profile_path.trim().to_string(),
          });
          close = true;
        }
        ui.add_space(geometry::GRID_GAP);
        ui.separator();
        ui.add_space(geometry::GRID_GAP);
        ui.label(RichText::new("订阅二维码").strong());
        ui.add(egui::TextEdit::singleline(&mut self.qr_profile_name).hint_text("订阅名称"));
        ui.add(
          egui::TextEdit::singleline(&mut self.qr_profile_path)
            .hint_text("/path/to/subscription-qr.png"),
        );
        if ui
          .add_enabled(
            !busy && !self.qr_profile_path.trim().is_empty(),
            egui::Button::new("识别并导入"),
          )
          .clicked()
        {
          self.command(UiCommand::ImportProfileQr {
            name: self.qr_profile_name.trim().to_string(),
            path: self.qr_profile_path.trim().to_string(),
            options: self.remote_profile_options.clone(),
          });
          close = true;
        }
        ui.add_space(geometry::GRID_GAP);
        ui.separator();
        ui.add_space(geometry::GRID_GAP);
        profile_diagnostics_card(ui, &self.snapshot.profiles.diagnostics);
      });
    self.profile_create_dialog = open && !close;
  }

  fn profile_yaml_editor(&mut self, ui: &mut Ui, busy: bool) {
    let mut save = None;
    let mut close = false;
    let mut cancel_close = false;
    if let Some(editor) = self.profile_editor.as_mut() {
      card(ui, &format!("YAML 编辑器 · {}", editor.name), |ui| {
        ui.label(
          RichText::new("保存前会创建快照并重新生成、校验受影响的运行配置。")
            .small()
            .weak(),
        );
        let dark_mode = ui.visuals().dark_mode;
        let ProfileEditor {
          uid,
          content,
          dirty,
          highlighter,
          ..
        } = editor;
        let mut layouter = |ui: &Ui, buffer: &dyn egui::TextBuffer, wrap_width: f32| {
          let mut job = highlighter.layout(buffer.as_str(), dark_mode);
          job.wrap.max_width = wrap_width;
          ui.fonts_mut(|fonts| fonts.layout_job(job))
        };
        let response = ui.add_enabled(
          !busy,
          egui::TextEdit::multiline(content)
            .code_editor()
            .desired_rows(24)
            .desired_width(f32::INFINITY)
            .layouter(&mut layouter),
        );
        if response.changed() {
          *dirty = true;
          self.pending_editor_close = false;
        }
        ui.horizontal(|ui| {
          if ui
            .add_enabled(!busy && *dirty, egui::Button::new("保存并校验"))
            .clicked()
          {
            save = Some((uid.clone(), content.clone()));
          }
          let close_label = if self.pending_editor_close && *dirty {
            "确认放弃修改"
          } else {
            "关闭编辑器"
          };
          if ui
            .add_enabled(!busy, egui::Button::new(close_label))
            .clicked()
          {
            if *dirty && !self.pending_editor_close {
              self.pending_editor_close = true;
            } else {
              close = true;
            }
          }
          if self.pending_editor_close
            && *dirty
            && ui
              .add_enabled(!busy, egui::Button::new("继续编辑"))
              .clicked()
          {
            cancel_close = true;
          }
          if busy {
            ui.spinner();
          }
        });
      });
    }
    if let Some((uid, content)) = save {
      self.command(UiCommand::SaveProfileContent {
        uid,
        content: SensitiveString::new(content),
      });
    }
    if close {
      self.profile_editor = None;
      self.pending_editor_close = false;
    } else if cancel_close {
      self.pending_editor_close = false;
    }
  }

  fn profile_sequence_editor(&mut self, ui: &mut Ui, busy: bool) {
    let mut save = None;
    let mut close = false;
    let mut cancel_close = false;
    if let Some(editor) = self.sequence_editor.as_mut() {
      card(
        ui,
        &format!("{}可视化编辑器 · {}", editor.kind.label(), editor.name),
        |ui| {
          ui.label(
            RichText::new(
              "按“前置 → 原配置 → 后置”的顺序生成；删除项会在合并时按规则文本或名称匹配。",
            )
            .small()
            .weak(),
          );
          ui.add_space(6.0);
          let mut changed = false;
          changed |= sequence_lane_editor(
            ui,
            "前置项目",
            "这些项目会放在订阅原有项目之前。",
            &mut editor.prepend,
            editor.kind,
            false,
            busy,
          );
          ui.add_space(8.0);
          changed |= sequence_lane_editor(
            ui,
            "后置项目",
            "这些项目会放在订阅原有项目之后。",
            &mut editor.append,
            editor.kind,
            false,
            busy,
          );
          ui.add_space(8.0);
          changed |= sequence_lane_editor(
            ui,
            "删除项目",
            "规则填写完整规则文本；代理和代理组填写 name。",
            &mut editor.delete,
            editor.kind,
            true,
            busy,
          );
          if changed {
            editor.dirty = true;
            editor.error = None;
            self.pending_editor_close = false;
          }
          if let Some(error) = editor.error.as_deref() {
            ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
          }
          ui.add_space(6.0);
          ui.horizontal(|ui| {
            if ui
              .add_enabled(!busy && editor.dirty, egui::Button::new("保存并校验"))
              .clicked()
            {
              match serialize_sequence_editor(editor) {
                Ok(content) => save = Some((editor.uid.clone(), content)),
                Err(error) => editor.error = Some(error),
              }
            }
            let close_label = if self.pending_editor_close && editor.dirty {
              "确认放弃修改"
            } else {
              "关闭编辑器"
            };
            if ui
              .add_enabled(!busy, egui::Button::new(close_label))
              .clicked()
            {
              if editor.dirty && !self.pending_editor_close {
                self.pending_editor_close = true;
              } else {
                close = true;
              }
            }
            if self.pending_editor_close
              && editor.dirty
              && ui
                .add_enabled(!busy, egui::Button::new("继续编辑"))
                .clicked()
            {
              cancel_close = true;
            }
            if busy {
              ui.spinner();
            }
          });
        },
      );
    }
    if let Some((uid, content)) = save {
      self.command(UiCommand::SaveProfileContent {
        uid,
        content: SensitiveString::new(content),
      });
    }
    if close {
      self.sequence_editor = None;
      self.pending_editor_close = false;
    } else if cancel_close {
      self.pending_editor_close = false;
    }
  }

  fn profile_qr_viewer(&mut self, ui: &mut Ui) {
    let Some(qr) = self.profile_qr.as_ref() else {
      return;
    };
    let mut close = false;
    card(ui, &format!("订阅二维码 · {}", qr.name), |ui| {
      ui.label(
        RichText::new("二维码仅在内存中生成；订阅 URL 不会进入应用状态快照。")
          .small()
          .weak(),
      );
      let side = ui.available_width().min(320.0);
      let (response, painter) = ui.allocate_painter(egui::Vec2::splat(side), egui::Sense::hover());
      painter.rect_filled(response.rect, 4.0, Color32::WHITE);
      let quiet_zone = 4_usize;
      let grid_width = qr.width.saturating_add(quiet_zone * 2);
      if qr.width > 0 && qr.modules.len() == qr.width.saturating_mul(qr.width) {
        let module_side = side / grid_width as f32;
        for (index, _) in qr.modules.iter().enumerate().filter(|(_, dark)| **dark) {
          let x = index % qr.width + quiet_zone;
          let y = index / qr.width + quiet_zone;
          let min = response.rect.min + egui::vec2(x as f32 * module_side, y as f32 * module_side);
          painter.rect_filled(
            egui::Rect::from_min_size(min, egui::Vec2::splat(module_side.ceil())),
            0.0,
            Color32::BLACK,
          );
        }
      } else {
        painter.text(
          response.rect.center(),
          egui::Align2::CENTER_CENTER,
          "二维码数据无效",
          egui::FontId::proportional(14.0),
          Color32::RED,
        );
      }
      if ui.button("关闭二维码").clicked() {
        close = true;
      }
    });
    if close {
      self.profile_qr = None;
    }
  }

  fn mode_controls(&mut self, ui: &mut Ui, current: &ProxyMode) {
    ui.horizontal(|ui| {
      for (mode, label) in [
        (ProxyMode::Rule, "规则"),
        (ProxyMode::Global, "全局"),
        (ProxyMode::Direct, "直连"),
      ] {
        if ui.selectable_label(current == &mode, label).clicked() {
          self.command(UiCommand::SetProxyMode(mode));
        }
      }
    });
  }

  fn core_controls(&mut self, ui: &mut Ui, state: &CoreState) {
    match state {
      CoreState::Stopped => {
        ui.label(RichText::new("已停止").size(18.0).strong());
        ui.label(RichText::new("选择要启动的 Mihomo 核心通道。").weak());
        self.start_buttons(ui, "启动 Stable", "启动 Alpha");
      },
      CoreState::Starting => {
        ui.label(RichText::new("正在启动…").size(18.0).strong());
        ui.spinner();
      },
      CoreState::Running {
        mode,
        channel,
        version,
      } => {
        ui.label(RichText::new("运行中").size(18.0).strong());
        let mode = match mode {
          CoreRunMode::Sidecar => "Sidecar",
          CoreRunMode::Service => "Service",
        };
        let channel_name = match channel {
          CoreChannel::Stable => "Stable",
          CoreChannel::Alpha => "Alpha",
        };
        let version = version.as_deref().unwrap_or("版本未知");
        ui.label(RichText::new(format!("{channel_name} · {mode} · {version}")).weak());
        ui.horizontal(|ui| {
          if ui.button("热加载").clicked() {
            self.command(UiCommand::ReloadCore);
          }
          if ui.button("重启").clicked() {
            self.command(UiCommand::RestartCore(*channel));
          }
          if ui.button("停止").clicked() {
            self.command(UiCommand::StopCore);
          }
        });
      },
      CoreState::Reloading => {
        ui.label(RichText::new("正在热加载…").size(18.0).strong());
        ui.spinner();
      },
      CoreState::Stopping => {
        ui.label(RichText::new("正在停止…").size(18.0).strong());
        ui.spinner();
      },
      CoreState::Failed { message } => {
        ui.label(
          RichText::new("核心异常")
            .size(18.0)
            .strong()
            .color(ui.visuals().error_fg_color),
        );
        ui.label(RichText::new(message).small().weak());
        self.start_buttons(ui, "重试 Stable", "重试 Alpha");
        if ui.button("停止并清理").clicked() {
          self.command(UiCommand::StopCore);
        }
      },
    }
  }

  fn start_buttons(&mut self, ui: &mut Ui, stable_label: &str, alpha_label: &str) {
    ui.horizontal(|ui| {
      if ui.button(stable_label).clicked() {
        self.command(UiCommand::StartCore(CoreChannel::Stable));
      }
      if ui.button(alpha_label).clicked() {
        self.command(UiCommand::StartCore(CoreChannel::Alpha));
      }
    });
  }

  fn settings(&mut self, ui: &mut Ui) {
    let state = self.snapshot.settings.clone();
    let mut draft = self.settings_draft.clone();
    let mut action = None;
    let mut requested_dialog = None;
    let viewport_width = ui.ctx().content_rect().width();
    let two_columns = geometry::settings_grid_columns(geometry::breakpoint(viewport_width)) == 2;

    let render_left = |ui: &mut Ui,
                       draft: &mut AppSettings,
                       action: &mut Option<SettingsUiAction>,
                       requested_dialog: &mut Option<SettingsSection>| {
      setting_list(ui, "系统设置", |ui| {
        setting_switch_row(ui, "TUN 模式", &mut draft.tun_enabled);
        let system_proxy = &self.snapshot.system_proxy;
        if setting_action_row(
          ui,
          "系统代理",
          if system_proxy.enabled {
            "已开启"
          } else {
            "已关闭"
          },
        )
        .clicked()
        {
          *action = Some(SettingsUiAction::ToggleSystemProxy(!system_proxy.enabled));
        }
        setting_switch_row(ui, "开机自启动", &mut draft.auto_launch);
        setting_switch_row(ui, "静默启动", &mut draft.silent_start);
      });
      ui.add_space(geometry::GRID_GAP);
      setting_list(ui, "Mihomo 设置", |ui| {
        setting_switch_row(ui, "允许局域网连接", &mut draft.allow_lan);
        if setting_action_row(
          ui,
          "DNS 设置",
          if draft.dns.enabled {
            "已启用"
          } else {
            "未启用"
          },
        )
        .clicked()
        {
          *requested_dialog = Some(SettingsSection::DnsTun);
        }
        setting_switch_row(ui, "IPv6", &mut draft.ipv6);
        setting_switch_row(ui, "统一延迟", &mut draft.unified_delay);
        settings_log_level_row(ui, &mut draft.mihomo_log_level);
        if setting_action_row(ui, "端口设置", &draft.ports.mixed.to_string()).clicked() {
          *requested_dialog = Some(SettingsSection::Mihomo);
        }
        if setting_action_row(ui, "外部控制器", &draft.controller.address).clicked() {
          *requested_dialog = Some(SettingsSection::Mihomo);
        }
        if setting_action_row(
          ui,
          "网络、Tunnels 与高级配置",
          draft.network_interface.as_deref().unwrap_or("自动"),
        )
        .clicked()
        {
          *requested_dialog = Some(SettingsSection::DnsTun);
        }
      });
    };

    let render_right =
      |ui: &mut Ui, draft: &mut AppSettings, requested_dialog: &mut Option<SettingsSection>| {
        setting_list(ui, "应用设置", |ui| {
          settings_theme_row(ui, &mut draft.theme);
          setting_switch_row(ui, "系统托盘", &mut draft.show_tray);
          setting_switch_row(ui, "流量图表", &mut draft.traffic_graph);
          setting_switch_row(ui, "内存用量", &mut draft.memory_usage);
          setting_switch_row(ui, "自动关闭连接", &mut draft.auto_close_connections);
          setting_switch_row(ui, "Wayland 全局快捷键", &mut draft.global_hotkeys);
          if setting_action_row(ui, "界面、首页与连接列", "配置").clicked() {
            *requested_dialog = Some(SettingsSection::Interface);
          }
        });
        ui.add_space(geometry::GRID_GAP);
        setting_list(ui, "高级设置", |ui| {
          if setting_action_row(
            ui,
            "特权服务",
            if self.snapshot.settings.service.reachable {
              "运行正常"
            } else if self.snapshot.settings.service.installed {
              "不可用"
            } else {
              "未安装"
            },
          )
          .clicked()
          {
            *requested_dialog = Some(SettingsSection::Proxy);
          }
          if setting_action_row(
            ui,
            "Mihomo 核心",
            match draft.core_channel {
              CoreChannel::Stable => "Stable",
              CoreChannel::Alpha => "Alpha",
            },
          )
          .clicked()
          {
            *requested_dialog = Some(SettingsSection::Maintenance);
          }
          if setting_action_row(ui, "代理控制", "PAC、绕过与服务").clicked() {
            *requested_dialog = Some(SettingsSection::Proxy);
          }
          if setting_action_row(ui, "应用行为", "启动、托盘与脚本").clicked() {
            *requested_dialog = Some(SettingsSection::General);
          }
          if setting_action_row(ui, "目录、Web UI 与诊断", "维护").clicked() {
            *requested_dialog = Some(SettingsSection::Maintenance);
          }
        });
      };

    if two_columns {
      ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = geometry::GRID_GAP;
        ui.columns(2, |columns| {
          render_left(
            &mut columns[0],
            &mut draft,
            &mut action,
            &mut requested_dialog,
          );
          render_right(&mut columns[1], &mut draft, &mut requested_dialog);
        });
      });
    } else {
      render_left(ui, &mut draft, &mut action, &mut requested_dialog);
      ui.add_space(geometry::GRID_GAP);
      render_right(ui, &mut draft, &mut requested_dialog);
    }

    if let Some(section) = requested_dialog {
      self.settings_dialog = Some(section);
    }
    if let Some(section) = self.settings_dialog {
      let mut open = true;
      egui::Window::new(section.label())
        .open(&mut open)
        .default_width(620.0)
        .default_height(560.0)
        .vscroll(true)
        .show(ui.ctx(), |ui| {
          settings_section(ui, section, &mut draft, &self.snapshot, &mut action);
        });
      if !open {
        self.settings_dialog = None;
      }
    }

    if draft != self.settings_draft {
      self.settings_draft = draft;
      self.settings_dirty = self.settings_draft != state.value;
    }
    match action {
      Some(SettingsUiAction::ToggleSystemProxy(enabled)) => {
        self.command(UiCommand::SetSystemProxy(enabled));
      },
      Some(SettingsUiAction::InstallService) => self.command(UiCommand::InstallService),
      Some(SettingsUiAction::UninstallService) => self.command(UiCommand::UninstallService),
      Some(SettingsUiAction::RegisterDeepLinks) => self.command(UiCommand::RegisterDeepLinks),
      Some(SettingsUiAction::OpenDirectory(directory)) => {
        self.command(UiCommand::OpenDirectory(directory));
      },
      Some(SettingsUiAction::OpenWebUi) => self.command(UiCommand::OpenWebUi),
      Some(SettingsUiAction::RestartCore(channel)) => {
        self.command(UiCommand::RestartCore(channel));
      },
      None => {},
    }
  }

  fn unlock(&mut self, ui: &mut Ui) {
    const SERVICES: &[&str] = &[
      "Bahamut",
      "Bilibili",
      "ChatGPT",
      "Claude",
      "Disney+",
      "Gemini",
      "Netflix",
      "Prime Video",
      "Spotify",
      "TikTok",
      "YouTube",
    ];
    let breakpoint = geometry::breakpoint(ui.ctx().content_rect().width());
    let columns = geometry::unlock_grid_columns(breakpoint);
    for row in SERVICES.chunks(columns) {
      ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = geometry::GRID_GAP;
        ui.columns(columns, |column_uis| {
          for (column, service) in column_uis.iter_mut().zip(row) {
            let tokens = theme::tokens(column);
            let width = column.available_width();
            let card = Frame::new()
              .fill(tokens.surface)
              .stroke(Stroke::new(1.0, tokens.border))
              .corner_radius(geometry::GLOBAL_RADIUS)
              .inner_margin(egui::Margin {
                left: 10,
                right: 10,
                top: 10,
                bottom: 2,
              })
              .show(column, |ui| {
                ui.set_min_width((width - 20.0).max(0.0));
                ui.set_min_height(82.0);
                ui.horizontal(|ui| {
                  ui.label(RichText::new(*service).size(16.0).strong());
                  ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                      .add_sized([32.0, 32.0], egui::Button::new("↻").corner_radius(16))
                      .clicked()
                    {
                      self.local_error =
                        Some(format!("{service} 解锁检测将在 P11 接入网络探测后可用。"));
                    }
                  });
                });
                ui.horizontal(|ui| {
                  Frame::new()
                    .fill(tokens.surface_raised)
                    .corner_radius(12)
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                      ui.label(RichText::new("◷  等待测试").small());
                    });
                });
                ui.separator();
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                  ui.label(RichText::new("-- --").size(11.0).weak());
                });
              });
            column.painter().line_segment(
              [
                card.response.rect.left_top() + egui::vec2(2.0, 4.0),
                card.response.rect.left_bottom() + egui::vec2(2.0, -4.0),
              ],
              Stroke::new(4.0, tokens.border),
            );
          }
        });
      });
      ui.add_space(geometry::GRID_GAP);
    }
  }

  fn command(&mut self, command: UiCommand) {
    if let Err(error) = self.client.try_command(command) {
      self.local_error = Some(client_error_message(&error));
    }
  }

  fn open_profile_editor(&mut self, uid: String, name: String) {
    self.pending_sequence_editor = None;
    self.pending_profile_editor_name = Some((uid.clone(), name));
    self.command(UiCommand::LoadProfileContent { uid });
  }

  fn open_sequence_editor(&mut self, uid: String, name: String, kind: SequenceEditorKind) {
    self.pending_profile_editor_name = None;
    self.pending_sequence_editor = Some(PendingSequenceEditor {
      uid: uid.clone(),
      name,
      kind,
    });
    self.command(UiCommand::LoadProfileContent { uid });
  }

  fn import_dropped_profile(&mut self, path: PathBuf) {
    let extension = path
      .extension()
      .and_then(|extension| extension.to_str())
      .map(str::to_ascii_lowercase);
    let name = path
      .file_stem()
      .and_then(|name| name.to_str())
      .unwrap_or("Imported profile")
      .to_string();
    let path = path.to_string_lossy().into_owned();
    match extension.as_deref() {
      Some("yaml" | "yml") => {
        self.command(UiCommand::ImportLocalProfile { name, path });
      },
      Some("png" | "jpg" | "jpeg") => {
        self.command(UiCommand::ImportProfileQr {
          name,
          path,
          options: self.remote_profile_options.clone(),
        });
      },
      _ => {
        self.local_error = Some("仅支持拖入 YAML 配置或 PNG/JPEG 二维码图片。".to_string());
      },
    }
  }
}

fn setting_list(ui: &mut Ui, title: &str, contents: impl FnOnce(&mut Ui)) {
  let tokens = theme::tokens(ui);
  let width = ui.available_width();
  Frame::new()
    .fill(tokens.surface)
    .corner_radius(geometry::GLOBAL_RADIUS)
    .show(ui, |ui| {
      ui.set_width(width);
      ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
      ui.allocate_ui_with_layout(
        egui::vec2(width, 48.0),
        Layout::left_to_right(Align::Center),
        |ui| {
          ui.add_space(16.0);
          ui.label(RichText::new(title).size(16.0).strong());
        },
      );
      contents(ui);
    });
}

fn setting_switch_row(ui: &mut Ui, label: &str, value: &mut bool) {
  let width = ui.available_width();
  ui.allocate_ui_with_layout(
    egui::vec2(width, 48.0),
    Layout::left_to_right(Align::Center),
    |ui| {
      ui.add_space(16.0);
      ui.label(RichText::new(label).size(14.0));
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.add_space(12.0);
        ui.checkbox(value, "");
      });
    },
  );
}

fn setting_action_row(ui: &mut Ui, label: &str, value: &str) -> egui::Response {
  let (rect, response) =
    ui.allocate_exact_size(egui::vec2(ui.available_width(), 48.0), egui::Sense::click());
  if response.hovered() {
    ui.painter()
      .rect_filled(rect, 0.0, theme::tokens(ui).surface_raised);
  }
  ui.painter().text(
    egui::pos2(rect.left() + 16.0, rect.center().y),
    egui::Align2::LEFT_CENTER,
    label,
    egui::FontId::proportional(14.0),
    ui.visuals().text_color(),
  );
  ui.painter().text(
    egui::pos2(rect.right() - 36.0, rect.center().y),
    egui::Align2::RIGHT_CENTER,
    value,
    egui::FontId::proportional(12.0),
    theme::tokens(ui).text_muted,
  );
  ui.painter().text(
    egui::pos2(rect.right() - 16.0, rect.center().y),
    egui::Align2::CENTER_CENTER,
    "›",
    egui::FontId::proportional(20.0),
    theme::tokens(ui).text_muted,
  );
  response
}

fn settings_theme_row(ui: &mut Ui, theme: &mut ThemeMode) {
  let width = ui.available_width();
  ui.allocate_ui_with_layout(
    egui::vec2(width, 48.0),
    Layout::left_to_right(Align::Center),
    |ui| {
      ui.add_space(16.0);
      ui.label("主题");
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.add_space(12.0);
        egui::ComboBox::from_id_salt("settings-summary-theme")
          .width(92.0)
          .selected_text(match theme {
            ThemeMode::System => "跟随系统",
            ThemeMode::Light => "浅色",
            ThemeMode::Dark => "深色",
          })
          .show_ui(ui, |ui| {
            ui.selectable_value(theme, ThemeMode::System, "跟随系统");
            ui.selectable_value(theme, ThemeMode::Light, "浅色");
            ui.selectable_value(theme, ThemeMode::Dark, "深色");
          });
      });
    },
  );
}

fn settings_log_level_row(ui: &mut Ui, level: &mut StreamLogLevel) {
  let width = ui.available_width();
  ui.allocate_ui_with_layout(
    egui::vec2(width, 48.0),
    Layout::left_to_right(Align::Center),
    |ui| {
      ui.add_space(16.0);
      ui.label("日志等级");
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.add_space(12.0);
        egui::ComboBox::from_id_salt("settings-summary-log-level")
          .width(92.0)
          .selected_text(stream_log_level_label(*level))
          .show_ui(ui, |ui| {
            for value in [
              StreamLogLevel::Debug,
              StreamLogLevel::Info,
              StreamLogLevel::Warning,
              StreamLogLevel::Error,
              StreamLogLevel::Silent,
            ] {
              ui.selectable_value(level, value, stream_log_level_label(value));
            }
          });
      });
    },
  );
}

fn settings_section(
  ui: &mut Ui,
  section: SettingsSection,
  draft: &mut AppSettings,
  snapshot: &AppSnapshot,
  action: &mut Option<SettingsUiAction>,
) {
  ui.label(RichText::new(section.label()).size(19.0).strong());
  ui.add_space(8.0);
  match section {
    SettingsSection::General => settings_general(ui, draft),
    SettingsSection::Proxy => settings_proxy(ui, draft, snapshot, action),
    SettingsSection::Mihomo => settings_mihomo(ui, draft),
    SettingsSection::DnsTun => settings_dns_tun(ui, draft, snapshot),
    SettingsSection::Interface => settings_interface(ui, draft),
    SettingsSection::Maintenance => settings_maintenance(ui, draft, snapshot, action),
  }
}

fn settings_general(ui: &mut Ui, draft: &mut AppSettings) {
  card(ui, "外观", |ui| {
    preference_label(ui, "颜色模式", "跟随系统或固定使用浅色/深色主题");
    ui.horizontal_wrapped(|ui| {
      for (mode, label) in [
        (ThemeMode::System, "跟随系统"),
        (ThemeMode::Light, "浅色"),
        (ThemeMode::Dark, "深色"),
      ] {
        ui.selectable_value(&mut draft.theme, mode, label);
      }
    });
  });
  ui.add_space(12.0);
  card(ui, "启动", |ui| {
    ui.checkbox(&mut draft.auto_launch, "登录后自动启动");
    ui.checkbox(&mut draft.silent_start, "自动启动时隐藏主窗口");
    preference_label(ui, "启动页面", "打开主窗口时首先显示的页面");
    egui::ComboBox::from_id_salt("settings-start-page")
      .selected_text(draft.start_page.label())
      .show_ui(ui, |ui| {
        for page in Page::ALL.into_iter().filter(|page| *page != Page::Unlock) {
          ui.selectable_value(&mut draft.start_page, page, page.label());
        }
      });
    preference_label(
      ui,
      "启动脚本",
      "保存后仅在下次主实例启动时执行；脚本由 /bin/sh 运行",
    );
    ui.add(
      egui::TextEdit::multiline(&mut draft.startup_script)
        .hint_text("留空表示不执行")
        .desired_rows(4)
        .code_editor(),
    );
  });
  ui.add_space(12.0);
  card(ui, "托盘", |ui| {
    ui.checkbox(&mut draft.show_tray, "显示系统托盘图标");
    preference_label(ui, "单击托盘图标", "选择主操作");
    egui::ComboBox::from_id_salt("settings-tray-click")
      .selected_text(match draft.tray_click {
        TrayClickAction::ToggleWindow => "显示或隐藏窗口",
        TrayClickAction::ShowMenu => "显示菜单",
        TrayClickAction::Disabled => "不执行操作",
      })
      .show_ui(ui, |ui| {
        ui.selectable_value(
          &mut draft.tray_click,
          TrayClickAction::ToggleWindow,
          "显示或隐藏窗口",
        );
        ui.selectable_value(&mut draft.tray_click, TrayClickAction::ShowMenu, "显示菜单");
        ui.selectable_value(
          &mut draft.tray_click,
          TrayClickAction::Disabled,
          "不执行操作",
        );
      });
  });
}

fn settings_proxy(
  ui: &mut Ui,
  draft: &mut AppSettings,
  snapshot: &AppSnapshot,
  action: &mut Option<SettingsUiAction>,
) {
  let system = &snapshot.system_proxy;
  card(ui, "系统代理", |ui| {
    preference_label(
      ui,
      if system.enabled {
        "系统代理已开启"
      } else {
        "系统代理已关闭"
      },
      system
        .backend
        .as_deref()
        .unwrap_or("正在检测 Linux 桌面后端"),
    );
    if ui
      .add_enabled(
        !system.busy
          && (system.enabled
            || (system.available
              && (draft.pac_url.is_some()
                || snapshot.mihomo.connection == MihomoConnection::Connected))),
        egui::Button::new(if system.enabled {
          "关闭系统代理"
        } else {
          "开启系统代理"
        })
        .selected(system.enabled),
      )
      .clicked()
    {
      *action = Some(SettingsUiAction::ToggleSystemProxy(!system.enabled));
    }
    if let Some(detail) = system.detail.as_deref() {
      ui.label(
        RichText::new(detail)
          .small()
          .color(theme::tokens(ui).warning),
      );
    }
    ui.separator();
    preference_label(ui, "绕过列表", "每行一个主机、域名或 CIDR");
    string_list_editor(ui, &mut draft.system_proxy_bypass, "localhost");
    ui.separator();
    let mut pac = draft.pac_url.is_some();
    if ui.checkbox(&mut pac, "使用 PAC 自动配置 URL").changed() {
      draft.pac_url = pac.then(|| "http://127.0.0.1/proxy.pac".to_string());
    }
    if let Some(url) = draft.pac_url.as_mut() {
      ui.add(
        egui::TextEdit::singleline(url)
          .hint_text("https://example.test/proxy.pac")
          .desired_width(f32::INFINITY),
      );
    }
  });
  ui.add_space(12.0);
  card(ui, "特权服务", |ui| {
    let service = &snapshot.settings.service;
    preference_label(
      ui,
      if service.reachable {
        "服务运行正常"
      } else if service.installed {
        "服务已安装但不可用"
      } else {
        "服务尚未安装"
      },
      service
        .version
        .as_deref()
        .or(service.detail.as_deref())
        .unwrap_or("TUN 需要安装一次受限特权服务"),
    );
    ui.horizontal_wrapped(|ui| {
      if ui
        .add_enabled(
          !snapshot.settings.busy && !service.reachable,
          egui::Button::new(if service.installed {
            "修复或升级服务"
          } else {
            "安装服务"
          }),
        )
        .clicked()
      {
        *action = Some(SettingsUiAction::InstallService);
      }
      if ui
        .add_enabled(
          !snapshot.settings.busy && service.installed,
          egui::Button::new("卸载服务"),
        )
        .clicked()
      {
        *action = Some(SettingsUiAction::UninstallService);
      }
    });
    ui.label(
      RichText::new("安装和卸载通过 polkit 显示系统密码窗口；应用不会保存 root 凭据。")
        .small()
        .weak(),
    );
  });
}

fn settings_mihomo(ui: &mut Ui, draft: &mut AppSettings) {
  card(ui, "基础网络", |ui| {
    ui.checkbox(&mut draft.allow_lan, "允许局域网设备连接");
    ui.checkbox(&mut draft.ipv6, "启用 IPv6");
    ui.checkbox(&mut draft.unified_delay, "使用统一延迟");
    preference_label(ui, "Mihomo 日志等级", "控制核心生成的日志详细程度");
    egui::ComboBox::from_id_salt("settings-mihomo-log-level")
      .selected_text(stream_log_level_label(draft.mihomo_log_level))
      .show_ui(ui, |ui| {
        for level in [
          StreamLogLevel::Debug,
          StreamLogLevel::Info,
          StreamLogLevel::Warning,
          StreamLogLevel::Error,
          StreamLogLevel::Silent,
        ] {
          ui.selectable_value(
            &mut draft.mihomo_log_level,
            level,
            stream_log_level_label(level),
          );
        }
      });
  });
  ui.add_space(12.0);
  card(ui, "监听端口", |ui| {
    ui.horizontal(|ui| {
      ui.label("Mixed");
      ui.add(egui::DragValue::new(&mut draft.ports.mixed).range(1..=u16::MAX));
    });
    optional_port_editor(ui, "SOCKS", &mut draft.ports.socks, 17_898);
    optional_port_editor(ui, "HTTP", &mut draft.ports.http, 17_899);
    optional_port_editor(ui, "Redir", &mut draft.ports.redir, 17_900);
    optional_port_editor(ui, "TProxy", &mut draft.ports.tproxy, 17_901);
    ui.label(
      RichText::new("保存前会拒绝端口 0 和重复端口。")
        .small()
        .weak(),
    );
  });
  ui.add_space(12.0);
  card(ui, "外部控制器与 CORS", |ui| {
    ui.checkbox(&mut draft.controller.enabled, "开放 TCP 外部控制器");
    ui.add_enabled_ui(draft.controller.enabled, |ui| {
      ui.add(egui::TextEdit::singleline(&mut draft.controller.address).hint_text("127.0.0.1:9090"));
      let mut secret = draft.controller.secret.expose().to_string();
      if ui
        .add(
          egui::TextEdit::singleline(&mut secret)
            .password(true)
            .hint_text("Controller secret"),
        )
        .changed()
      {
        draft.controller.secret = SensitiveString::new(secret);
      }
      ui.checkbox(
        &mut draft.controller.allow_private_network,
        "允许浏览器 Private Network 请求",
      );
      preference_label(ui, "允许的 Origins", "每行一个 HTTP(S) origin，也可使用 *");
      string_list_editor(
        ui,
        &mut draft.controller.allowed_origins,
        "http://localhost",
      );
    });
  });
}

fn settings_dns_tun(ui: &mut Ui, draft: &mut AppSettings, snapshot: &AppSnapshot) {
  let service_active = matches!(
    snapshot.core,
    CoreState::Running {
      mode: CoreRunMode::Service,
      ..
    }
  );
  card(ui, "TUN", |ui| {
    ui.add_enabled_ui(service_active || draft.tun_enabled, |ui| {
      ui.checkbox(&mut draft.tun_enabled, "启用 TUN 模式");
    });
    if !service_active && !draft.tun_enabled {
      ui.label(
        RichText::new("先安装特权服务并重启核心，才能启用 rsclash TUN 网卡。")
          .small()
          .color(theme::tokens(ui).warning),
      );
    }
    preference_label(ui, "网络栈", "默认 mixed 兼顾兼容性与性能");
    egui::ComboBox::from_id_salt("settings-tun-stack")
      .selected_text(match draft.tun_stack {
        TunStack::System => "system",
        TunStack::Gvisor => "gvisor",
        TunStack::Mixed => "mixed",
      })
      .show_ui(ui, |ui| {
        ui.selectable_value(&mut draft.tun_stack, TunStack::System, "system");
        ui.selectable_value(&mut draft.tun_stack, TunStack::Gvisor, "gvisor");
        ui.selectable_value(&mut draft.tun_stack, TunStack::Mixed, "mixed");
      });
    let mut automatic = draft.network_interface.is_none();
    if ui.checkbox(&mut automatic, "自动检测网络接口").changed() {
      draft.network_interface = (!automatic).then(String::new);
    }
    if let Some(interface) = draft.network_interface.as_mut() {
      ui.add(egui::TextEdit::singleline(interface).hint_text("例如 wlan0"));
    }
    ui.label(
      RichText::new("TUN 设备名固定为 rsclash，避免与其他代理客户端冲突。")
        .small()
        .weak(),
    );
  });
  ui.add_space(12.0);
  card(ui, "DNS", |ui| {
    ui.checkbox(&mut draft.dns.enabled, "使用 rsclash DNS 覆盖");
    ui.add_enabled_ui(draft.dns.enabled, |ui| {
      ui.horizontal(|ui| {
        ui.label("监听");
        ui.add(egui::TextEdit::singleline(&mut draft.dns.listen).hint_text("0.0.0.0:1053"));
      });
      ui.checkbox(&mut draft.dns.ipv6, "DNS 返回 IPv6");
      egui::ComboBox::from_id_salt("settings-dns-mode")
        .selected_text(match draft.dns.enhanced_mode {
          DnsEnhancedMode::Normal => "normal",
          DnsEnhancedMode::RedirHost => "redir-host",
          DnsEnhancedMode::FakeIp => "fake-ip",
        })
        .show_ui(ui, |ui| {
          ui.selectable_value(
            &mut draft.dns.enhanced_mode,
            DnsEnhancedMode::Normal,
            "normal",
          );
          ui.selectable_value(
            &mut draft.dns.enhanced_mode,
            DnsEnhancedMode::RedirHost,
            "redir-host",
          );
          ui.selectable_value(
            &mut draft.dns.enhanced_mode,
            DnsEnhancedMode::FakeIp,
            "fake-ip",
          );
        });
      if draft.dns.enhanced_mode == DnsEnhancedMode::FakeIp {
        ui.add(egui::TextEdit::singleline(&mut draft.dns.fake_ip_range).hint_text("198.18.0.1/16"));
      }
      preference_label(ui, "默认 DNS", "用于解析 DoH/DoT 服务器域名");
      string_list_editor(ui, &mut draft.dns.default_nameservers, "223.5.5.5");
      preference_label(ui, "Nameservers", "主要解析服务器");
      string_list_editor(
        ui,
        &mut draft.dns.nameservers,
        "https://dns.alidns.com/dns-query",
      );
      preference_label(ui, "Fallback", "可选的后备解析服务器");
      string_list_editor(ui, &mut draft.dns.fallback, "https://1.1.1.1/dns-query");
    });
  });
  ui.add_space(12.0);
  card(ui, "Tunnels", |ui| {
    let mut remove = None;
    for (index, tunnel) in draft.tunnels.iter_mut().enumerate() {
      Frame::new()
        .fill(theme::tokens(ui).surface_raised)
        .corner_radius(8)
        .inner_margin(10)
        .show(ui, |ui| {
          let mut tcp = tunnel.network.iter().any(|network| network == "tcp");
          let mut udp = tunnel.network.iter().any(|network| network == "udp");
          ui.horizontal(|ui| {
            ui.checkbox(&mut tcp, "TCP");
            ui.checkbox(&mut udp, "UDP");
            if ui.button("删除").clicked() {
              remove = Some(index);
            }
          });
          tunnel.network.clear();
          if tcp {
            tunnel.network.push("tcp".to_string());
          }
          if udp {
            tunnel.network.push("udp".to_string());
          }
          ui.add(egui::TextEdit::singleline(&mut tunnel.address).hint_text("127.0.0.1:8000"));
          ui.add(egui::TextEdit::singleline(&mut tunnel.target).hint_text("target.example:443"));
          let proxy_empty = {
            let proxy = tunnel.proxy.get_or_insert_default();
            ui.add(egui::TextEdit::singleline(proxy).hint_text("代理组（可选）"));
            proxy.trim().is_empty()
          };
          if proxy_empty {
            tunnel.proxy = None;
          }
        });
      ui.add_space(6.0);
    }
    if let Some(index) = remove {
      draft.tunnels.remove(index);
    }
    if ui.button("添加 Tunnel").clicked() {
      draft.tunnels.push(rsclash_domain::TunnelSettings {
        network: vec!["tcp".to_string()],
        address: "127.0.0.1:8000".to_string(),
        target: String::new(),
        proxy: None,
      });
    }
  });
}

fn settings_interface(ui: &mut Ui, draft: &mut AppSettings) {
  card(ui, "布局", |ui| {
    ui.checkbox(&mut draft.traffic_graph, "首页显示流量图");
    ui.checkbox(&mut draft.memory_usage, "首页显示内存用量");
    ui.checkbox(&mut draft.show_tray, "显示托盘图标");
    ui.checkbox(&mut draft.global_hotkeys, "启用桌面全局快捷键");
    ui.label(
      RichText::new("首次启用时，Wayland 桌面将通过 XDG Portal 请求快捷键权限。")
        .small()
        .weak(),
    );
    preference_label(ui, "首页卡片", "启用卡片并调整它们在首页中的显示顺序");
    ordered_home_card_editor(ui, &mut draft.home_cards);
    ui.horizontal(|ui| {
      ui.label("刷新间隔");
      ui.add(
        egui::DragValue::new(&mut draft.refresh_interval_ms)
          .range(100..=60_000)
          .suffix(" ms"),
      );
    });
    preference_label(ui, "导航栏", "自动模式会在窄窗口折叠");
    egui::ComboBox::from_id_salt("settings-navigation-layout")
      .selected_text(match draft.navigation_layout {
        NavigationLayout::Automatic => "自动",
        NavigationLayout::Expanded => "展开",
        NavigationLayout::Compact => "紧凑",
      })
      .show_ui(ui, |ui| {
        ui.selectable_value(
          &mut draft.navigation_layout,
          NavigationLayout::Automatic,
          "自动",
        );
        ui.selectable_value(
          &mut draft.navigation_layout,
          NavigationLayout::Expanded,
          "展开",
        );
        ui.selectable_value(
          &mut draft.navigation_layout,
          NavigationLayout::Compact,
          "紧凑",
        );
      });
    preference_label(ui, "代理组布局", "卡片更直观，紧凑模式提高信息密度");
    ui.horizontal(|ui| {
      ui.selectable_value(
        &mut draft.proxy_group_layout,
        ProxyGroupLayout::Cards,
        "卡片",
      );
      ui.selectable_value(
        &mut draft.proxy_group_layout,
        ProxyGroupLayout::Compact,
        "紧凑",
      );
      ui.add(
        egui::DragValue::new(&mut draft.proxy_layout_columns)
          .range(1..=6)
          .suffix(" 列"),
      );
    });
  });
  ui.add_space(12.0);
  card(ui, "连接与测速", |ui| {
    ui.checkbox(
      &mut draft.auto_close_connections,
      "切换配置或代理后自动关闭旧连接",
    );
    ui.checkbox(&mut draft.auto_test, "更新配置后自动测速");
    ui.add(
      egui::TextEdit::singleline(&mut draft.latency_test_url)
        .hint_text("https://www.gstatic.com/generate_204"),
    );
    ui.add(
      egui::DragValue::new(&mut draft.latency_timeout_ms)
        .range(100..=120_000)
        .suffix(" ms"),
    );
    preference_label(ui, "连接列", "目标和流量始终显示，以下字段可以单独开关");
    ui.horizontal_wrapped(|ui| {
      setting_membership_checkbox(ui, &mut draft.connection_columns, "process", "进程");
      setting_membership_checkbox(ui, &mut draft.connection_columns, "rule", "规则");
      setting_membership_checkbox(ui, &mut draft.connection_columns, "chains", "代理链");
    });
  });
  ui.add_space(12.0);
  card(ui, "应用日志保留", |ui| {
    ui.horizontal_wrapped(|ui| {
      ui.add(
        egui::DragValue::new(&mut draft.app_log_max_size_mib)
          .range(1..=1024)
          .suffix(" MiB/文件"),
      );
      ui.add(
        egui::DragValue::new(&mut draft.app_log_max_count)
          .range(1..=100)
          .suffix(" 个文件"),
      );
      ui.add(
        egui::DragValue::new(&mut draft.app_log_retention_days)
          .range(1..=365)
          .suffix(" 天"),
      );
    });
  });
}

fn settings_maintenance(
  ui: &mut Ui,
  draft: &mut AppSettings,
  snapshot: &AppSnapshot,
  action: &mut Option<SettingsUiAction>,
) {
  card(ui, "核心通道", |ui| {
    ui.horizontal(|ui| {
      ui.selectable_value(&mut draft.core_channel, CoreChannel::Stable, "Stable");
      ui.selectable_value(&mut draft.core_channel, CoreChannel::Alpha, "Alpha");
      if matches!(snapshot.core, CoreState::Running { .. }) && ui.button("切换并重启").clicked()
      {
        *action = Some(SettingsUiAction::RestartCore(draft.core_channel));
      }
    });
    ui.label(
      RichText::new("核心与 GeoData 的安全下载、哈希校验和发布更新由打包阶段统一提供。")
        .small()
        .weak(),
    );
  });
  ui.add_space(12.0);
  card(ui, "目录", |ui| {
    for (directory, label, path) in [
      (
        ApplicationDirectory::Configuration,
        "配置目录",
        snapshot.settings.paths.configuration.as_str(),
      ),
      (
        ApplicationDirectory::Data,
        "数据目录",
        snapshot.settings.paths.data.as_str(),
      ),
      (
        ApplicationDirectory::Logs,
        "日志目录",
        snapshot.settings.paths.logs.as_str(),
      ),
      (
        ApplicationDirectory::Core,
        "核心目录",
        snapshot.settings.paths.core.as_str(),
      ),
    ] {
      ui.horizontal(|ui| {
        ui.vertical(|ui| {
          ui.label(RichText::new(label).strong());
          ui.label(RichText::new(path).small().weak());
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
          if ui.button("打开").clicked() {
            *action = Some(SettingsUiAction::OpenDirectory(directory));
          }
        });
      });
      ui.separator();
    }
  });
  ui.add_space(12.0);
  card(ui, "桌面集成", |ui| {
    if ui.button("注册订阅深链").clicked() {
      *action = Some(SettingsUiAction::RegisterDeepLinks);
    }
    if ui
      .add_enabled(
        draft.controller.enabled,
        egui::Button::new("打开外部 Web UI"),
      )
      .clicked()
    {
      *action = Some(SettingsUiAction::OpenWebUi);
    }
    ui.label(
      RichText::new("外部 Web UI 始终在默认浏览器中打开，不会嵌入 WebView。")
        .small()
        .weak(),
    );
  });
}

fn preference_label(ui: &mut Ui, title: &str, description: &str) {
  ui.label(RichText::new(title).strong());
  ui.label(RichText::new(description).small().weak());
}

fn normalized_home_cards(cards: &[String]) -> Vec<String> {
  let mut seen = BTreeSet::new();
  let cards = cards
    .iter()
    .filter(|card| matches!(card.as_str(), "profile" | "proxy" | "network" | "traffic"))
    .filter(|card| seen.insert(card.as_str()))
    .cloned()
    .collect::<Vec<_>>();
  if cards.is_empty() {
    vec!["profile".to_string()]
  } else {
    cards
  }
}

fn ordered_home_card_editor(ui: &mut Ui, cards: &mut Vec<String>) {
  let mut order = normalized_home_cards(cards);
  for key in ["profile", "proxy", "network", "traffic"] {
    if !order.iter().any(|card| card == key) {
      order.push(key.to_string());
    }
  }
  let mut action = None;
  for key in order {
    let position = cards.iter().position(|card| card == &key);
    let mut enabled = position.is_some();
    ui.horizontal(|ui| {
      if ui.checkbox(&mut enabled, home_card_label(&key)).changed() {
        action = Some(if enabled {
          HomeCardEdit::Enable(key.clone())
        } else {
          HomeCardEdit::Disable(key.clone())
        });
      }
      if let Some(position) = position {
        if ui
          .add_enabled(position > 0, egui::Button::new("↑"))
          .clicked()
        {
          action = Some(HomeCardEdit::MoveUp(position));
        }
        if ui
          .add_enabled(position + 1 < cards.len(), egui::Button::new("↓"))
          .clicked()
        {
          action = Some(HomeCardEdit::MoveDown(position));
        }
      }
    });
  }
  match action {
    Some(HomeCardEdit::Enable(key)) => cards.push(key),
    Some(HomeCardEdit::Disable(key)) => cards.retain(|card| card != &key),
    Some(HomeCardEdit::MoveUp(position)) => cards.swap(position, position - 1),
    Some(HomeCardEdit::MoveDown(position)) => cards.swap(position, position + 1),
    None => {},
  }
}

enum HomeCardEdit {
  Enable(String),
  Disable(String),
  MoveUp(usize),
  MoveDown(usize),
}

fn home_card_label(key: &str) -> &str {
  match key {
    "profile" => "核心与当前配置",
    "proxy" => "出站模式",
    "network" => "系统代理与 TUN",
    "traffic" => "流量与资源",
    _ => key,
  }
}

fn setting_membership_checkbox(ui: &mut Ui, values: &mut Vec<String>, key: &str, label: &str) {
  let mut enabled = values.iter().any(|value| value == key);
  if ui.checkbox(&mut enabled, label).changed() {
    if enabled {
      values.push(key.to_string());
    } else {
      values.retain(|value| value != key);
    }
  }
}

fn optional_port_editor(ui: &mut Ui, label: &str, port: &mut Option<u16>, default: u16) {
  ui.horizontal(|ui| {
    let mut enabled = port.is_some();
    if ui.checkbox(&mut enabled, label).changed() {
      *port = enabled.then_some(default);
    }
    if let Some(port) = port.as_mut() {
      ui.add(egui::DragValue::new(port).range(1..=u16::MAX));
    }
  });
}

fn string_list_editor(ui: &mut Ui, values: &mut Vec<String>, hint: &str) {
  let mut remove = None;
  for (index, value) in values.iter_mut().enumerate() {
    ui.horizontal(|ui| {
      ui.add(
        egui::TextEdit::singleline(value)
          .hint_text(hint)
          .desired_width((ui.available_width() - 52.0).max(160.0)),
      );
      if ui.small_button("删除").clicked() {
        remove = Some(index);
      }
    });
  }
  if let Some(index) = remove {
    values.remove(index);
  }
  if ui.small_button("添加一项").clicked() {
    values.push(String::new());
  }
}

#[derive(Clone, Copy)]
enum SequenceLaneAction {
  Add,
  MoveUp(usize),
  MoveDown(usize),
  Remove(usize),
}

fn sequence_lane_editor(
  ui: &mut Ui,
  title: &str,
  description: &str,
  items: &mut Vec<String>,
  kind: SequenceEditorKind,
  delete_lane: bool,
  busy: bool,
) -> bool {
  let mut changed = false;
  let mut action = None;
  ui.group(|ui| {
    ui.set_min_width(ui.available_width());
    ui.horizontal(|ui| {
      ui.vertical(|ui| {
        ui.label(RichText::new(title).strong());
        ui.label(RichText::new(description).small().weak());
      });
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui.add_enabled(!busy, egui::Button::new("添加")).clicked() {
          action = Some(SequenceLaneAction::Add);
        }
      });
    });
    if items.is_empty() {
      ui.label(RichText::new("没有项目").small().weak());
    }
    let item_count = items.len();
    for (index, item) in items.iter_mut().enumerate() {
      ui.push_id((title, index), |ui| {
        ui.separator();
        ui.horizontal(|ui| {
          ui.label(
            RichText::new(format!("{} {}", kind.label(), index + 1))
              .small()
              .strong(),
          );
          if ui
            .add_enabled(!busy && index > 0, egui::Button::new("↑"))
            .on_hover_text("上移")
            .clicked()
          {
            action = Some(SequenceLaneAction::MoveUp(index));
          }
          if ui
            .add_enabled(!busy && index + 1 < item_count, egui::Button::new("↓"))
            .on_hover_text("下移")
            .clicked()
          {
            action = Some(SequenceLaneAction::MoveDown(index));
          }
          if ui.add_enabled(!busy, egui::Button::new("删除")).clicked() {
            action = Some(SequenceLaneAction::Remove(index));
          }
        });
        let response = if delete_lane || kind == SequenceEditorKind::Rules {
          ui.add_enabled(
            !busy,
            egui::TextEdit::singleline(item).desired_width(f32::INFINITY),
          )
        } else {
          ui.add_enabled(
            !busy,
            egui::TextEdit::multiline(item)
              .code_editor()
              .desired_rows(if kind == SequenceEditorKind::Groups {
                5
              } else {
                4
              })
              .desired_width(f32::INFINITY),
          )
        };
        changed |= response.changed();
      });
    }
  });
  match action {
    Some(SequenceLaneAction::Add) => {
      items.push(if delete_lane {
        String::new()
      } else {
        kind.default_item()
      });
      true
    },
    Some(SequenceLaneAction::MoveUp(index)) => {
      items.swap(index, index - 1);
      true
    },
    Some(SequenceLaneAction::MoveDown(index)) => {
      items.swap(index, index + 1);
      true
    },
    Some(SequenceLaneAction::Remove(index)) => {
      items.remove(index);
      true
    },
    None => changed,
  }
}

fn parse_sequence_editor(
  uid: String,
  name: String,
  kind: SequenceEditorKind,
  content: &str,
) -> Result<SequenceEditor, String> {
  let value = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(content)
    .map_err(|error| format!("无法解析{}扩展：{error}", kind.label()))?;
  let serde_yaml_ng::Value::Mapping(mapping) = value else {
    return Err(format!("{}扩展的顶层必须是映射。", kind.label()));
  };
  Ok(SequenceEditor {
    uid,
    name,
    kind,
    prepend: parse_sequence_lane(&mapping, "prepend", kind, false)?,
    append: parse_sequence_lane(&mapping, "append", kind, false)?,
    delete: parse_sequence_lane(&mapping, "delete", kind, true)?,
    dirty: false,
    error: None,
  })
}

fn parse_sequence_lane(
  mapping: &serde_yaml_ng::Mapping,
  key: &str,
  kind: SequenceEditorKind,
  delete_lane: bool,
) -> Result<Vec<String>, String> {
  let Some(value) = mapping.get(serde_yaml_ng::Value::String(key.to_string())) else {
    return Ok(Vec::new());
  };
  let serde_yaml_ng::Value::Sequence(values) = value else {
    return Err(format!("字段 {key} 必须是列表。"));
  };
  values
    .iter()
    .map(|value| {
      if delete_lane || kind == SequenceEditorKind::Rules {
        value
          .as_str()
          .map(str::to_string)
          .ok_or_else(|| format!("字段 {key} 只能包含文本。"))
      } else {
        serde_yaml_ng::to_string(value)
          .map(|yaml| yaml.trim().to_string())
          .map_err(|error| format!("无法读取字段 {key}：{error}"))
      }
    })
    .collect()
}

fn serialize_sequence_editor(editor: &SequenceEditor) -> Result<String, String> {
  let mut mapping = serde_yaml_ng::Mapping::new();
  for (key, items, delete_lane) in [
    ("prepend", &editor.prepend, false),
    ("append", &editor.append, false),
    ("delete", &editor.delete, true),
  ] {
    let values = items
      .iter()
      .enumerate()
      .map(|(index, item)| sequence_item_value(editor.kind, key, index, item, delete_lane))
      .collect::<Result<Vec<_>, _>>()?;
    mapping.insert(
      serde_yaml_ng::Value::String(key.to_string()),
      serde_yaml_ng::Value::Sequence(values),
    );
  }
  serde_yaml_ng::to_string(&serde_yaml_ng::Value::Mapping(mapping))
    .map_err(|error| format!("无法生成{}扩展：{error}", editor.kind.label()))
}

fn sequence_item_value(
  kind: SequenceEditorKind,
  lane: &str,
  index: usize,
  item: &str,
  delete_lane: bool,
) -> Result<serde_yaml_ng::Value, String> {
  let item = item.trim();
  if item.is_empty() {
    return Err(format!("{lane} 的第 {} 项不能为空。", index + 1));
  }
  if delete_lane || kind == SequenceEditorKind::Rules {
    return Ok(serde_yaml_ng::Value::String(item.to_string()));
  }
  let value = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(item)
    .map_err(|error| format!("{lane} 的第 {} 项不是有效 YAML：{error}", index + 1))?;
  let serde_yaml_ng::Value::Mapping(mapping) = &value else {
    return Err(format!("{lane} 的第 {} 项必须是 YAML 映射。", index + 1));
  };
  for field in ["name", "type"] {
    if mapping
      .get(serde_yaml_ng::Value::String(field.to_string()))
      .and_then(serde_yaml_ng::Value::as_str)
      .is_none_or(str::is_empty)
    {
      return Err(format!(
        "{lane} 的第 {} 项必须包含文本字段 {field}。",
        index + 1
      ));
    }
  }
  Ok(value)
}

fn profile_diagnostics_card(ui: &mut Ui, diagnostics: &ProfileDiagnostics) {
  card(ui, "原生增强与运行诊断", |ui| {
    if let Some(last) = diagnostics.last_operation.as_ref() {
      let color = if last.success {
        Color32::from_rgb(38, 162, 105)
      } else {
        ui.visuals().error_fg_color
      };
      ui.horizontal_wrapped(|ui| {
        ui.label(
          RichText::new(if last.success {
            "最近操作成功"
          } else {
            "最近操作失败"
          })
          .strong()
          .color(color),
        );
        ui.label(format!(
          "{} · {} · {}",
          profile_operation_label(last.operation),
          profile_stage_label(last.stage),
          format_update_age(last.timestamp)
        ));
      });
      if !last.success {
        ui.label(RichText::new(&last.message).small().color(color));
      }
    } else {
      ui.label(RichText::new("本次启动尚未执行配置操作。").small().weak());
    }
    ui.add_space(8.0);
    egui::CollapsingHeader::new(format!(
      "启用的原生兼容增强（{}）",
      diagnostics.native_transforms.len()
    ))
    .default_open(true)
    .show(ui, |ui| {
      for transform in &diagnostics.native_transforms {
        ui.label(format!("• {transform}"));
      }
    });
    egui::CollapsingHeader::new(format!(
      "生成、校验与部署顺序（{} 步）",
      diagnostics.pipeline_order.len()
    ))
    .show(ui, |ui| {
      for (index, stage) in diagnostics.pipeline_order.iter().enumerate() {
        ui.label(format!("{}. {stage}", index + 1));
      }
    });
  });
}

const fn profile_operation_label(operation: ProfileOperationKind) -> &'static str {
  match operation {
    ProfileOperationKind::Import => "导入配置",
    ProfileOperationKind::Activate => "激活配置",
    ProfileOperationKind::Update => "更新订阅",
    ProfileOperationKind::Edit => "编辑配置",
    ProfileOperationKind::Manage => "管理配置",
    ProfileOperationKind::AutomaticUpdate => "自动更新",
  }
}

const fn profile_stage_label(stage: ProfileDiagnosticStage) -> &'static str {
  match stage {
    ProfileDiagnosticStage::Download => "下载阶段",
    ProfileDiagnosticStage::Enhancement => "增强生成阶段",
    ProfileDiagnosticStage::Validation => "Mihomo 校验阶段",
    ProfileDiagnosticStage::Deployment => "运行配置部署阶段",
    ProfileDiagnosticStage::Storage => "配置存储阶段",
    ProfileDiagnosticStage::Completed => "全部阶段完成",
  }
}

fn proxy_groups(view: &ProxyViewV1) -> impl Iterator<Item = &ProxyGroupView> {
  view.global.iter().chain(&view.groups)
}

fn proxy_display_item(member: &ProxyMemberSnapshot, view: &ProxyViewV1) -> ProxyDisplayItem {
  match member {
    ProxyMemberSnapshot::Node { name, record_id } => view.records.get(record_id).map_or_else(
      || ProxyDisplayItem {
        name: name.clone(),
        kind: "Unknown".to_string(),
        record_id: None,
        alive: false,
        delay_ms: None,
        source: "Missing record".to_string(),
        capabilities: ProxyCapabilities::default(),
        unresolved: Some(ProxyMemberUnresolvedReason::Missing),
        chain_eligible: false,
      },
      proxy_record_display,
    ),
    ProxyMemberSnapshot::Group { name } => {
      let nested = proxy_groups(view).find(|group| group.name == *name);
      ProxyDisplayItem {
        name: name.clone(),
        kind: nested.map_or_else(|| "Group".to_string(), |group| group.kind.clone()),
        record_id: None,
        alive: nested.is_none_or(|group| group.alive),
        delay_ms: nested.and_then(|group| group.delay_ms),
        source: "Nested group".to_string(),
        capabilities: nested.map_or_else(ProxyCapabilities::default, |group| {
          group.capabilities.clone()
        }),
        unresolved: None,
        chain_eligible: false,
      }
    },
    ProxyMemberSnapshot::Unresolved { name, reason } => ProxyDisplayItem {
      name: name.clone(),
      kind: "Unresolved".to_string(),
      record_id: None,
      alive: false,
      delay_ms: None,
      source: "Unresolved member".to_string(),
      capabilities: ProxyCapabilities::default(),
      unresolved: Some(*reason),
      chain_eligible: false,
    },
  }
}

fn proxy_record_display(record: &ProxyNodeSnapshot) -> ProxyDisplayItem {
  let source = match record.source.as_ref() {
    Some(ProxyNodeSource::Core { .. }) => "Core".to_string(),
    Some(ProxyNodeSource::Provider { provider_name, .. }) => {
      format!("Provider: {provider_name}")
    },
    None => "Unknown source".to_string(),
  };
  let chain_eligible = matches!(record.source.as_ref(), Some(ProxyNodeSource::Core { .. }));
  ProxyDisplayItem {
    name: record.name.clone(),
    kind: record.kind.clone(),
    record_id: Some(record.record_id.clone()),
    alive: record.alive,
    delay_ms: record.delay_ms,
    source,
    capabilities: record.capabilities.clone(),
    unresolved: None,
    chain_eligible,
  }
}

fn proxy_item_matches(
  item: &ProxyDisplayItem,
  query: &str,
  regex: Option<&regex_lite::Regex>,
  regex_mode: bool,
  whole_word: bool,
) -> bool {
  let query = query.trim();
  if query.is_empty() {
    return true;
  }
  let fields = [item.name.as_str(), item.kind.as_str(), item.source.as_str()];
  if regex_mode {
    return regex.is_some_and(|regex| fields.iter().any(|field| regex.is_match(field)));
  }
  if whole_word {
    fields.iter().any(|field| field.eq_ignore_ascii_case(query))
  } else {
    let query = query.to_ascii_lowercase();
    fields
      .iter()
      .any(|field| field.to_ascii_lowercase().contains(&query))
  }
}

fn sort_proxy_items(items: &mut [ProxyDisplayItem], sort: ProxySort) {
  match sort {
    ProxySort::Configuration => {},
    ProxySort::Name => items.sort_by_cached_key(|item| item.name.to_ascii_lowercase()),
    ProxySort::Delay => items.sort_by_key(|item| (item.delay_ms.unwrap_or(u32::MAX), !item.alive)),
  }
}

const fn proxy_unresolved_label(reason: ProxyMemberUnresolvedReason) -> &'static str {
  match reason {
    ProxyMemberUnresolvedReason::Missing => "节点缺失",
    ProxyMemberUnresolvedReason::Ambiguous => "同名 provider 节点不明确",
    ProxyMemberUnresolvedReason::ProviderUnavailable => "provider 元数据不可用",
  }
}

fn paint_tag_row(ui: &Ui, clip: egui::Rect, tags: &[String]) {
  let tokens = theme::tokens(ui);
  let painter = ui.painter().with_clip_rect(clip);
  let mut x = clip.left();
  for tag in tags {
    let galley = painter.layout_no_wrap(
      tag.clone(),
      egui::FontId::proportional(10.0),
      tokens.text_muted,
    );
    let width = galley.size().x + 8.0;
    let tag_rect = egui::Rect::from_min_size(
      egui::pos2(x, clip.center().y - 7.0),
      egui::vec2(width, 14.0),
    );
    painter.rect_stroke(
      tag_rect,
      4.0,
      Stroke::new(1.0, tokens.border),
      egui::StrokeKind::Inside,
    );
    painter.galley(
      egui::pos2(
        tag_rect.left() + 4.0,
        tag_rect.center().y - galley.size().y / 2.0,
      ),
      galley,
      tokens.text_muted,
    );
    x += width + 4.0;
  }
}

fn paint_proxy_tags(ui: &Ui, rect: egui::Rect, item: &ProxyDisplayItem, detailed: bool) {
  let tokens = theme::tokens(ui);
  let clip = egui::Rect::from_min_max(
    egui::pos2(rect.left() + 12.0, rect.center().y + 2.0),
    egui::pos2(rect.right() - 68.0, rect.bottom() - 3.0),
  );
  let painter = ui.painter().with_clip_rect(clip);
  let capabilities = proxy_capability_label(&item.capabilities);
  let mut tags = vec![item.kind.as_str()];
  if detailed {
    tags.push(item.source.as_str());
    if capabilities != "无附加能力" {
      tags.push(&capabilities);
    }
  }
  let mut x = rect.left() + 12.0;
  let y = rect.center().y + 11.0;
  for tag in tags {
    let galley = painter.layout_no_wrap(
      tag.to_string(),
      egui::FontId::proportional(10.0),
      tokens.text_muted,
    );
    let width = galley.size().x + 8.0;
    let tag_rect =
      egui::Rect::from_center_size(egui::pos2(x + width / 2.0, y), egui::vec2(width, 16.0));
    painter.rect_stroke(
      tag_rect,
      4.0,
      Stroke::new(1.0, tokens.text_muted),
      egui::StrokeKind::Inside,
    );
    painter.galley(
      egui::pos2(
        tag_rect.left() + 4.0,
        tag_rect.center().y - galley.size().y / 2.0,
      ),
      galley,
      tokens.text_muted,
    );
    x += width + 4.0;
  }
}

fn proxy_capability_label(capabilities: &ProxyCapabilities) -> String {
  let mut enabled = Vec::new();
  for (available, label) in [
    (capabilities.udp, "UDP"),
    (capabilities.uot, "UoT"),
    (capabilities.xudp, "XUDP"),
    (capabilities.tfo, "TFO"),
    (capabilities.mptcp, "MPTCP"),
    (capabilities.smux, "SMUX"),
  ] {
    if available {
      enabled.push(label);
    }
  }
  if enabled.is_empty() {
    "无附加能力".to_string()
  } else {
    enabled.join(" · ")
  }
}

fn proxy_delay_color(ui: &Ui, delay: Option<u32>, alive: bool) -> Color32 {
  match (alive, delay) {
    (false, _) => ui.visuals().error_fg_color,
    (_, Some(0..=199)) => Color32::from_rgb(38, 162, 105),
    (_, Some(200..=499)) => ui.visuals().warn_fg_color,
    (_, Some(_)) => ui.visuals().error_fg_color,
    _ => ui.visuals().weak_text_color(),
  }
}

const RULE_KINDS: &[&str] = &[
  "DOMAIN",
  "DOMAIN-SUFFIX",
  "DOMAIN-KEYWORD",
  "IP-CIDR",
  "IP-CIDR6",
  "GEOIP",
  "GEOSITE",
  "IP-ASN",
  "PROCESS-NAME",
  "PROCESS-PATH",
  "DST-PORT",
  "SRC-PORT",
  "NETWORK",
  "IN-TYPE",
  "IN-USER",
  "IN-NAME",
  "RULE-SET",
  "AND",
  "OR",
  "NOT",
  "MATCH",
];

fn build_rule_draft(draft: &RuleDraft) -> Result<String, String> {
  let target = draft.target.trim();
  if target.is_empty() {
    return Err("规则目标不能为空。".to_string());
  }
  if draft.kind == "MATCH" {
    return Ok(format!("MATCH,{target}"));
  }
  let payload = draft.payload.trim();
  if payload.is_empty() {
    return Err(format!("{} 规则内容不能为空。", draft.kind));
  }
  let mut rule = format!("{},{payload},{target}", draft.kind);
  if draft.no_resolve && rule_supports_no_resolve(&draft.kind) {
    rule.push_str(",no-resolve");
  }
  Ok(rule)
}

fn rule_payload_hint(kind: &str) -> &'static str {
  match kind {
    "DOMAIN" | "DOMAIN-SUFFIX" | "DOMAIN-KEYWORD" => "域名或关键字",
    "IP-CIDR" | "IP-CIDR6" => "CIDR，例如 10.0.0.0/8",
    "GEOIP" | "GEOSITE" => "国家或 Geo 标识",
    "IP-ASN" => "ASN，例如 13335",
    "PROCESS-NAME" | "PROCESS-PATH" => "进程名或路径",
    "DST-PORT" | "SRC-PORT" => "端口或端口范围",
    "NETWORK" => "tcp 或 udp",
    "IN-TYPE" | "IN-USER" | "IN-NAME" => "入站属性",
    "RULE-SET" => "rule provider 名称",
    "AND" | "OR" | "NOT" => "逻辑子规则表达式",
    _ => "规则内容",
  }
}

fn rule_supports_no_resolve(kind: &str) -> bool {
  matches!(kind, "IP-CIDR" | "IP-CIDR6" | "GEOIP" | "IP-ASN")
}

fn rule_matches(rule: &RuleSnapshot, query: &str) -> bool {
  query.is_empty()
    || [
      rule.kind.as_str(),
      rule.payload.as_str(),
      rule.proxy.as_str(),
    ]
    .iter()
    .any(|field| field.to_ascii_lowercase().contains(query))
}

fn connection_matches(connection: &ConnectionSnapshot, query: &str) -> bool {
  query.is_empty()
    || [
      connection.source.as_str(),
      connection.destination.as_str(),
      connection.host.as_str(),
      connection.process.as_str(),
      connection.rule.as_str(),
      connection.rule_payload.as_str(),
    ]
    .into_iter()
    .chain(connection.chains.iter().map(String::as_str))
    .any(|field| field.to_ascii_lowercase().contains(query))
}

fn sort_connections(connections: &mut [&ConnectionSnapshot], sort: ConnectionSort) {
  match sort {
    ConnectionSort::Traffic => connections.sort_by_key(|connection| {
      std::cmp::Reverse(connection.upload.saturating_add(connection.download))
    }),
    ConnectionSort::Destination => {
      connections.sort_by_key(|connection| connection.destination.to_ascii_lowercase());
    },
    ConnectionSort::Process => {
      connections.sort_by_key(|connection| connection.process.to_ascii_lowercase());
    },
    ConnectionSort::Started => {
      connections.sort_by(|left, right| right.start.cmp(&left.start));
    },
  }
}

fn connection_table_columns(rect: egui::Rect) -> [egui::Rect; 6] {
  let fractions = [0.30, 0.10, 0.18, 0.16, 0.20, 0.06];
  let mut left = rect.left();
  let mut columns = [rect; 6];
  for (index, fraction) in fractions.into_iter().enumerate() {
    let right = if index + 1 == fractions.len() {
      rect.right()
    } else {
      left + rect.width() * fraction
    };
    columns[index] = egui::Rect::from_min_max(
      egui::pos2(left, rect.top()),
      egui::pos2(right, rect.bottom()),
    );
    left = right;
  }
  columns
}

fn connection_detail(ui: &mut Ui, connection: &ConnectionSnapshot) {
  card(ui, "连接详情", |ui| {
    for (label, value) in [
      ("ID", connection.id.as_str()),
      ("源地址", connection.source.as_str()),
      ("目标地址", connection.destination.as_str()),
      ("Host", connection.host.as_str()),
      ("进程", connection.process.as_str()),
      ("网络", connection.network.as_str()),
      ("开始时间", connection.start.as_str()),
      ("规则", connection.rule.as_str()),
      ("规则内容", connection.rule_payload.as_str()),
    ] {
      if !value.is_empty() {
        ui.horizontal_wrapped(|ui| {
          ui.label(RichText::new(label).small().strong());
          ui.label(value);
        });
      }
    }
    if !connection.chains.is_empty() {
      ui.label(format!(
        "代理链：{}",
        connection
          .chains
          .iter()
          .rev()
          .cloned()
          .collect::<Vec<_>>()
          .join(" → ")
      ));
    }
  });
}

fn log_matches(log: &LogSnapshot, query: &str) -> bool {
  query.is_empty()
    || log.level.to_ascii_lowercase().contains(query)
    || log.payload.to_ascii_lowercase().contains(query)
}

fn log_row(ui: &mut Ui, log: &LogSnapshot) {
  let color = match log.level.to_ascii_lowercase().as_str() {
    "error" => ui.visuals().error_fg_color,
    "warning" | "warn" => ui.visuals().warn_fg_color,
    "debug" => ui.visuals().weak_text_color(),
    _ => ui.visuals().text_color(),
  };
  let (rect, response) = ui.allocate_exact_size(
    egui::vec2(ui.available_width(), geometry::LOG_ROW_HEIGHT),
    egui::Sense::click(),
  );
  if response.hovered() {
    ui.painter()
      .rect_filled(rect, 0.0, theme::tokens(ui).surface_raised);
  }
  ui.painter().text(
    egui::pos2(rect.left() + 12.0, rect.top() + 13.0),
    egui::Align2::LEFT_CENTER,
    format!("#{}", log.sequence),
    egui::FontId::proportional(12.0),
    theme::tokens(ui).text_muted,
  );
  ui.painter().text(
    egui::pos2(rect.left() + 62.0, rect.top() + 13.0),
    egui::Align2::LEFT_CENTER,
    log.level.to_ascii_uppercase(),
    egui::FontId::proportional(12.0),
    color,
  );
  ui.painter().text(
    egui::pos2(rect.left() + 12.0, rect.top() + 34.0),
    egui::Align2::LEFT_CENTER,
    &log.payload,
    egui::FontId::proportional(14.0),
    ui.visuals().text_color(),
  );
  ui.painter().line_segment(
    [
      egui::pos2(rect.left() + 12.0, rect.bottom()),
      egui::pos2(rect.right() - 12.0, rect.bottom()),
    ],
    Stroke::new(1.0, theme::tokens(ui).border),
  );
}

const fn stream_log_level_label(level: StreamLogLevel) -> &'static str {
  match level {
    StreamLogLevel::Debug => "Debug",
    StreamLogLevel::Info => "Info",
    StreamLogLevel::Warning => "Warning",
    StreamLogLevel::Error => "Error",
    StreamLogLevel::Silent => "Silent",
  }
}

fn remote_profile_options_editor(ui: &mut Ui, options: &mut RemoteProfileOptions) {
  ui.horizontal(|ui| {
    ui.label("User-Agent");
    ui.add(
      egui::TextEdit::singleline(options.user_agent.get_or_insert_with(String::new))
        .hint_text(concat!("rsclash/", env!("CARGO_PKG_VERSION"))),
    );
  });
  ui.horizontal(|ui| {
    ui.label("HTTP 超时");
    ui.add(
      egui::DragValue::new(&mut options.timeout_seconds)
        .range(1..=300)
        .suffix(" 秒"),
    );
  });
  ui.horizontal(|ui| {
    let mut scheduled = options.update_interval_minutes.is_some();
    if ui.checkbox(&mut scheduled, "定时自动更新").changed() {
      options.update_interval_minutes = scheduled.then_some(1_440);
    }
    if let Some(interval) = options.update_interval_minutes.as_mut() {
      ui.add(
        egui::DragValue::new(interval)
          .range(1..=525_600)
          .suffix(" 分钟"),
      );
    }
  });
  ui.horizontal(|ui| {
    ui.label("下载代理");
    egui::ComboBox::from_id_salt(("profile-download-proxy", ui.id()))
      .selected_text(match options.download_proxy {
        ProfileDownloadProxy::Direct => "直连",
        ProfileDownloadProxy::System => "系统代理",
        ProfileDownloadProxy::Mihomo => "Mihomo 代理",
      })
      .show_ui(ui, |ui| {
        ui.selectable_value(
          &mut options.download_proxy,
          ProfileDownloadProxy::Direct,
          "直连",
        );
        ui.selectable_value(
          &mut options.download_proxy,
          ProfileDownloadProxy::System,
          "系统代理",
        );
        ui.selectable_value(
          &mut options.download_proxy,
          ProfileDownloadProxy::Mihomo,
          "Mihomo 代理",
        );
      });
  });
  ui.checkbox(
    &mut options.accept_invalid_certs,
    "接受无效 TLS 证书（不安全）",
  );
  ui.checkbox(&mut options.allow_auto_update, "允许定时自动更新");
}

impl YamlHighlightCache {
  fn layout(&mut self, source: &str, dark_mode: bool) -> egui::text::LayoutJob {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    let source_hash = hasher.finish();
    if !self.initialized || self.source_hash != source_hash || self.dark_mode != dark_mode {
      self.source_hash = source_hash;
      self.dark_mode = dark_mode;
      self.initialized = true;
      self.job = highlight_yaml(source, dark_mode);
    }
    self.job.clone()
  }
}

fn highlight_yaml(source: &str, dark_mode: bool) -> egui::text::LayoutJob {
  let font_id = egui::FontId::monospace(13.0);
  let normal = egui::TextFormat {
    font_id: font_id.clone(),
    color: if dark_mode {
      Color32::from_rgb(238, 238, 236)
    } else {
      Color32::from_rgb(46, 52, 54)
    },
    ..egui::TextFormat::default()
  };
  let key = egui::TextFormat {
    font_id: font_id.clone(),
    color: if dark_mode {
      Color32::from_rgb(138, 226, 252)
    } else {
      Color32::from_rgb(28, 113, 216)
    },
    ..egui::TextFormat::default()
  };
  let comment = egui::TextFormat {
    font_id,
    color: if dark_mode {
      Color32::from_rgb(143, 161, 179)
    } else {
      Color32::from_rgb(94, 92, 100)
    },
    italics: true,
    ..egui::TextFormat::default()
  };
  let mut job = egui::text::LayoutJob::default();
  for line in source.split_inclusive('\n') {
    let comment_start = yaml_comment_start(line).unwrap_or(line.len());
    let (code, comment_text) = line.split_at(comment_start);
    let indentation = code.len().saturating_sub(code.trim_start().len());
    let trimmed = &code[indentation..];
    let key_end = (!trimmed.starts_with('-'))
      .then(|| trimmed.find(':'))
      .flatten()
      .filter(|index| *index != 0)
      .map(|index| indentation + index + 1);
    if let Some(key_end) = key_end {
      job.append(&code[..indentation], 0.0, normal.clone());
      job.append(&code[indentation..key_end], 0.0, key.clone());
      job.append(&code[key_end..], 0.0, normal.clone());
    } else {
      job.append(code, 0.0, normal.clone());
    }
    if !comment_text.is_empty() {
      job.append(comment_text, 0.0, comment.clone());
    }
  }
  job
}

fn yaml_comment_start(line: &str) -> Option<usize> {
  let mut single_quote = false;
  let mut double_quote = false;
  let mut escaped = false;
  for (index, character) in line.char_indices() {
    if escaped {
      escaped = false;
      continue;
    }
    match character {
      '\\' if double_quote => escaped = true,
      '\'' if !double_quote => single_quote = !single_quote,
      '"' if !single_quote => double_quote = !double_quote,
      '#' if !single_quote && !double_quote => return Some(index),
      _ => {},
    }
  }
  None
}

fn format_update_age(updated_at: u64) -> String {
  let now = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs();
  let Some(age) = now.checked_sub(updated_at) else {
    return "时间异常".to_string();
  };
  match age {
    0..=59 => "刚刚".to_string(),
    60..=3_599 => format!("{} 分钟前", age / 60),
    3_600..=86_399 => format!("{} 小时前", age / 3_600),
    _ => format!("{} 天前", age / 86_400),
  }
}

fn mihomo_connection_pill(ui: &mut Ui, connection: MihomoConnection) {
  let tokens = theme::tokens(ui);
  let (text, color) = match connection {
    MihomoConnection::Offline => ("核心离线", tokens.text_muted),
    MihomoConnection::Connecting => ("正在连接", tokens.warning),
    MihomoConnection::Connected => ("代理运行中", tokens.success),
    MihomoConnection::Degraded => ("连接异常", tokens.danger),
  };
  Frame::new()
    .fill(color.gamma_multiply(0.14))
    .corner_radius(20)
    .inner_margin(egui::Margin::symmetric(10, 5))
    .show(ui, |ui| {
      ui.label(RichText::new(text).small().strong().color(color));
    });
}

fn proxy_mode_label(mode: &ProxyMode) -> &str {
  match mode {
    ProxyMode::Rule => "规则模式",
    ProxyMode::Global => "全局模式",
    ProxyMode::Direct => "直连模式",
    ProxyMode::Unknown(value) => value,
  }
}

const fn tun_capability(core: &CoreState, enabled: bool) -> (&'static str, &'static str, bool) {
  if enabled {
    return ("TUN 已启用", "Mihomo 当前配置已启用 TUN。", true);
  }
  match core {
    CoreState::Running {
      mode: CoreRunMode::Service,
      ..
    } => (
      "TUN 权限可用",
      "核心由受限特权 service 运行，可创建 rsclash TUN 设备。",
      true,
    ),
    CoreState::Running {
      mode: CoreRunMode::Sidecar,
      ..
    } => (
      "TUN 权限不可用",
      "当前使用普通用户 sidecar；安装并使用特权 service 后才能安全启用 TUN。",
      false,
    ),
    _ => (
      "等待核心启动",
      "核心启动后将根据实际运行后端检测 TUN 权限。",
      false,
    ),
  }
}

fn stat_pair(ui: &mut Ui, first_label: &str, first: &str, second_label: &str, second: &str) {
  ui.columns(2, |columns| {
    columns[0].label(RichText::new(first).size(19.0).strong());
    columns[0].label(RichText::new(first_label).small().weak());
    columns[1].label(RichText::new(second).size(19.0).strong());
    columns[1].label(RichText::new(second_label).small().weak());
  });
}

fn empty_state(ui: &mut Ui, title: &str, detail: &str) {
  Frame::new()
    .fill(ui.visuals().faint_bg_color)
    .stroke(Stroke::new(1.0, ui.visuals().window_stroke().color))
    .corner_radius(12)
    .inner_margin(24)
    .show(ui, |ui| {
      ui.set_min_width((ui.available_width() - 1.0).max(240.0));
      ui.vertical_centered(|ui| {
        ui.label(RichText::new(title).size(18.0).strong());
        ui.label(RichText::new(detail).weak());
      });
    });
}

fn format_rate(bytes: u64) -> String {
  format!("{}/s", format_bytes(bytes))
}

fn format_bytes(bytes: u64) -> String {
  const KIB: u64 = 1_024;
  const MIB: u64 = KIB * 1_024;
  const GIB: u64 = MIB * 1_024;
  if bytes >= GIB {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
  } else if bytes >= MIB {
    format!("{:.1} MiB", bytes as f64 / MIB as f64)
  } else if bytes >= KIB {
    format!("{:.1} KiB", bytes as f64 / KIB as f64)
  } else {
    format!("{bytes} B")
  }
}

fn metric_chart(ui: &mut Ui, metrics: &[MetricPoint]) {
  let width = ui.available_width().max(240.0);
  let height = 130.0;
  let (response, painter) = ui.allocate_painter(egui::vec2(width, height), egui::Sense::hover());
  painter.rect_filled(response.rect, 8.0, ui.visuals().extreme_bg_color);
  if metrics.len() < 2 {
    return;
  }
  let traffic_max = metrics
    .iter()
    .map(|point| {
      point
        .upload_bytes_per_second
        .max(point.download_bytes_per_second)
    })
    .max()
    .unwrap_or(1)
    .max(1);
  let memory_max = metrics
    .iter()
    .map(|point| point.memory_bytes)
    .max()
    .unwrap_or(1)
    .max(1);
  let plot = |index: usize, value: u64, maximum: u64| {
    let x = response.rect.left()
      + response.rect.width() * index as f32 / (metrics.len().saturating_sub(1)) as f32;
    let y = response.rect.bottom() - response.rect.height() * value as f32 / maximum as f32;
    egui::pos2(x, y)
  };
  for (values, maximum, color) in [
    (
      metrics
        .iter()
        .map(|point| point.download_bytes_per_second)
        .collect::<Vec<_>>(),
      traffic_max,
      Color32::from_rgb(53, 132, 228),
    ),
    (
      metrics
        .iter()
        .map(|point| point.upload_bytes_per_second)
        .collect::<Vec<_>>(),
      traffic_max,
      Color32::from_rgb(38, 162, 105),
    ),
    (
      metrics
        .iter()
        .map(|point| point.memory_bytes)
        .collect::<Vec<_>>(),
      memory_max,
      Color32::from_rgb(145, 65, 172),
    ),
  ] {
    for index in 1..values.len() {
      painter.line_segment(
        [
          plot(index - 1, values[index - 1], maximum),
          plot(index, values[index], maximum),
        ],
        Stroke::new(1.5, color),
      );
    }
  }
}

fn paint_navigation_icon(painter: &egui::Painter, page: Page, center: egui::Pos2, color: Color32) {
  let stroke = Stroke::new(2.0, color);
  let point = |x: f32, y: f32| center + egui::vec2(x, y);
  match page {
    Page::Home => {
      painter.add(egui::Shape::line(
        vec![point(-9.0, -1.0), point(0.0, -9.0), point(9.0, -1.0)],
        stroke,
      ));
      painter.add(egui::Shape::line(
        vec![
          point(-7.0, -2.0),
          point(-7.0, 9.0),
          point(7.0, 9.0),
          point(7.0, -2.0),
        ],
        stroke,
      ));
    },
    Page::Proxies => {
      for (radius, start) in [(10.0, 0.20), (6.5, 0.28)] {
        let points = (0..=16)
          .map(|index| {
            let angle = std::f32::consts::PI * (start + (1.0 - start * 2.0) * index as f32 / 16.0);
            center + egui::vec2(angle.cos() * radius, -angle.sin() * radius + 2.0)
          })
          .collect::<Vec<_>>();
        painter.add(egui::Shape::line(points, stroke));
      }
      painter.circle_filled(point(0.0, 7.0), 2.2, color);
    },
    Page::Profiles => {
      for y in [-7.0, 0.0, 7.0] {
        let rect = egui::Rect::from_center_size(point(0.0, y), egui::vec2(18.0, 5.0));
        painter.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
        painter.circle_filled(point(-6.0, y), 1.0, color);
      }
    },
    Page::Connections => {
      painter.circle_stroke(center, 10.0, stroke);
      painter.line_segment([point(-10.0, 0.0), point(10.0, 0.0)], stroke);
      painter.line_segment([point(0.0, -10.0), point(0.0, 10.0)], stroke);
      painter.add(egui::Shape::line(
        vec![
          point(-4.0, -9.0),
          point(-2.0, -4.0),
          point(-2.0, 4.0),
          point(-4.0, 9.0),
        ],
        stroke,
      ));
      painter.add(egui::Shape::line(
        vec![
          point(4.0, -9.0),
          point(2.0, -4.0),
          point(2.0, 4.0),
          point(4.0, 9.0),
        ],
        stroke,
      ));
    },
    Page::Rules => {
      painter.line_segment([point(-5.0, -9.0), point(-5.0, 9.0)], stroke);
      painter.line_segment([point(-5.0, -3.0), point(5.0, -3.0)], stroke);
      painter.line_segment([point(5.0, -3.0), point(5.0, 6.0)], stroke);
      for position in [point(-5.0, -9.0), point(-5.0, 9.0), point(5.0, 6.0)] {
        painter.circle_filled(position, 2.2, color);
      }
    },
    Page::Logs => {
      for (y, width) in [(-7.0, 18.0), (0.0, 14.0), (7.0, 18.0)] {
        painter.line_segment(
          [point(-9.0, y), point(-9.0 + width, y)],
          Stroke::new(2.5, color),
        );
      }
    },
    Page::Unlock => {
      let body = egui::Rect::from_center_size(point(0.0, 4.0), egui::vec2(16.0, 13.0));
      painter.rect_stroke(body, 3.0, stroke, egui::StrokeKind::Inside);
      painter.add(egui::Shape::line(
        vec![
          point(-5.0, -2.0),
          point(-5.0, -5.0),
          point(-3.0, -9.0),
          point(0.0, -10.0),
          point(3.0, -9.0),
          point(5.0, -5.0),
          point(5.0, -2.0),
        ],
        stroke,
      ));
    },
    Page::Settings => {
      painter.circle_stroke(center, 6.0, stroke);
      painter.circle_stroke(center, 2.0, stroke);
      for index in 0..8 {
        let angle = std::f32::consts::TAU * index as f32 / 8.0;
        let direction = egui::vec2(angle.cos(), angle.sin());
        painter.line_segment(
          [center + direction * 7.0, center + direction * 10.0],
          Stroke::new(3.0, color),
        );
      }
    },
  }
}

fn header_icon_button(ui: &mut Ui, symbol: &str, tooltip: &str) -> egui::Response {
  ui.add_sized(
    [32.0, 32.0],
    egui::Button::new(RichText::new(symbol).size(17.0)).frame(false),
  )
  .on_hover_text(tooltip)
}

fn proxy_tool_button(
  ui: &Ui,
  rect: egui::Rect,
  id: impl std::hash::Hash + std::fmt::Debug,
  symbol: &str,
  tooltip: &str,
) -> egui::Response {
  let response = ui
    .interact(rect, ui.id().with(id), egui::Sense::click())
    .on_hover_text(tooltip);
  if response.hovered() {
    ui.painter()
      .rect_filled(rect, 15.0, theme::tokens(ui).accent_soft);
  }
  ui.painter().text(
    rect.center(),
    egui::Align2::CENTER_CENTER,
    symbol,
    egui::FontId::proportional(16.0),
    ui.visuals().text_color(),
  );
  response
}

fn paint_navigation_metric(
  painter: &egui::Painter,
  rect: egui::Rect,
  metrics: &[MetricPoint],
  upload: bool,
  color: Color32,
) {
  let values = metrics
    .iter()
    .rev()
    .take(60)
    .map(|point| {
      if upload {
        point.upload_bytes_per_second
      } else {
        point.download_bytes_per_second
      }
    })
    .collect::<Vec<_>>();
  if values.len() < 2 {
    return;
  }
  let max = values.iter().copied().max().unwrap_or(0).max(1) as f32;
  let width_step = rect.width() / (values.len() - 1) as f32;
  let points = values
    .iter()
    .rev()
    .enumerate()
    .map(|(index, value)| {
      egui::pos2(
        rect.left() + index as f32 * width_step,
        rect.bottom() - 2.0 - (*value as f32 / max) * (rect.height() - 5.0),
      )
    })
    .collect::<Vec<_>>();
  painter.add(egui::Shape::line(points, Stroke::new(1.5, color)));
}

fn navigation_metric_row(ui: &mut Ui, symbol: &str, value: &str, color: Color32) {
  ui.allocate_ui_with_layout(
    egui::vec2(ui.available_width(), 24.0),
    Layout::left_to_right(Align::Center),
    |ui| {
      ui.add_space(geometry::NAV_TRAFFIC_HORIZONTAL_PADDING);
      ui.label(RichText::new(symbol).color(color));
      ui.add_space(8.0);
      ui.label(RichText::new(value).color(color));
    },
  );
}

fn enhanced_card(ui: &mut Ui, title: &str, symbol: &str, contents: impl FnOnce(&mut Ui)) {
  let tokens = theme::tokens(ui);
  let width = ui.available_width();
  Frame::new()
    .fill(tokens.surface)
    .corner_radius(geometry::GLOBAL_RADIUS)
    .show(ui, |ui| {
      ui.set_width(width);
      ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
      ui.allocate_ui_with_layout(
        egui::vec2(width, 54.0),
        Layout::left_to_right(Align::Center),
        |ui| {
          ui.add_space(16.0);
          Frame::new()
            .fill(tokens.accent_soft)
            .corner_radius(6)
            .show(ui, |ui| {
              ui.allocate_ui_with_layout(
                egui::Vec2::splat(38.0),
                Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                  ui.label(RichText::new(symbol).size(20.0).color(tokens.accent));
                },
              );
            });
          ui.add_space(12.0);
          ui.label(RichText::new(title).size(18.0).strong());
        },
      );
      ui.painter().line_segment(
        [
          egui::pos2(ui.min_rect().left(), ui.cursor().top()),
          egui::pos2(ui.min_rect().right(), ui.cursor().top()),
        ],
        Stroke::new(1.0, tokens.border),
      );
      Frame::new()
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
          ui.set_min_width((width - 32.0).max(0.0));
          ui.set_min_height(104.0);
          ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
          contents(ui);
        });
    });
}

fn profile_more_card(ui: &mut Ui, title: &str, badge: &str, detail: &str) -> egui::Response {
  let tokens = theme::tokens(ui);
  let width = ui.available_width();
  let shown = Frame::new()
    .fill(tokens.surface)
    .corner_radius(geometry::GLOBAL_RADIUS)
    .inner_margin(egui::Margin::symmetric(16, 8))
    .show(ui, |ui| {
      ui.set_min_width((width - 32.0).max(0.0));
      ui.set_min_height(58.0);
      ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(18.0).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
          Frame::new()
            .stroke(Stroke::new(1.0, tokens.accent))
            .corner_radius(10)
            .inner_margin(egui::Margin::symmetric(7, 2))
            .show(ui, |ui| {
              ui.label(RichText::new(badge).size(11.0).color(tokens.accent));
            });
        });
      });
      ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), geometry::PROFILE_CARD_LINE_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
          ui.label(RichText::new(detail).size(12.0).weak());
        },
      );
    });
  ui.interact(
    shown.response.rect,
    ui.id().with(("profile-more", title)),
    egui::Sense::click(),
  )
  .on_hover_text("双击编辑")
}

fn profile_origin(profile: &ProfileSummary) -> String {
  let Some(location) = profile.location.as_deref() else {
    return profile_source_label(profile.source).to_string();
  };
  if let Some((_, rest)) = location.split_once("://") {
    return rest.split('/').next().unwrap_or(rest).to_string();
  }
  PathBuf::from(location)
    .file_name()
    .and_then(|name| name.to_str())
    .unwrap_or(location)
    .to_string()
}

const fn profile_source_label(source: ProfileSourceKind) -> &'static str {
  match source {
    ProfileSourceKind::Local => "本地配置",
    ProfileSourceKind::Remote => "远程订阅",
    ProfileSourceKind::Merge => "合并配置",
    ProfileSourceKind::Rules => "规则扩展",
    ProfileSourceKind::Proxies => "代理扩展",
    ProfileSourceKind::Groups => "代理组扩展",
    ProfileSourceKind::Other => "扩展配置",
  }
}

fn card(ui: &mut Ui, title: &str, contents: impl FnOnce(&mut Ui)) {
  let tokens = theme::tokens(ui);
  Frame::new()
    .fill(tokens.surface)
    .stroke(Stroke::NONE)
    .corner_radius(geometry::GLOBAL_RADIUS)
    .inner_margin(egui::Margin::symmetric(16, 8))
    .show(ui, |ui| {
      ui.set_min_width((ui.available_width() - 1.0).max(240.0));
      ui.label(RichText::new(title).size(16.0).strong());
      ui.add_space(geometry::MUI_SPACING);
      contents(ui);
    });
}

fn notice_frame(ui: &mut Ui, title: &str, detail: &str, close: impl FnOnce()) {
  let mut should_close = false;
  Frame::new()
    .fill(ui.visuals().error_fg_color.gamma_multiply(0.08))
    .stroke(Stroke::new(
      1.0,
      ui.visuals().error_fg_color.gamma_multiply(0.35),
    ))
    .corner_radius(geometry::GLOBAL_RADIUS)
    .inner_margin(egui::Margin::same(12))
    .show(ui, |ui| {
      ui.set_min_width(280.0);
      ui.horizontal(|ui| {
        ui.vertical(|ui| {
          ui.label(
            RichText::new(title)
              .strong()
              .color(ui.visuals().error_fg_color),
          );
          ui.label(RichText::new(detail).color(ui.visuals().error_fg_color));
        });
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
          should_close = ui
            .add_sized([28.0, 28.0], egui::Button::new("×").frame(false))
            .clicked();
        });
      });
    });
  ui.add_space(geometry::MUI_SPACING);
  if should_close {
    close();
  }
}

fn client_error_message(error: &ClientError) -> String {
  format!("后台命令失败：{error}")
}

#[cfg(test)]
mod tests {
  use rsclash_domain::{CoreChannel, CoreRunMode, CoreState, Page};

  use super::{
    RuleDraft, SequenceEditorKind, build_rule_draft, format_bytes, highlight_yaml,
    parse_sequence_editor, serialize_sequence_editor, tun_capability, yaml_comment_start,
  };

  #[test]
  fn every_page_has_a_non_empty_native_label() {
    assert!(
      Page::ALL
        .iter()
        .all(|page| !page.label().is_empty() && !page.symbol().is_empty())
    );
  }

  #[test]
  fn byte_counts_use_binary_units() {
    assert_eq!(format_bytes(0), "0 B");
    assert_eq!(format_bytes(1_024), "1.0 KiB");
    assert_eq!(format_bytes(5 * 1_024 * 1_024), "5.0 MiB");
  }

  #[test]
  fn yaml_highlighter_keeps_quoted_hashes_in_values() {
    let source = "name: '#value' # comment\n";
    assert_eq!(yaml_comment_start(source), source.rfind('#'));
    assert!(highlight_yaml(source, true).sections.len() >= 3);
  }

  #[test]
  fn tun_capability_reflects_the_actual_core_backend() {
    let service = CoreState::Running {
      mode: CoreRunMode::Service,
      channel: CoreChannel::Stable,
      version: None,
    };
    let sidecar = CoreState::Running {
      mode: CoreRunMode::Sidecar,
      channel: CoreChannel::Stable,
      version: None,
    };

    assert!(tun_capability(&service, false).2);
    assert!(!tun_capability(&sidecar, false).2);
    assert!(tun_capability(&sidecar, true).2);
  }

  #[test]
  fn visual_proxy_editor_preserves_arbitrary_fields() -> Result<(), String> {
    let source = r"
prepend:
  - name: Node A
    type: ss
    plugin-opts:
      mode: websocket
append: []
delete:
  - Old node
";
    let editor = parse_sequence_editor(
      "proxy-uid".to_string(),
      "Proxy extension".to_string(),
      SequenceEditorKind::Proxies,
      source,
    )?;
    let output = serialize_sequence_editor(&editor)?;
    let source =
      serde_yaml_ng::from_str::<serde_yaml_ng::Value>(source).map_err(|error| error.to_string())?;
    let output = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&output)
      .map_err(|error| error.to_string())?;

    assert_eq!(output, source);
    Ok(())
  }

  #[test]
  fn visual_group_editor_rejects_items_without_required_fields() -> Result<(), String> {
    let mut editor = parse_sequence_editor(
      "group-uid".to_string(),
      "Group extension".to_string(),
      SequenceEditorKind::Groups,
      "prepend: []\nappend: []\ndelete: []\n",
    )?;
    editor.append.push("type: select".to_string());

    match serialize_sequence_editor(&editor) {
      Ok(_) => Err("the missing name should fail".to_string()),
      Err(error) => {
        assert!(error.contains("name"));
        Ok(())
      },
    }
  }

  #[test]
  fn visual_rule_builder_handles_match_and_no_resolve() -> Result<(), String> {
    let cidr = build_rule_draft(&RuleDraft {
      kind: "IP-CIDR".to_string(),
      payload: "10.0.0.0/8".to_string(),
      target: "DIRECT".to_string(),
      no_resolve: true,
    })?;
    let final_rule = build_rule_draft(&RuleDraft {
      kind: "MATCH".to_string(),
      payload: String::new(),
      target: "Proxy".to_string(),
      no_resolve: false,
    })?;

    assert_eq!(cidr, "IP-CIDR,10.0.0.0/8,DIRECT,no-resolve");
    assert_eq!(final_rule, "MATCH,Proxy");
    Ok(())
  }
}
