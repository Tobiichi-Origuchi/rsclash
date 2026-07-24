use std::collections::HashMap;

use futures_util::StreamExt as _;
use rsclash_app::{AppClient, ClientError};
use rsclash_domain::UiCommand;
use tokio::{
  runtime::Handle,
  task::{JoinHandle, JoinSet},
};
use tracing::{debug, warn};
use zbus::{
  Connection, Proxy,
  zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SHORTCUT_INTERFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

pub(crate) struct GlobalShortcutsHandle {
  task: JoinHandle<()>,
}

impl GlobalShortcutsHandle {
  pub(crate) fn spawn(runtime: &Handle, client: AppClient) -> Self {
    Self {
      task: runtime.spawn(supervise(client)),
    }
  }
}

impl Drop for GlobalShortcutsHandle {
  fn drop(&mut self) {
    self.task.abort();
  }
}

async fn supervise(mut client: AppClient) {
  let mut portal_tasks = JoinSet::new();
  let mut enabled = false;
  loop {
    let next = client.current_snapshot().settings.value.global_hotkeys;
    if next != enabled {
      portal_tasks.abort_all();
      enabled = next;
      if enabled {
        portal_tasks.spawn(run_portal(client.clone()));
      }
    }
    tokio::select! {
      result = client.changed() => {
        if result.is_err() {
          return;
        }
      },
      task = portal_tasks.join_next(), if !portal_tasks.is_empty() => {
        if let Some(Err(error)) = task
          && !error.is_cancelled()
        {
          warn!(%error, "global shortcut portal task failed");
        }
      },
    }
  }
}

async fn run_portal(client: AppClient) {
  if let Err(error) = portal_session(&client).await {
    warn!(%error, "XDG global shortcuts are unavailable");
  }
}

async fn portal_session(client: &AppClient) -> Result<(), String> {
  let connection = Connection::session()
    .await
    .map_err(|error| error.to_string())?;
  register_host_application(&connection).await;
  let portal = Proxy::new(
    &connection,
    PORTAL_DESTINATION,
    PORTAL_PATH,
    SHORTCUT_INTERFACE,
  )
  .await
  .map_err(|error| error.to_string())?;

  let options = HashMap::from([
    ("handle_token", Value::from("rsclash_create")),
    ("session_handle_token", Value::from("rsclash_shortcuts")),
  ]);
  let request: OwnedObjectPath = portal
    .call("CreateSession", &(options,))
    .await
    .map_err(|error| error.to_string())?;
  let results = wait_request(&connection, &request).await?;
  let session = results
    .get("session_handle")
    .ok_or_else(|| "shortcut portal did not return a session handle".to_string())?
    .try_clone()
    .map_err(|error| error.to_string())
    .and_then(|value| String::try_from(value).map_err(|error| error.to_string()))?;
  let session = ObjectPath::try_from(session).map_err(|error| error.to_string())?;

  let options = HashMap::from([("handle_token", Value::from("rsclash_bind"))]);
  let request: OwnedObjectPath = portal
    .call(
      "BindShortcuts",
      &(&session, shortcut_descriptions(), "", options),
    )
    .await
    .map_err(|error| error.to_string())?;
  wait_request(&connection, &request).await?;
  debug!("XDG global shortcuts are active");

  let mut activated = portal
    .receive_signal("Activated")
    .await
    .map_err(|error| error.to_string())?;
  while let Some(message) = activated.next().await {
    let (_session, shortcut, _timestamp, _options): (
      OwnedObjectPath,
      String,
      u64,
      HashMap<String, OwnedValue>,
    ) = message
      .body()
      .deserialize()
      .map_err(|error| error.to_string())?;
    dispatch(client, &shortcut)?;
  }
  Ok(())
}

async fn register_host_application(connection: &Connection) {
  let Ok(registry) = Proxy::new(
    connection,
    PORTAL_DESTINATION,
    PORTAL_PATH,
    "org.freedesktop.host.portal.Registry",
  )
  .await
  else {
    return;
  };
  let options: HashMap<&str, Value<'_>> = HashMap::new();
  let _: Result<(), _> = registry
    .call("Register", &("io.github.rsclash", options))
    .await;
}

fn shortcut_descriptions() -> Vec<(&'static str, HashMap<&'static str, Value<'static>>)> {
  [
    ("toggle-window", "显示或隐藏 rsclash"),
    ("toggle-system-proxy", "切换系统代理"),
    ("toggle-tun", "切换 TUN 模式"),
  ]
  .into_iter()
  .map(|(id, description)| {
    (
      id,
      HashMap::from([("description", Value::from(description))]),
    )
  })
  .collect()
}

async fn wait_request(
  connection: &Connection,
  request: &ObjectPath<'_>,
) -> Result<HashMap<String, OwnedValue>, String> {
  let proxy = Proxy::new(connection, PORTAL_DESTINATION, request, REQUEST_INTERFACE)
    .await
    .map_err(|error| error.to_string())?;
  let mut responses = proxy
    .receive_signal("Response")
    .await
    .map_err(|error| error.to_string())?;
  let message = responses
    .next()
    .await
    .ok_or_else(|| "shortcut portal request ended without a response".to_string())?;
  let (response, results): (u32, HashMap<String, OwnedValue>) = message
    .body()
    .deserialize()
    .map_err(|error| error.to_string())?;
  if response == 0 {
    Ok(results)
  } else {
    Err(format!(
      "shortcut portal request was rejected with response {response}"
    ))
  }
}

fn dispatch(client: &AppClient, shortcut: &str) -> Result<(), String> {
  let command = match shortcut {
    "toggle-window" => UiCommand::ToggleWindow,
    "toggle-system-proxy" => {
      UiCommand::SetSystemProxy(!client.current_snapshot().system_proxy.enabled)
    },
    "toggle-tun" => {
      let mut settings = client.current_snapshot().settings.value.clone();
      settings.tun_enabled = !settings.tun_enabled;
      UiCommand::ApplySettings(Box::new(settings))
    },
    _ => return Ok(()),
  };
  client
    .try_command(command)
    .map_err(|error: ClientError| error.to_string())
}
