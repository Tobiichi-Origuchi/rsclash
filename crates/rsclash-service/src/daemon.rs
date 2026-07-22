use rsclash_core::CoreHandle;

use async_trait::async_trait;

use crate::{
  RemoteError, RemoteErrorCode, ResponseData, ServiceCommand, ServiceRequestHandler, ServiceStatus,
};

pub struct CoreServiceHandler {
  core: CoreHandle,
}

impl CoreServiceHandler {
  pub const fn new(core: CoreHandle) -> Self {
    Self { core }
  }

  fn lifecycle_error(error: impl ToString) -> RemoteError {
    RemoteError::new(RemoteErrorCode::Lifecycle, error.to_string())
  }
}

#[async_trait]
impl ServiceRequestHandler for CoreServiceHandler {
  async fn handle(&self, command: ServiceCommand) -> Result<ResponseData, RemoteError> {
    match command {
      ServiceCommand::Ping => Ok(ResponseData::Pong {
        service_version: env!("CARGO_PKG_VERSION").to_string(),
      }),
      ServiceCommand::Status => Ok(ResponseData::Status(ServiceStatus {
        service_version: env!("CARGO_PKG_VERSION").to_string(),
        core: (*self.core.current_state()).clone(),
      })),
      ServiceCommand::StartCore { channel } => self
        .core
        .start(channel)
        .await
        .map(ResponseData::CoreState)
        .map_err(Self::lifecycle_error),
      ServiceCommand::StopCore => self
        .core
        .stop()
        .await
        .map(ResponseData::CoreState)
        .map_err(Self::lifecycle_error),
      ServiceCommand::ReloadCore => self
        .core
        .reload()
        .await
        .map(ResponseData::CoreState)
        .map_err(Self::lifecycle_error),
    }
  }
}
