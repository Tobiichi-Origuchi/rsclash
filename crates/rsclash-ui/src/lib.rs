//! Native egui presentation layer. This crate only talks to the application protocol.

mod theme;

use std::sync::Arc;

use egui::{Align, Color32, Frame, Layout, RichText, ScrollArea, Stroke, Ui};
use rsclash_app::{AppClient, ClientError};
use rsclash_domain::{AppEvent, AppSnapshot, AppStatus, CoreState, Page, ThemeMode, UiCommand};
use tokio::sync::broadcast;

pub struct RsClashUi {
  client: AppClient,
  events: broadcast::Receiver<AppEvent>,
  snapshot: Arc<AppSnapshot>,
  applied_theme: Option<ThemeMode>,
  applied_window_visibility: Option<bool>,
  last_event: Option<AppEvent>,
  local_error: Option<String>,
  close_to_tray: bool,
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
      last_event: None,
      local_error: None,
      close_to_tray,
    }
  }

  /// Synchronize background state without painting. This is called even when the root viewport is hidden.
  pub fn logic(&mut self, context: &egui::Context) {
    if let Some(snapshot) = self.client.take_snapshot_if_changed() {
      self.snapshot = snapshot;
    }

    loop {
      match self.events.try_recv() {
        Ok(event) => self.last_event = Some(event),
        Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
        Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
          break;
        },
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
      ui.label(RichText::new("P1/P2 技术骨架").small().weak());
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
      Page::Settings => self.settings(ui),
      page => self.placeholder(ui, page),
    }
  }

  fn home(&mut self, ui: &mut Ui) {
    ui.label(
      RichText::new("一个不依赖 WebView 的 Mihomo 原生桌面壳")
        .size(22.0)
        .strong(),
    );
    ui.label(RichText::new("当前阶段验证 egui、Tokio 状态桥接、系统主题和事件驱动重绘。").weak());
    ui.add_space(18.0);

    ui.columns(2, |columns| {
      card(&mut columns[0], "Mihomo 核心", |ui| {
        match &self.snapshot.core {
          CoreState::Stopped => {
            ui.label(RichText::new("尚未接入").size(18.0).strong());
            ui.label(RichText::new("将在 P3–P5 接入本地 IPC 与生命周期").weak());
          },
          state => {
            ui.label(RichText::new(format!("{state:?}")).size(18.0));
          },
        }
      });
      card(&mut columns[1], "应用协调器", |ui| {
        ui.label(RichText::new("运行正常").size(18.0).strong());
        ui.label(RichText::new(format!("Snapshot revision {}", self.snapshot.revision)).weak());
      });
    });

    ui.add_space(12.0);
    card(ui, "P1/P2 验证范围", |ui| {
      ui.label("• eframe 0.35 + Glow 原生渲染");
      ui.label("• Tokio 有界命令通道与 watch 快照");
      ui.label("• 后台状态变化主动唤醒 egui");
      ui.label("• GTK/Adwaita 风格语义色板和系统主题");
      ui.label("• 窗口持久化、托盘隐藏与确定性退出");
      if let Some(event) = &self.last_event {
        ui.add_space(8.0);
        ui.label(RichText::new(format!("最近事件：{event:?}")).small().weak());
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

  fn placeholder(&mut self, ui: &mut Ui, page: Page) {
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

  #[test]
  fn every_page_has_a_non_empty_native_label() {
    assert!(
      Page::ALL
        .iter()
        .all(|page| !page.label().is_empty() && !page.symbol().is_empty())
    );
  }
}
