use std::error::Error;

use rsclash_app::AppClient;
use rsclash_domain::UiCommand;
use tray_icon::{
  Icon, TrayIcon, TrayIconBuilder,
  menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

#[cfg(target_os = "linux")]
use std::{
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
  },
  thread,
  time::Duration,
};

pub struct TrayHandle {
  #[cfg(not(target_os = "linux"))]
  _tray: TrayIcon,
  #[cfg(target_os = "linux")]
  stop: Arc<AtomicBool>,
  #[cfg(target_os = "linux")]
  thread: Option<thread::JoinHandle<()>>,
}

impl TrayHandle {
  pub fn new(client: AppClient) -> Result<Self, Box<dyn Error + Send + Sync>> {
    #[cfg(target_os = "linux")]
    {
      Self::new_linux(client)
    }

    #[cfg(not(target_os = "linux"))]
    {
      Ok(Self {
        _tray: build_tray(client)?,
      })
    }
  }

  #[cfg(target_os = "linux")]
  fn new_linux(client: AppClient) -> Result<Self, Box<dyn Error + Send + Sync>> {
    const INIT_TIMEOUT: Duration = Duration::from_secs(3);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);
    let (init_tx, init_rx) = mpsc::sync_channel(1);

    let thread = thread::Builder::new()
      .name("rsclash-tray".to_owned())
      .spawn(move || {
        if let Err(error) = gtk::init() {
          let _ = init_tx.send(Err(format!("failed to initialize GTK: {error}")));
          return;
        }

        let tray = match build_tray(client) {
          Ok(tray) => tray,
          Err(error) => {
            let _ = init_tx.send(Err(error.to_string()));
            return;
          },
        };

        let main_loop = gtk::glib::MainLoop::new(None, false);
        let loop_for_timer = main_loop.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
          if stop_for_thread.load(Ordering::Acquire) {
            loop_for_timer.quit();
            gtk::glib::ControlFlow::Break
          } else {
            gtk::glib::ControlFlow::Continue
          }
        });

        if init_tx.send(Ok(())).is_err() {
          return;
        }

        main_loop.run();
        drop(tray);
        clear_menu_handler();
      })?;

    match init_rx.recv_timeout(INIT_TIMEOUT) {
      Ok(Ok(())) => Ok(Self {
        stop,
        thread: Some(thread),
      }),
      Ok(Err(message)) => {
        let _ = thread.join();
        Err(boxed_error(message))
      },
      Err(error) => {
        stop.store(true, Ordering::Release);
        let _ = thread.join();
        Err(boxed_error(format!(
          "timed out while initializing the Linux tray: {error}"
        )))
      },
    }
  }
}

impl Drop for TrayHandle {
  fn drop(&mut self) {
    #[cfg(target_os = "linux")]
    {
      self.stop.store(true, Ordering::Release);
      if let Some(thread) = self.thread.take() {
        let _ = thread.join();
      }
    }

    #[cfg(not(target_os = "linux"))]
    clear_menu_handler();
  }
}

fn build_tray(client: AppClient) -> Result<TrayIcon, Box<dyn Error + Send + Sync>> {
  let menu = Menu::new();
  let toggle = MenuItem::new("显示或隐藏 rsclash", true, None);
  let quit = MenuItem::new("退出", true, None);
  let toggle_id = toggle.id().clone();
  let quit_id = quit.id().clone();

  menu.append(&toggle)?;
  menu.append(&PredefinedMenuItem::separator())?;
  menu.append(&quit)?;

  MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
    if event.id == toggle_id {
      let _ = client.try_command(UiCommand::ToggleWindow);
    } else if event.id == quit_id {
      let _ = client.try_command(UiCommand::Shutdown);
    }
  }));

  TrayIconBuilder::new()
    .with_menu(Box::new(menu))
    .with_tooltip("rsclash · Native Mihomo GUI")
    .with_icon(app_icon()?)
    .build()
    .map_err(Into::into)
}

fn clear_menu_handler() {
  MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
}

#[cfg(target_os = "linux")]
fn boxed_error(message: String) -> Box<dyn Error + Send + Sync> {
  Box::new(std::io::Error::other(message))
}

fn app_icon() -> Result<Icon, tray_icon::BadIcon> {
  const SIZE: u32 = 32;
  let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);

  for y in 0..SIZE {
    for x in 0..SIZE {
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
      rgba.extend_from_slice(&[red, green, blue, alpha]);
    }
  }

  Icon::from_rgba(rgba, SIZE, SIZE)
}
