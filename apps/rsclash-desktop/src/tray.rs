use std::sync::LazyLock;

use ksni::{
  blocking::TrayMethods as _,
  menu::{MenuItem, StandardItem},
};
use rsclash_app::AppClient;
use rsclash_domain::UiCommand;
use tracing::{debug, warn};

const ICON_SIZE: i32 = 32;
static APP_ICON: LazyLock<ksni::Icon> = LazyLock::new(app_icon);

pub(crate) struct TrayHandle {
  handle: Option<ksni::blocking::Handle<AppTray>>,
}

impl TrayHandle {
  pub(crate) fn new(client: AppClient) -> Result<Self, ksni::Error> {
    let handle = AppTray { client }.spawn()?;
    Ok(Self {
      handle: Some(handle),
    })
  }

  pub(crate) fn shutdown(&mut self) {
    if let Some(handle) = self.handle.take() {
      handle.shutdown().wait();
    }
  }
}

impl Drop for TrayHandle {
  fn drop(&mut self) {
    self.shutdown();
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
    vec![
      StandardItem {
        label: "显示或隐藏 rsclash".to_owned(),
        icon_name: "view-restore-symbolic".to_owned(),
        activate: Box::new(|tray: &mut Self| tray.send(UiCommand::ToggleWindow)),
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
  use super::{APP_ICON, ICON_SIZE};

  #[test]
  fn icon_uses_argb_network_byte_order() {
    assert_eq!(APP_ICON.width, ICON_SIZE);
    assert_eq!(APP_ICON.height, ICON_SIZE);
    assert_eq!(APP_ICON.data.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
    assert_eq!(&APP_ICON.data[..4], &[0, 0, 0, 0]);

    let center = ((16 * ICON_SIZE + 16) * 4) as usize;
    assert_eq!(&APP_ICON.data[center..center + 4], &[255, 28, 113, 216]);
  }
}
