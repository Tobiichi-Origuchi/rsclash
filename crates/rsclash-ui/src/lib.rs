//! Native egui presentation layer. This crate only talks to the application protocol.

mod theme;

use std::sync::Arc;

use egui::{Align, Color32, Frame, Layout, RichText, ScrollArea, Stroke, Ui};
use rsclash_app::{AppClient, AppEventReceiver, ClientError};
use rsclash_domain::{
  AppSnapshot, AppStatus, CoreChannel, CoreRunMode, CoreState, MihomoConnection, Page,
  ProfileSourceKind, ProxyGroupSnapshot, ProxyMode, ThemeMode, UiCommand,
};

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
    }
  }

  /// Synchronize background state without painting. This is called even when the root viewport is hidden.
  pub fn logic(&mut self, context: &egui::Context) {
    if let Some(snapshot) = self.client.take_snapshot_if_changed() {
      self.snapshot = snapshot;
    }

    while self.events.try_recv().is_some() {}

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
    ui.horizontal(|ui| {
      ui.vertical(|ui| {
        ui.label(RichText::new("网络概览").size(24.0).strong());
        ui.label(RichText::new("Mihomo 与系统代理的实时状态").weak());
      });
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui.button("刷新").clicked() {
          self.command(UiCommand::RefreshMihomo);
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
    ui.horizontal(|ui| {
      ui.vertical(|ui| {
        ui.label(RichText::new("订阅与配置").size(24.0).strong());
        ui.label(RichText::new("导入本地 YAML 或远程订阅，并激活为当前运行配置").weak());
      });
      ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if profiles.busy {
          ui.spinner();
        } else if ui.button("刷新").clicked() {
          self.command(UiCommand::RefreshProfiles);
        }
      });
    });
    ui.add_space(16.0);

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
        ui.add(
          egui::TextEdit::singleline(&mut self.remote_profile_url)
            .password(true)
            .hint_text("https://example.com/subscription"),
        );
        if ui
          .add_enabled(!profiles.busy, egui::Button::new("下载并导入"))
          .clicked()
        {
          self.command(UiCommand::ImportRemoteProfile {
            name: self.remote_profile_name.trim().to_string(),
            url: self.remote_profile_url.trim().to_string(),
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

    for profile in &profiles.items {
      card(ui, &profile.name, |ui| {
        ui.horizontal(|ui| {
          let source = match profile.source {
            ProfileSourceKind::Local => "本地",
            ProfileSourceKind::Remote => "远程订阅",
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
          ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
              .add_enabled(
                !profiles.busy && !profile.active,
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
          });
        });
      });
      ui.add_space(10.0);
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
  use rsclash_domain::Page;

  use super::format_bytes;

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
}
