use std::{fmt, path::Path, sync::Arc};

use async_trait::async_trait;
use rsclash_config::{Error, Result, RuntimeActivator};
use rsclash_mihomo::MihomoApi;

#[derive(Clone)]
pub struct MihomoRuntimeActivator {
  api: Arc<dyn MihomoApi>,
  force_reload: bool,
}

impl MihomoRuntimeActivator {
  #[must_use]
  pub fn new(api: Arc<dyn MihomoApi>) -> Self {
    Self {
      api,
      force_reload: true,
    }
  }

  #[must_use]
  pub const fn with_force_reload(mut self, force_reload: bool) -> Self {
    self.force_reload = force_reload;
    self
  }
}

impl fmt::Debug for MihomoRuntimeActivator {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("MihomoRuntimeActivator")
      .field("force_reload", &self.force_reload)
      .finish_non_exhaustive()
  }
}

#[async_trait]
impl RuntimeActivator for MihomoRuntimeActivator {
  async fn reload(&self, runtime_path: &Path) -> Result<()> {
    let path = runtime_path.to_str().ok_or_else(|| {
      Error::RuntimeActivation(format!(
        "runtime path is not valid UTF-8: {}",
        runtime_path.display()
      ))
    })?;
    self
      .api
      .reload_config(path, self.force_reload)
      .await
      .map_err(|error| Error::RuntimeActivation(error.to_string()))
  }

  async fn restart(&self, _runtime_path: &Path) -> Result<()> {
    self
      .api
      .restart()
      .await
      .map_err(|error| Error::RuntimeActivation(error.to_string()))
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use std::{path::Path, sync::Arc};

  use rsclash_config::RuntimeActivator;
  use rsclash_mihomo::{Error, FakeMihomoApi, MihomoCall};

  use super::MihomoRuntimeActivator;

  #[tokio::test]
  async fn adapter_forwards_reload_and_restart_to_controller() {
    let fake = FakeMihomoApi::default();
    let activator = MihomoRuntimeActivator::new(Arc::new(fake.clone()));

    activator
      .reload(Path::new("/tmp/runtime.yaml"))
      .await
      .expect("reload should succeed");
    activator
      .restart(Path::new("/tmp/runtime.yaml"))
      .await
      .expect("restart should succeed");

    assert_eq!(
      fake.calls().expect("calls should be available"),
      vec![
        MihomoCall::ReloadConfig {
          path: "/tmp/runtime.yaml".to_string(),
          force: true,
        },
        MihomoCall::Restart,
      ]
    );
  }

  #[tokio::test]
  async fn adapter_maps_controller_failures_without_retrying() {
    let fake = FakeMihomoApi::default();
    fake
      .fail_next(Error::Fake("injected".to_string()))
      .expect("failure should be configured");
    let activator = MihomoRuntimeActivator::new(Arc::new(fake.clone()));

    let error = activator
      .reload(Path::new("/tmp/runtime.yaml"))
      .await
      .expect_err("reload should fail");

    assert!(error.to_string().contains("injected"));
    assert_eq!(
      fake.calls().expect("calls should be available"),
      vec![MihomoCall::ReloadConfig {
        path: "/tmp/runtime.yaml".to_string(),
        force: true,
      }]
    );
  }
}
