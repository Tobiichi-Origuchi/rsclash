//! Native egui presentation layer. This crate only talks to the application protocol.

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
  AppEvent, AppSnapshot, AppStatus, CoreChannel, CoreRunMode, CoreState, MihomoConnection, Page,
  ProfileDownloadProxy, ProfileQrCode, ProfileSourceKind, ProxyGroupSnapshot, ProxyMode,
  RemoteProfileOptions, SensitiveString, ThemeMode, UiCommand,
};

struct ProfileEditor {
  uid: String,
  name: String,
  content: String,
  dirty: bool,
  highlighter: YamlHighlightCache,
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
  profile_batch_mode: bool,
  selected_profiles: BTreeSet<String>,
  pending_batch_delete: bool,
  editing_profile_options: Option<String>,
  profile_options_edits: BTreeMap<String, RemoteProfileOptions>,
  profile_editor: Option<ProfileEditor>,
  pending_profile_editor_name: Option<(String, String)>,
  pending_editor_close: bool,
}

impl RsClashUi {
  pub fn new(context: &egui::Context, client: AppClient, close_to_tray: bool) -> Self {
    theme::install_styles(context);
    let snapshot = client.current_snapshot();
    theme::apply_preference(context, snapshot.theme);

    Self {
      events: client.subscribe_events(),
      client,
      snapshot,
      applied_theme: None,
      applied_window_visibility: None,
      local_error: None,
      close_to_tray,
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
      profile_batch_mode: false,
      selected_profiles: BTreeSet::new(),
      pending_batch_delete: false,
      editing_profile_options: None,
      profile_options_edits: BTreeMap::new(),
      profile_editor: None,
      pending_profile_editor_name: None,
      pending_editor_close: false,
    }
  }

  /// Synchronize background state without painting. This is called even when the root viewport is hidden.
  pub fn logic(&mut self, context: &egui::Context) {
    if let Some(snapshot) = self.client.take_snapshot_if_changed() {
      self.snapshot = snapshot;
    }

    while let Some(event) = self.events.try_recv() {
      match event {
        AppEvent::ProfileContentLoaded { uid, content } => {
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
        },
        AppEvent::ProfileQrReady(qr) => {
          self.profile_qr = Some(qr);
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
      if self.close_to_tray {
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

  pub fn ui(&mut self, root: &mut Ui) {
    egui::Panel::left("navigation")
      .exact_size(190.0)
      .frame(
        Frame::side_top_panel(root.style())
          .fill(root.visuals().panel_fill)
          .inner_margin(egui::Margin::symmetric(12, 16)),
      )
      .show(root, |ui| self.navigation(ui));

    egui::CentralPanel::default()
      .frame(
        Frame::central_panel(root.style())
          .fill(root.visuals().window_fill())
          .inner_margin(egui::Margin::same(0)),
      )
      .show(root, |ui| {
        self.header(ui);
        ui.separator();
        ScrollArea::vertical()
          .auto_shrink([false, false])
          .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.add_space(20.0);
            ui.horizontal(|ui| {
              ui.add_space(24.0);
              ui.vertical(|ui| {
                ui.set_max_width((ui.available_width() - 24.0).max(320.0));
                self.page(ui);
              });
            });
            ui.add_space(24.0);
          });
      });
  }

  fn navigation(&mut self, ui: &mut Ui) {
    ui.horizontal(|ui| {
      let accent = ui.visuals().selection.bg_fill;
      ui.label(RichText::new("◈").size(28.0).color(accent));
      ui.vertical(|ui| {
        ui.label(RichText::new("rsclash").size(19.0).strong());
        ui.label(RichText::new("Native Mihomo GUI").small().weak());
      });
    });
    ui.add_space(22.0);

    for page in Page::ALL {
      let selected = self.snapshot.page == page;
      let label = format!("{}   {}", page.symbol(), page.label());
      if ui
        .add_sized(
          [ui.available_width(), 40.0],
          egui::Button::new(RichText::new(label).size(15.0))
            .selected(selected)
            .corner_radius(9),
        )
        .clicked()
      {
        self.command(UiCommand::Navigate(page));
      }
      ui.add_space(3.0);
    }

    ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
      ui.label(RichText::new("原生 egui · Mihomo").small().weak());
      ui.label(
        RichText::new(format!("状态版本 {}", self.snapshot.revision))
          .small()
          .weak(),
      );
    });
  }

  fn header(&mut self, ui: &mut Ui) {
    Frame::new()
      .inner_margin(egui::Margin::symmetric(24, 15))
      .show(ui, |ui| {
        ui.horizontal(|ui| {
          ui.heading(self.snapshot.page.label());
          ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("退出").clicked() {
              self.command(UiCommand::Shutdown);
            }
            if self.close_to_tray && ui.button("隐藏到托盘").clicked() {
              self.command(UiCommand::SetWindowVisible(false));
            }
            status_pill(ui, self.snapshot.status);
          });
        });
      });
  }

  fn page(&mut self, ui: &mut Ui) {
    if let Some(error) = self.snapshot.last_error.clone() {
      Frame::new()
        .fill(ui.visuals().error_fg_color.gamma_multiply(0.08))
        .stroke(Stroke::new(
          1.0,
          ui.visuals().error_fg_color.gamma_multiply(0.35),
        ))
        .corner_radius(10)
        .inner_margin(14)
        .show(ui, |ui| {
          ui.label(
            RichText::new(error.title)
              .strong()
              .color(ui.visuals().error_fg_color),
          );
          ui.label(RichText::new(error.detail).color(ui.visuals().error_fg_color));
          if ui.button("关闭").clicked() {
            self.command(UiCommand::ClearError);
          }
        });
      ui.add_space(14.0);
    }

    if let Some(error) = self.local_error.clone() {
      Frame::new()
        .fill(ui.visuals().error_fg_color.gamma_multiply(0.08))
        .stroke(Stroke::new(
          1.0,
          ui.visuals().error_fg_color.gamma_multiply(0.35),
        ))
        .corner_radius(10)
        .inner_margin(14)
        .show(ui, |ui| {
          ui.horizontal(|ui| {
            ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
            if ui.button("关闭").clicked() {
              self.local_error = None;
            }
          });
        });
      ui.add_space(14.0);
    }

    match self.snapshot.page {
      Page::Home => self.home(ui),
      Page::Proxies => self.proxies(ui),
      Page::Profiles => self.profiles(ui),
      Page::Settings => self.settings(ui),
      page => self.placeholder(ui, page),
    }
  }

  fn home(&mut self, ui: &mut Ui) {
    let core = self.snapshot.core.clone();
    let mihomo = self.snapshot.mihomo.clone();
    let system_proxy = self.snapshot.system_proxy.clone();
    let can_enable_system_proxy = system_proxy.available
      && mihomo.connection == MihomoConnection::Connected
      && mihomo.mixed_port.is_some();
    ui.horizontal(|ui| {
      ui.vertical(|ui| {
        ui.label(RichText::new("网络概览").size(24.0).strong());
        ui.label(RichText::new("Mihomo 与系统代理的实时状态").weak());
      });
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui.button("刷新").clicked() {
          self.command(UiCommand::RefreshMihomo);
          self.command(UiCommand::RefreshSystemProxy);
        }
        mihomo_connection_pill(ui, mihomo.connection);
      });
    });
    ui.add_space(18.0);

    ui.columns(2, |columns| {
      card(&mut columns[0], "Mihomo 核心", |ui| {
        self.core_controls(ui, &core);
      });
      card(&mut columns[1], "当前代理", |ui| {
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
        ui.label(
          RichText::new(format!(
            "配置：{}",
            self
              .snapshot
              .profiles
              .current()
              .map_or("默认配置", |profile| profile.name.as_str())
          ))
          .small()
          .weak(),
        );
      });
    });

    ui.add_space(12.0);
    ui.columns(2, |columns| {
      card(&mut columns[0], "实时流量", |ui| {
        stat_pair(
          ui,
          "上传",
          &format_rate(mihomo.traffic.upload_bytes_per_second),
          "下载",
          &format_rate(mihomo.traffic.download_bytes_per_second),
        );
      });
      card(&mut columns[1], "资源使用", |ui| {
        stat_pair(
          ui,
          "内存",
          &format_bytes(mihomo.memory_bytes),
          "连接",
          &mihomo.connection_count.to_string(),
        );
      });
    });

    ui.add_space(12.0);
    card(ui, "出站模式", |ui| {
      self.mode_controls(ui, &mihomo.mode);
      if let Some(error) = mihomo.last_error.as_deref() {
        ui.add_space(8.0);
        ui.label(
          RichText::new(format!("控制器暂时不可用：{error}"))
            .small()
            .color(ui.visuals().warn_fg_color),
        );
      }
    });

    ui.add_space(12.0);
    card(ui, "系统代理", |ui| {
      ui.horizontal(|ui| {
        ui.vertical(|ui| {
          ui.label(
            RichText::new(if system_proxy.enabled {
              "已接管系统代理"
            } else {
              "未接管系统代理"
            })
            .size(18.0)
            .strong(),
          );
          let backend = system_proxy
            .backend
            .as_deref()
            .unwrap_or("正在检测 Linux 后端");
          ui.label(RichText::new(backend).small().weak());
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
          if system_proxy.busy {
            ui.spinner();
          }
          if ui
            .add_enabled(
              !system_proxy.busy && (system_proxy.enabled || can_enable_system_proxy),
              egui::Button::new(if system_proxy.enabled {
                "关闭系统代理"
              } else {
                "启用系统代理"
              }),
            )
            .clicked()
          {
            self.command(UiCommand::SetSystemProxy(!system_proxy.enabled));
          }
        });
      });
      if !system_proxy.available {
        ui.add_space(8.0);
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
      } else if system_proxy.enabled && !system_proxy.applied {
        ui.add_space(8.0);
        ui.label(
          RichText::new("系统设置已在外部发生变化；关闭时仍会恢复启用前的状态。")
            .small()
            .color(ui.visuals().warn_fg_color),
        );
      } else if !can_enable_system_proxy && !system_proxy.enabled {
        ui.add_space(8.0);
        ui.label(
          RichText::new("启动 Mihomo 后即可启用系统代理。")
            .small()
            .weak(),
        );
      }
    });

    ui.add_space(12.0);
    card(ui, "TUN 能力", |ui| {
      let (status, detail, available) = tun_capability(&core, mihomo.tun_enabled);
      ui.label(
        RichText::new(status)
          .size(18.0)
          .strong()
          .color(if available {
            Color32::from_rgb(38, 162, 105)
          } else {
            ui.visuals().warn_fg_color
          }),
      );
      ui.label(RichText::new(detail).small().weak());
      if available && !mihomo.tun_enabled {
        ui.label(
          RichText::new("P6 仅显示权限状态；TUN 配置开关将在设置事务接入后开放。")
            .small()
            .weak(),
        );
      }
    });
  }

  fn proxies(&mut self, ui: &mut Ui) {
    let mihomo = self.snapshot.mihomo.clone();
    ui.horizontal(|ui| {
      ui.vertical(|ui| {
        ui.label(RichText::new("代理选择").size(24.0).strong());
        ui.label(RichText::new("选择出站模式和各代理组的当前节点").weak());
      });
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui.button("刷新").clicked() {
          self.command(UiCommand::RefreshMihomo);
        }
      });
    });
    ui.add_space(16.0);
    card(ui, "出站模式", |ui| {
      self.mode_controls(ui, &mihomo.mode)
    });
    ui.add_space(12.0);

    if mihomo.connection == MihomoConnection::Offline {
      empty_state(
        ui,
        "Mihomo 尚未运行",
        "启动核心后即可读取代理组并选择节点。",
      );
      return;
    }
    if mihomo.groups.is_empty() {
      empty_state(
        ui,
        "没有可用代理组",
        "当前配置尚未提供 Selector、URL-Test 等代理组。",
      );
      return;
    }

    for group in &mihomo.groups {
      self.proxy_group(ui, group);
      ui.add_space(12.0);
    }
  }

  fn proxy_group(&mut self, ui: &mut Ui, group: &ProxyGroupSnapshot) {
    card(ui, &group.name, |ui| {
      ui.horizontal(|ui| {
        ui.label(RichText::new(&group.kind).small().weak());
        if let Some(selected) = group.selected.as_deref() {
          ui.label(RichText::new(format!("当前：{selected}")).small().weak());
        }
      });
      ui.add_space(6.0);
      ui.horizontal_wrapped(|ui| {
        for option in &group.options {
          let selected = group.selected.as_deref() == Some(option.name.as_str());
          let delay = option
            .delay_ms
            .map_or_else(|| "—".to_string(), |delay| format!("{delay} ms"));
          let text = format!("{}  {delay}", option.name);
          let response = ui.add_enabled(
            option.alive || selected,
            egui::Button::new(text).selected(selected),
          );
          if response.clicked() {
            self.command(UiCommand::SelectProxy {
              group: group.name.clone(),
              proxy: option.name.clone(),
            });
          }
        }
      });
    });
  }

  fn profiles(&mut self, ui: &mut Ui) {
    let profiles = self.snapshot.profiles.clone();
    let has_remote = profiles
      .items
      .iter()
      .any(|profile| profile.source == ProfileSourceKind::Remote);
    ui.horizontal(|ui| {
      ui.vertical(|ui| {
        ui.label(RichText::new("订阅与配置").size(24.0).strong());
        ui.label(RichText::new("导入本地 YAML 或远程订阅，并激活为当前运行配置").weak());
      });
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if profiles.busy {
          ui.spinner();
        } else if ui.button("刷新列表").clicked() {
          self.command(UiCommand::RefreshProfiles);
        }
        if ui
          .add_enabled(!profiles.busy && has_remote, egui::Button::new("更新全部"))
          .clicked()
        {
          self.command(UiCommand::UpdateAllProfiles);
        }
        if !profiles.items.is_empty()
          && ui
            .add_enabled(
              !profiles.busy,
              egui::Button::new(if self.profile_batch_mode {
                "完成批量管理"
              } else {
                "批量管理"
              }),
            )
            .clicked()
        {
          self.profile_batch_mode = !self.profile_batch_mode;
          self.selected_profiles.clear();
          self.pending_batch_delete = false;
        }
      });
    });
    ui.add_space(16.0);

    if self.profile_editor.is_some() {
      self.profile_yaml_editor(ui, profiles.busy);
      ui.add_space(16.0);
    }
    if self.profile_qr.is_some() {
      self.profile_qr_viewer(ui);
      ui.add_space(16.0);
    }

    ui.columns(2, |columns| {
      card(&mut columns[0], "导入本地配置", |ui| {
        ui.add(egui::TextEdit::singleline(&mut self.local_profile_name).hint_text("配置名称"));
        ui.add(
          egui::TextEdit::singleline(&mut self.local_profile_path)
            .hint_text("/path/to/profile.yaml"),
        );
        if ui
          .add_enabled(!profiles.busy, egui::Button::new("导入本地文件"))
          .clicked()
        {
          self.command(UiCommand::ImportLocalProfile {
            name: self.local_profile_name.trim().to_string(),
            path: self.local_profile_path.trim().to_string(),
          });
        }
      });
      card(&mut columns[1], "添加远程订阅", |ui| {
        ui.add(egui::TextEdit::singleline(&mut self.remote_profile_name).hint_text("订阅名称"));
        let url_edit = ui.add(
          egui::TextEdit::singleline(&mut self.remote_profile_url)
            .password(true)
            .hint_text("HTTP(S) URL 或 clash:// 深链"),
        );
        if ui.button("从剪贴板粘贴").clicked() {
          url_edit.request_focus();
          ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
        }
        ui.collapsing("下载选项", |ui| {
          remote_profile_options_editor(ui, &mut self.remote_profile_options);
        });
        if ui
          .add_enabled(!profiles.busy, egui::Button::new("下载并导入"))
          .clicked()
        {
          self.command(UiCommand::ImportRemoteProfile {
            name: self.remote_profile_name.trim().to_string(),
            url: self.remote_profile_url.trim().to_string(),
            options: self.remote_profile_options.clone(),
          });
        }
      });
    });
    ui.add_space(12.0);
    card(ui, "文件、拖放与二维码", |ui| {
      ui.label(
        RichText::new("可将 YAML 或 PNG/JPEG 二维码直接拖入窗口；也可以输入二维码图片路径。")
          .small()
          .weak(),
      );
      ui.horizontal(|ui| {
        ui.add(
          egui::TextEdit::singleline(&mut self.qr_profile_name).hint_text("订阅名称（可留空）"),
        );
        ui.add(
          egui::TextEdit::singleline(&mut self.qr_profile_path)
            .hint_text("/path/to/subscription-qr.png"),
        );
        if ui
          .add_enabled(!profiles.busy, egui::Button::new("识别并导入"))
          .clicked()
        {
          self.command(UiCommand::ImportProfileQr {
            name: self.qr_profile_name.trim().to_string(),
            path: self.qr_profile_path.trim().to_string(),
            options: self.remote_profile_options.clone(),
          });
        }
      });
    });
    ui.add_space(16.0);

    if profiles.items.is_empty() {
      empty_state(
        ui,
        "还没有配置",
        "从本地文件或 HTTPS 订阅导入第一个 Mihomo 配置。",
      );
      return;
    }

    if self.profile_batch_mode {
      card(ui, "批量管理", |ui| {
        ui.horizontal_wrapped(|ui| {
          ui.label(format!(
            "已选择 {} / {} 项",
            self.selected_profiles.len(),
            profiles.items.len()
          ));
          if ui
            .add_enabled(!profiles.busy, egui::Button::new("全选"))
            .clicked()
          {
            self
              .selected_profiles
              .extend(profiles.items.iter().map(|profile| profile.uid.clone()));
          }
          if ui
            .add_enabled(
              !profiles.busy && !self.selected_profiles.is_empty(),
              egui::Button::new("清除选择"),
            )
            .clicked()
          {
            self.selected_profiles.clear();
            self.pending_batch_delete = false;
          }
          let delete_label = if self.pending_batch_delete {
            "确认删除选中项"
          } else {
            "删除选中项"
          };
          if ui
            .add_enabled(
              !profiles.busy && !self.selected_profiles.is_empty(),
              egui::Button::new(delete_label),
            )
            .clicked()
          {
            if self.pending_batch_delete {
              self.command(UiCommand::DeleteProfiles {
                uids: self.selected_profiles.iter().cloned().collect(),
              });
              self.selected_profiles.clear();
              self.pending_batch_delete = false;
              self.profile_batch_mode = false;
            } else {
              self.pending_batch_delete = true;
            }
          }
        });
        if self.pending_batch_delete {
          ui.label(RichText::new("此操作会同时删除所选配置文件，无法撤销。").weak());
        }
      });
      ui.add_space(10.0);
    }

    for (profile_index, profile) in profiles.items.iter().enumerate() {
      card(ui, &profile.name, |ui| {
        ui.horizontal(|ui| {
          if self.profile_batch_mode {
            let mut selected = self.selected_profiles.contains(&profile.uid);
            if ui.checkbox(&mut selected, "").changed() {
              if selected {
                self.selected_profiles.insert(profile.uid.clone());
              } else {
                self.selected_profiles.remove(&profile.uid);
              }
              self.pending_batch_delete = false;
            }
          }
          let source = match profile.source {
            ProfileSourceKind::Local => "本地",
            ProfileSourceKind::Remote => "远程订阅",
            ProfileSourceKind::Merge => "合并配置",
            ProfileSourceKind::Rules => "规则扩展",
            ProfileSourceKind::Proxies => "代理扩展",
            ProfileSourceKind::Groups => "代理组扩展",
            ProfileSourceKind::Other => "扩展配置",
          };
          ui.label(RichText::new(source).small().weak());
          if profile.active {
            ui.label(
              RichText::new("当前使用")
                .small()
                .strong()
                .color(Color32::from_rgb(38, 162, 105)),
            );
          }
        });
        if let Some(usage) = profile.usage {
          let used = usage.upload.saturating_add(usage.download);
          ui.label(
            RichText::new(format!(
              "流量：{} / {} · 到期时间：{}",
              format_bytes(used),
              format_bytes(usage.total),
              if usage.expire == 0 {
                "未提供".to_string()
              } else {
                usage.expire.to_string()
              }
            ))
            .small()
            .weak(),
          );
        }
        if let Some(updated_at) = profile.updated_at {
          ui.label(
            RichText::new(format!("最近更新：{}", format_update_age(updated_at)))
              .small()
              .weak(),
          );
        }
        if let Some(home_page) = profile.home_page.as_deref() {
          ui.label(
            RichText::new(format!("订阅主页：{home_page}"))
              .small()
              .weak(),
          );
        }
        if self.profile_batch_mode {
          return;
        }

        if self.renaming_profile.as_deref() == Some(profile.uid.as_str()) {
          let edit = self
            .profile_name_edits
            .entry(profile.uid.clone())
            .or_insert_with(|| profile.name.clone());
          let mut save = false;
          let mut cancel = false;
          ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(edit).hint_text("配置名称"));
            save = ui
              .add_enabled(!profiles.busy, egui::Button::new("保存"))
              .clicked();
            cancel = ui.button("取消").clicked();
          });
          if save {
            let name = self
              .profile_name_edits
              .get(&profile.uid)
              .cloned()
              .unwrap_or_default();
            self.command(UiCommand::RenameProfile {
              uid: profile.uid.clone(),
              name,
            });
            self.renaming_profile = None;
          } else if cancel {
            self.renaming_profile = None;
          }
        }

        if self.editing_profile_options.as_deref() == Some(profile.uid.as_str())
          && let Some(original) = profile.remote_options.as_ref()
        {
          let edit = self
            .profile_options_edits
            .entry(profile.uid.clone())
            .or_insert_with(|| original.clone());
          remote_profile_options_editor(ui, edit);
          let mut save = false;
          let mut cancel = false;
          ui.horizontal(|ui| {
            save = ui
              .add_enabled(!profiles.busy, egui::Button::new("保存下载设置"))
              .clicked();
            cancel = ui.button("取消").clicked();
          });
          if save {
            let options = self
              .profile_options_edits
              .get(&profile.uid)
              .cloned()
              .unwrap_or_default();
            self.command(UiCommand::SetRemoteProfileOptions {
              uid: profile.uid.clone(),
              options,
            });
            self.editing_profile_options = None;
          } else if cancel {
            self.editing_profile_options = None;
          }
        }

        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
          let source_profile = matches!(
            profile.source,
            ProfileSourceKind::Local | ProfileSourceKind::Remote
          );
          if ui
            .add_enabled(
              !profiles.busy && source_profile && !profile.active,
              egui::Button::new(if profile.active {
                "已激活"
              } else {
                "激活"
              }),
            )
            .clicked()
          {
            self.command(UiCommand::ActivateProfile {
              uid: profile.uid.clone(),
            });
          }
          if profile.source == ProfileSourceKind::Remote
            && ui
              .add_enabled(!profiles.busy, egui::Button::new("更新"))
              .clicked()
          {
            self.command(UiCommand::UpdateProfile {
              uid: profile.uid.clone(),
            });
          }
          if let Some(options) = profile.remote_options.as_ref()
            && ui
              .add_enabled(!profiles.busy, egui::Button::new("下载设置"))
              .clicked()
          {
            self
              .profile_options_edits
              .insert(profile.uid.clone(), options.clone());
            self.editing_profile_options = Some(profile.uid.clone());
          }
          if profile.source == ProfileSourceKind::Remote
            && ui
              .add_enabled(!profiles.busy, egui::Button::new("分享二维码"))
              .clicked()
          {
            self.command(UiCommand::RequestProfileQr {
              uid: profile.uid.clone(),
            });
          }
          if ui
            .add_enabled(
              !profiles.busy && profile.source != ProfileSourceKind::Other,
              egui::Button::new("编辑 YAML"),
            )
            .clicked()
          {
            self.open_profile_editor(profile.uid.clone(), profile.name.clone());
          }
          for (label, title, uid) in [
            (
              "扩展配置",
              "合并配置",
              profile.enhancements.merge.as_deref(),
            ),
            (
              "编辑规则",
              "规则扩展",
              profile.enhancements.rules.as_deref(),
            ),
            (
              "编辑代理",
              "代理扩展",
              profile.enhancements.proxies.as_deref(),
            ),
            (
              "编辑代理组",
              "代理组扩展",
              profile.enhancements.groups.as_deref(),
            ),
          ] {
            if let Some(uid) = uid
              && ui
                .add_enabled(!profiles.busy, egui::Button::new(label))
                .clicked()
            {
              self.open_profile_editor(uid.to_string(), format!("{} · {title}", profile.name));
            }
          }
          if ui
            .add_enabled(
              !profiles.busy && profile.source != ProfileSourceKind::Other,
              egui::Button::new("复制"),
            )
            .clicked()
          {
            self.command(UiCommand::DuplicateProfile {
              uid: profile.uid.clone(),
            });
          }
          if ui
            .add_enabled(!profiles.busy, egui::Button::new("重命名"))
            .clicked()
          {
            self
              .profile_name_edits
              .insert(profile.uid.clone(), profile.name.clone());
            self.renaming_profile = Some(profile.uid.clone());
          }
          if ui
            .add_enabled(
              !profiles.busy && profile_index > 0,
              egui::Button::new("上移"),
            )
            .clicked()
          {
            self.command(UiCommand::ReorderProfile {
              uid: profile.uid.clone(),
              new_index: profile_index - 1,
            });
          }
          if ui
            .add_enabled(
              !profiles.busy && profile_index + 1 < profiles.items.len(),
              egui::Button::new("下移"),
            )
            .clicked()
          {
            self.command(UiCommand::ReorderProfile {
              uid: profile.uid.clone(),
              new_index: profile_index + 1,
            });
          }
          let delete_pending = self.pending_profile_delete.as_deref() == Some(profile.uid.as_str());
          if ui
            .add_enabled(
              !profiles.busy,
              egui::Button::new(if delete_pending {
                "确认删除"
              } else {
                "删除"
              }),
            )
            .clicked()
          {
            if delete_pending {
              self.command(UiCommand::DeleteProfiles {
                uids: vec![profile.uid.clone()],
              });
              self.pending_profile_delete = None;
            } else {
              self.pending_profile_delete = Some(profile.uid.clone());
            }
          }
          if delete_pending && ui.button("取消删除").clicked() {
            self.pending_profile_delete = None;
          }
        });
      });
      ui.add_space(10.0);
    }
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
    ui.label(RichText::new("外观").size(20.0).strong());
    ui.label(RichText::new("主题命令会经过异步应用协调器，而不是直接修改全局状态。").weak());
    ui.add_space(14.0);

    card(ui, "颜色模式", |ui| {
      ui.horizontal(|ui| {
        for (mode, label) in [
          (ThemeMode::System, "跟随系统"),
          (ThemeMode::Light, "浅色"),
          (ThemeMode::Dark, "深色"),
        ] {
          if ui
            .selectable_label(self.snapshot.theme == mode, label)
            .clicked()
          {
            self.command(UiCommand::SetTheme(mode));
          }
        }
      });
    });

    ui.add_space(12.0);
    card(ui, "原生 UI 策略", |ui| {
      ui.label("使用语义主题 token，不支持浏览器 CSS 注入。");
      ui.label("默认使用系统标题栏；外部 Mihomo Web UI 将由默认浏览器打开。");
      ui.label("页面不可见时停止高频数据流，避免隐藏的持续渲染与内存增长。");
    });
  }

  fn placeholder(&self, ui: &mut Ui, page: Page) {
    ui.label(RichText::new(page.label()).size(22.0).strong());
    ui.label(RichText::new("页面协议和导航已经就位，业务功能将在后续阶段纵向接入。").weak());
    ui.add_space(18.0);
    card(ui, "分层边界", |ui| {
      ui.label("这个页面只能读取 AppSnapshot 并发送 UiCommand。");
      ui.label("文件、网络、Mihomo 和操作系统调用不会进入 egui render 函数。");
    });
  }

  fn command(&mut self, command: UiCommand) {
    if let Err(error) = self.client.try_command(command) {
      self.local_error = Some(client_error_message(&error));
    }
  }

  fn open_profile_editor(&mut self, uid: String, name: String) {
    self.pending_profile_editor_name = Some((uid.clone(), name));
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

fn status_pill(ui: &mut Ui, status: AppStatus) {
  let (text, color) = match status {
    AppStatus::Booting => ("启动中", Color32::from_rgb(196, 121, 0)),
    AppStatus::Ready => ("后台已就绪", Color32::from_rgb(38, 162, 105)),
    AppStatus::ShuttingDown => ("正在退出", Color32::from_rgb(192, 28, 40)),
  };

  Frame::new()
    .fill(color.gamma_multiply(0.14))
    .corner_radius(20)
    .inner_margin(egui::Margin::symmetric(10, 5))
    .show(ui, |ui| {
      ui.label(RichText::new(text).small().strong().color(color));
    });
}

fn mihomo_connection_pill(ui: &mut Ui, connection: MihomoConnection) {
  let (text, color) = match connection {
    MihomoConnection::Offline => ("核心离线", Color32::from_rgb(119, 118, 123)),
    MihomoConnection::Connecting => ("正在连接", Color32::from_rgb(196, 121, 0)),
    MihomoConnection::Connected => ("代理运行中", Color32::from_rgb(38, 162, 105)),
    MihomoConnection::Degraded => ("连接异常", Color32::from_rgb(192, 28, 40)),
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

fn card(ui: &mut Ui, title: &str, contents: impl FnOnce(&mut Ui)) {
  Frame::new()
    .fill(ui.visuals().faint_bg_color)
    .stroke(Stroke::new(1.0, ui.visuals().window_stroke().color))
    .corner_radius(12)
    .inner_margin(18)
    .show(ui, |ui| {
      ui.set_min_height(92.0);
      ui.set_min_width((ui.available_width() - 1.0).max(240.0));
      ui.label(RichText::new(title).size(15.0).strong());
      ui.add_space(9.0);
      contents(ui);
    });
}

fn client_error_message(error: &ClientError) -> String {
  format!("后台命令失败：{error}")
}

#[cfg(test)]
mod tests {
  use rsclash_domain::{CoreChannel, CoreRunMode, CoreState, Page};

  use super::{format_bytes, highlight_yaml, tun_capability, yaml_comment_start};

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
}
