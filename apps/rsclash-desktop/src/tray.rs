use std::sync::LazyLock;

use ksni::{
  TrayMethods as _,
  menu::{CheckmarkItem, MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu},
};
use rsclash_app::AppClient;
use rsclash_domain::{MihomoConnection, ProxyMode, UiCommand};
use tokio::runtime::Handle as RuntimeHandle;
use tracing::{debug, warn};

const ICON_SIZE: i32 = 32;
static APP_ICON: LazyLock<ksni::Icon> = LazyLock::new(app_icon);

pub(crate) struct TrayHandle {
  handle: Option<ksni::Handle<AppTray>>,
}

impl TrayHandle {
  pub(crate) fn new(client: AppClient, runtime: &RuntimeHandle) -> Result<Self, ksni::Error> {
    let handle = runtime.block_on(AppTray { client }.spawn())?;
    Ok(Self {
      handle: Some(handle),
    })
  }

  pub(crate) fn shutdown(&mut self, runtime: &RuntimeHandle) {
    if let Some(handle) = self.handle.take() {
      runtime.block_on(handle.shutdown());
    }
  }
}

impl Drop for TrayHandle {
  fn drop(&mut self) {
    if let Some(handle) = self.handle.take() {
      drop(handle.shutdown());
    }
  }
}

struct AppTray {
  client: AppClient,
}

impl AppTray {
  fn send(&self, command: UiCommand) {
    debug!(?command, "dispatching system tray command");
    if let Err(error) = self.client.try_command(command) {
      warn!(%error, "failed to dispatch system tray command");
    }
  }
}

impl ksni::Tray for AppTray {
  fn id(&self) -> String {
    "rsclash".to_owned()
  }

  fn title(&self) -> String {
    "rsclash".to_owned()
  }

  fn activate(&mut self, _x: i32, _y: i32) {
    self.send(UiCommand::ToggleWindow);
  }

  fn icon_pixmap(&self) -> Vec<ksni::Icon> {
    vec![APP_ICON.clone()]
  }

  fn tool_tip(&self) -> ksni::ToolTip {
    ksni::ToolTip {
      icon_pixmap: self.icon_pixmap(),
      title: self.title(),
      description: "Native Mihomo GUI".to_owned(),
      ..Default::default()
    }
  }

  fn menu(&self) -> Vec<MenuItem<Self>> {
    let snapshot = self.client.current_snapshot();
    let system_proxy = &snapshot.system_proxy;
    let mihomo_ready = snapshot.mihomo.connection == MihomoConnection::Connected;
    let can_toggle_system_proxy = system_proxy.enabled
      || (system_proxy.available && mihomo_ready && snapshot.mihomo.mixed_port.is_some());
    let current_mode = snapshot.mihomo.mode.clone();
    vec![
      StandardItem {
        label: "显示或隐藏 rsclash".to_owned(),
        icon_name: "view-restore-symbolic".to_owned(),
        activate: Box::new(|tray: &mut Self| tray.send(UiCommand::ToggleWindow)),
        ..Default::default()
      }
      .into(),
      CheckmarkItem {
        label: "系统代理".to_owned(),
        enabled: can_toggle_system_proxy && !system_proxy.busy,
        checked: system_proxy.enabled,
        activate: Box::new(|tray: &mut Self| {
          let enabled = tray.client.current_snapshot().system_proxy.enabled;
          tray.send(UiCommand::SetSystemProxy(!enabled));
        }),
        ..Default::default()
      }
      .into(),
      SubMenu {
        label: format!("代理模式：{}", proxy_mode_label(&current_mode)),
        enabled: mihomo_ready,
        submenu: vec![
          RadioGroup {
            selected: proxy_mode_index(&current_mode),
            select: Box::new(|tray: &mut Self, selected| {
              if let Some(mode) = proxy_mode_from_index(selected) {
                tray.send(UiCommand::SetProxyMode(mode));
              }
            }),
            options: vec![
              RadioItem {
                label: "规则".to_owned(),
                ..Default::default()
              },
              RadioItem {
                label: "全局".to_owned(),
                ..Default::default()
              },
              RadioItem {
                label: "直连".to_owned(),
                ..Default::default()
              },
            ],
          }
          .into(),
        ],
        ..Default::default()
      }
      .into(),
      MenuItem::Separator,
      StandardItem {
        label: "退出".to_owned(),
        icon_name: "application-exit-symbolic".to_owned(),
        activate: Box::new(|tray: &mut Self| tray.send(UiCommand::Shutdown)),
        ..Default::default()
      }
      .into(),
    ]
  }
}

const fn proxy_mode_index(mode: &ProxyMode) -> usize {
  match mode {
    ProxyMode::Rule | ProxyMode::Unknown(_) => 0,
    ProxyMode::Global => 1,
    ProxyMode::Direct => 2,
  }
}

const fn proxy_mode_from_index(index: usize) -> Option<ProxyMode> {
  match index {
    0 => Some(ProxyMode::Rule),
    1 => Some(ProxyMode::Global),
    2 => Some(ProxyMode::Direct),
    _ => None,
  }
}

const fn proxy_mode_label(mode: &ProxyMode) -> &str {
  match mode {
    ProxyMode::Rule => "规则",
    ProxyMode::Global => "全局",
    ProxyMode::Direct => "直连",
    ProxyMode::Unknown(_) => "未知",
  }
}

fn app_icon() -> ksni::Icon {
  let mut data = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);

  for y in 0..ICON_SIZE {
    for x in 0..ICON_SIZE {
      let dx = x as f32 - 15.5;
      let dy = y as f32 - 15.5;
      let inside = dx.mul_add(dx, dy * dy) <= 14.5_f32.powi(2);
      let mark = inside && ((x > 8 && x < 13) || (x > 18 && x < 23)) && y > 8 && y < 24;
      let (red, green, blue, alpha) = if mark {
        (255, 255, 255, 255)
      } else if inside {
        (28, 113, 216, 255)
      } else {
        (0, 0, 0, 0)
      };
      let pixel: [u8; 4] = (alpha, red, green, blue).into();
      data.extend_from_slice(&pixel);
    }
  }

  ksni::Icon {
    width: ICON_SIZE,
    height: ICON_SIZE,
    data,
  }
}

#[cfg(test)]
mod tests {
  use rsclash_domain::ProxyMode;

  use super::{APP_ICON, ICON_SIZE, proxy_mode_from_index, proxy_mode_index, proxy_mode_label};

  #[test]
  fn icon_uses_argb_network_byte_order() {
    assert_eq!(APP_ICON.width, ICON_SIZE);
    assert_eq!(APP_ICON.height, ICON_SIZE);
    assert_eq!(APP_ICON.data.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
    assert_eq!(&APP_ICON.data[..4], &[0, 0, 0, 0]);

    let center = ((16 * ICON_SIZE + 16) * 4) as usize;
    assert_eq!(&APP_ICON.data[center..center + 4], &[255, 28, 113, 216]);
  }

  #[test]
  fn proxy_modes_have_stable_tray_indices() {
    for (index, mode, label) in [
      (0, ProxyMode::Rule, "规则"),
      (1, ProxyMode::Global, "全局"),
      (2, ProxyMode::Direct, "直连"),
    ] {
      assert_eq!(proxy_mode_index(&mode), index);
      assert_eq!(proxy_mode_from_index(index), Some(mode.clone()));
      assert_eq!(proxy_mode_label(&mode), label);
    }
    assert_eq!(proxy_mode_from_index(3), None);
  }
}
