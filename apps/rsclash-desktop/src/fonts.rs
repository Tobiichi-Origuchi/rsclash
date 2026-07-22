use std::{path::Path, sync::Arc};

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};
use tracing::{debug, info};

pub(crate) fn install_system_cjk_font(context: &egui::Context) {
  let Some(path) = font_candidates().into_iter().find(|path| path.exists()) else {
    debug!("no system CJK fallback font was found");
    return;
  };

  let Ok(bytes) = std::fs::read(path) else {
    debug!(path = %path.display(), "failed to read the system CJK fallback font");
    return;
  };

  let font_name = "rsclash-system-cjk".to_owned();
  let mut fonts = FontDefinitions::default();
  fonts
    .font_data
    .insert(font_name.clone(), Arc::new(FontData::from_owned(bytes)));

  for family in [FontFamily::Proportional, FontFamily::Monospace] {
    if let Some(fallbacks) = fonts.families.get_mut(&family) {
      fallbacks.push(font_name.clone());
    }
  }

  context.set_fonts(fonts);
  info!(path = %path.display(), "installed system CJK fallback font");
}

fn font_candidates() -> Vec<&'static Path> {
  if cfg!(target_os = "windows") {
    vec![
      Path::new(r"C:\Windows\Fonts\msyh.ttc"),
      Path::new(r"C:\Windows\Fonts\meiryo.ttc"),
      Path::new(r"C:\Windows\Fonts\malgun.ttf"),
    ]
  } else if cfg!(target_os = "macos") {
    vec![
      Path::new("/System/Library/Fonts/PingFang.ttc"),
      Path::new("/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc"),
    ]
  } else {
    vec![
      Path::new("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"),
      Path::new("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
      Path::new("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc"),
    ]
  }
}
