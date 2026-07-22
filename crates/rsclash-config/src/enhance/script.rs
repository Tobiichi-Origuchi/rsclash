use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use boa_engine::{Context, Source};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_yaml_ng::Mapping;

use crate::{Error, Result, ScriptLog};

use super::{ScriptExecutor, ScriptLayer, ScriptOutput, lowercase_mapping};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScriptLimits {
    pub max_logs: usize,
    pub max_log_bytes: usize,
    pub max_json_bytes: usize,
    pub max_script_bytes: usize,
    pub max_profile_name_bytes: usize,
    pub max_loop_iterations: u64,
    pub timeout: Duration,
}

impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            max_logs: 1_000,
            max_log_bytes: 1024 * 1024,
            max_json_bytes: 10 * 1024 * 1024,
            max_script_bytes: 1024 * 1024,
            max_profile_name_bytes: 1024,
            max_loop_iterations: 10_000_000,
            timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct BoaScriptExecutor {
    limits: ScriptLimits,
}

impl BoaScriptExecutor {
    #[must_use]
    pub const fn new(limits: ScriptLimits) -> Self {
        Self { limits }
    }

    #[must_use]
    pub const fn limits(&self) -> ScriptLimits {
        self.limits
    }
}

impl ScriptExecutor for BoaScriptExecutor {
    fn execute(
        &self,
        script: &ScriptLayer,
        config: &Mapping,
        profile_name: &str,
    ) -> Result<ScriptOutput> {
        validate_input(script, config, profile_name, self.limits)?;

        let script = script.clone();
        let config = lowercase_mapping(config);
        let profile_name = profile_name.to_string();
        let limits = self.limits;
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name(format!("rsclash-script-{}", thread_label(&script.id)))
            .spawn(move || {
                let result = execute_inner(&script, &config, &profile_name, limits);
                let _ignored = sender.send(result);
            })
            .map_err(|error| {
                Error::ScriptExecution(format!("failed to start script worker: {error}"))
            })?;

        let started = Instant::now();
        match receiver.recv_timeout(limits.timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(Error::ScriptExecution(format!(
                "execution timed out after {:?}",
                limits.timeout
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::ScriptExecution(format!(
                "script worker stopped after {:?}",
                started.elapsed()
            ))),
        }
    }
}

#[derive(Deserialize)]
struct ExecutionEnvelope {
    config: Option<JsonValue>,
    logs: Vec<(String, String)>,
    error: Option<String>,
}

fn validate_input(
    script: &ScriptLayer,
    config: &Mapping,
    profile_name: &str,
    limits: ScriptLimits,
) -> Result<()> {
    if script.source.len() > limits.max_script_bytes {
        return Err(Error::ScriptExecution(format!(
            "script exceeds {} bytes",
            limits.max_script_bytes
        )));
    }
    if profile_name.len() > limits.max_profile_name_bytes {
        return Err(Error::ScriptExecution(format!(
            "profile name exceeds {} bytes",
            limits.max_profile_name_bytes
        )));
    }
    let config_size = serde_json::to_vec(config)
        .map_err(|error| Error::ScriptExecution(format!("failed to encode config: {error}")))?
        .len();
    if config_size > limits.max_json_bytes {
        return Err(Error::ScriptExecution(format!(
            "configuration exceeds {} bytes",
            limits.max_json_bytes
        )));
    }
    Ok(())
}

fn execute_inner(
    script: &ScriptLayer,
    config: &Mapping,
    profile_name: &str,
    limits: ScriptLimits,
) -> Result<ScriptOutput> {
    let config_json = serde_json::to_string(config)
        .map_err(|error| Error::ScriptExecution(format!("failed to encode config: {error}")))?;
    let profile_json = serde_json::to_string(profile_name).map_err(|error| {
        Error::ScriptExecution(format!("failed to encode profile name: {error}"))
    })?;
    let code = build_program(&script.source, &config_json, &profile_json, limits);

    let mut context = Context::default();
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(limits.max_loop_iterations);
    let value = context.eval(Source::from_bytes(&code)).map_err(|error| {
        Error::ScriptExecution(format!("JavaScript evaluation failed: {error}"))
    })?;
    if !value.is_string() {
        return Err(Error::ScriptExecution(
            "script wrapper did not return JSON".to_string(),
        ));
    }
    let result = value
        .to_string(&mut context)
        .map_err(|error| Error::ScriptExecution(format!("failed to read script result: {error}")))?
        .to_std_string()
        .map_err(|error| Error::ScriptExecution(format!("script result is not UTF-8: {error}")))?;
    if result.len() > limits.max_json_bytes {
        return Err(Error::ScriptExecution(format!(
            "script result exceeds {} bytes",
            limits.max_json_bytes
        )));
    }

    let mut envelope: ExecutionEnvelope = serde_json::from_str(&result)
        .map_err(|error| Error::ScriptExecution(format!("invalid script result: {error}")))?;
    validate_logs(&envelope.logs, limits)?;
    if let Some(error) = envelope.error {
        append_exception(&mut envelope.logs, error, limits)?;
        return Ok(ScriptOutput {
            config: config.clone(),
            logs: into_script_logs(envelope.logs),
        });
    }

    let value = envelope.config.ok_or_else(|| {
        Error::ScriptExecution("main(config, profileName) must return an object".to_string())
    })?;
    if !value.is_object() {
        return Err(Error::ScriptExecution(
            "main(config, profileName) must return an object".to_string(),
        ));
    }
    let output = serde_json::from_value::<Mapping>(value).map_err(|error| {
        Error::ScriptExecution(format!("invalid configuration output: {error}"))
    })?;
    Ok(ScriptOutput {
        config: lowercase_mapping(&output),
        logs: into_script_logs(envelope.logs),
    })
}

fn build_program(
    script: &str,
    config_json: &str,
    profile_json: &str,
    limits: ScriptLimits,
) -> String {
    format!(
        r#"
(() => {{
  const __rsclashLogs = [];
  let __rsclashLogBytes = 0;
  const __rsclashLog = (level, data) => {{
    if (__rsclashLogs.length >= {max_logs}) {{
      throw new Error("maximum number of log outputs exceeded");
    }}
    const encoded = JSON.stringify(data, null, 2);
    const text = encoded === undefined ? String(data) : encoded;
    __rsclashLogBytes += level.length + text.length;
    if (__rsclashLogBytes > {max_log_bytes}) {{
      throw new Error("maximum log output size exceeded");
    }}
    __rsclashLogs.push([level, text]);
  }};
  const console = Object.freeze({{
    log(data) {{ __rsclashLog("log", data); }},
    info(data) {{ __rsclashLog("info", data); }},
    error(data) {{ __rsclashLog("error", data); }},
    debug(data) {{ __rsclashLog("debug", data); }},
    warn(data) {{ __rsclashLog("warn", data); }},
    table(data) {{ __rsclashLog("table", data); }}
  }});
  let __rsclashConfig = null;
  let __rsclashError = null;
  try {{
    {script}
    __rsclashConfig = main({config_json}, {profile_json});
  }} catch (error) {{
    __rsclashError = String(error);
  }}
  return JSON.stringify({{
    config: __rsclashConfig,
    logs: __rsclashLogs,
    error: __rsclashError
  }});
}})()
"#,
        max_logs = limits.max_logs,
        max_log_bytes = limits.max_log_bytes,
    )
}

fn validate_logs(logs: &[(String, String)], limits: ScriptLimits) -> Result<()> {
    if logs.len() > limits.max_logs {
        return Err(Error::ScriptExecution(format!(
            "script produced more than {} logs",
            limits.max_logs
        )));
    }
    let size = logs
        .iter()
        .map(|(level, message)| level.len().saturating_add(message.len()))
        .sum::<usize>();
    if size > limits.max_log_bytes {
        return Err(Error::ScriptExecution(format!(
            "script logs exceed {} bytes",
            limits.max_log_bytes
        )));
    }
    Ok(())
}

fn append_exception(
    logs: &mut Vec<(String, String)>,
    mut message: String,
    limits: ScriptLimits,
) -> Result<()> {
    const LEVEL: &str = "exception";
    if limits.max_logs == 0 || limits.max_log_bytes < LEVEL.len() {
        return Err(Error::ScriptExecution(
            "script limits leave no room for an exception log".to_string(),
        ));
    }
    while logs.len() >= limits.max_logs {
        logs.pop();
    }
    let used = logs
        .iter()
        .map(|(level, message)| level.len().saturating_add(message.len()))
        .sum::<usize>();
    let available = limits
        .max_log_bytes
        .saturating_sub(used)
        .saturating_sub(LEVEL.len());
    if message.len() > available {
        let mut boundary = available;
        while !message.is_char_boundary(boundary) {
            boundary = boundary.saturating_sub(1);
        }
        message.truncate(boundary);
    }
    logs.push((LEVEL.to_string(), message));
    Ok(())
}

fn into_script_logs(logs: Vec<(String, String)>) -> Vec<ScriptLog> {
    logs.into_iter()
        .map(|(level, message)| ScriptLog { level, message })
        .collect()
}

fn thread_label(id: &str) -> String {
    let label = id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(32)
        .collect::<String>();
    if label.is_empty() {
        "anonymous".to_string()
    } else {
        label
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use serde_yaml_ng::{Mapping, Value};

    use super::{BoaScriptExecutor, ScriptExecutor, ScriptLayer, ScriptLimits};

    fn mapping(source: &str) -> Mapping {
        serde_yaml_ng::from_str(source).expect("test YAML should parse")
    }

    #[test]
    fn executes_script_with_profile_name_and_captures_logs() {
        let executor = BoaScriptExecutor::default();
        let output = executor
            .execute(
                &ScriptLayer {
                    id: "profile-script".to_string(),
                    source: r#"
function main(config, profileName) {
  console.info(profileName);
  config.Mode = "global";
  config.profile = profileName;
  return config;
}
"#
                    .to_string(),
                },
                &mapping("Mixed-Port: 7890"),
                "Alice's profile\nnext",
            )
            .expect("script should execute");

        assert_eq!(
            output.config.get("mode").and_then(Value::as_str),
            Some("global")
        );
        assert_eq!(
            output.config.get("profile").and_then(Value::as_str),
            Some("Alice's profile\nnext")
        );
        assert_eq!(output.logs[0].level, "info");
        assert!(output.logs[0].message.contains("Alice's profile"));
    }

    #[test]
    fn log_limit_stops_script_and_preserves_input() {
        let executor = BoaScriptExecutor::new(ScriptLimits {
            max_logs: 2,
            ..ScriptLimits::default()
        });
        let output = executor
            .execute(
                &ScriptLayer {
                    id: "noisy".to_string(),
                    source: r#"
function main(config) {
  for (let index = 0; index < 3; index += 1) console.log(index);
  config.changed = true;
  return config;
}
"#
                    .to_string(),
                },
                &mapping("original: true"),
                "Test",
            )
            .expect("caught script errors should produce logs");

        assert_eq!(
            output.config.get("original").and_then(Value::as_bool),
            Some(true)
        );
        assert!(!output.config.contains_key("changed"));
        assert_eq!(
            output.logs.last().map(|log| log.level.as_str()),
            Some("exception")
        );
    }

    #[test]
    fn rejects_non_object_results() {
        let error = BoaScriptExecutor::default()
            .execute(
                &ScriptLayer {
                    id: "invalid".to_string(),
                    source: "function main() { return 42; }".to_string(),
                },
                &Mapping::new(),
                "Test",
            )
            .expect_err("scalar output should be rejected");

        assert!(error.to_string().contains("must return an object"));
    }

    #[test]
    fn loop_limit_interrupts_runaway_script() {
        let executor = BoaScriptExecutor::new(ScriptLimits {
            max_loop_iterations: 20,
            ..ScriptLimits::default()
        });
        let error = executor
            .execute(
                &ScriptLayer {
                    id: "loop".to_string(),
                    source: "function main(config) { while (true) {} return config; }".to_string(),
                },
                &Mapping::new(),
                "Test",
            )
            .expect_err("Boa should interrupt the runaway loop");

        assert!(error.to_string().contains("loop iteration limit"));
    }

    #[test]
    fn rejects_oversized_input_before_starting_boa() {
        let executor = BoaScriptExecutor::new(ScriptLimits {
            max_json_bytes: 8,
            ..ScriptLimits::default()
        });
        let error = executor
            .execute(
                &ScriptLayer {
                    id: "size".to_string(),
                    source: "function main(config) { return config; }".to_string(),
                },
                &mapping("value: too-large"),
                "Test",
            )
            .expect_err("oversized config should be rejected");

        assert!(error.to_string().contains("configuration exceeds"));
    }
}
