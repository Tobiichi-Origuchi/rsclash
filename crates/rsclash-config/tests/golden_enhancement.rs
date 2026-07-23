#![allow(
  clippy::expect_used,
  reason = "fixture tests use expect for clear failures"
)]

use rsclash_config::{
  ApplicationLayer, EnhancementInput, EnhancementPipeline, ListenerPolicy, ManualLayer,
  MihomoConfig, SequenceEdit, SequenceLayers, TargetPlatform,
};
use serde_yaml_ng::Mapping;

const FIXTURE_ROOT: &str = "tests/fixtures/golden/cvr-6219452";
const CVR_REFERENCE_COMMIT: &str = "62194521681d1c70b674e8a0414eeac50bc034b0";

#[test]
fn runtime_is_semantically_equivalent_to_cvr_golden() {
  assert_eq!(CVR_REFERENCE_COMMIT.len(), 40);
  let input = EnhancementInput {
    current: MihomoConfig::parse(include_str!("fixtures/golden/cvr-6219452/current.yaml"))
      .expect("current profile should parse"),
    sequence: SequenceLayers {
      rules: sequence(include_str!("fixtures/golden/cvr-6219452/rules.yaml")),
      proxies: sequence(include_str!("fixtures/golden/cvr-6219452/proxies.yaml")),
      groups: sequence(include_str!("fixtures/golden/cvr-6219452/groups.yaml")),
    },
    application: ApplicationLayer {
      defaults: mapping(include_str!("fixtures/golden/cvr-6219452/application.yaml")),
      listeners: ListenerPolicy {
        socks: false,
        http: true,
        redir: true,
        tproxy: false,
        external_controller: true,
      },
      platform: TargetPlatform::Linux,
      enable_tun: true,
      dns_settings: Some(mapping(include_str!(
        "fixtures/golden/cvr-6219452/dns.yaml"
      ))),
      native_transforms: Vec::new(),
    },
    global: ManualLayer {
      merge: Some(mapping(include_str!(
        "fixtures/golden/cvr-6219452/global-merge.yaml"
      ))),
    },
    profile: ManualLayer {
      merge: Some(mapping(include_str!(
        "fixtures/golden/cvr-6219452/profile-merge.yaml"
      ))),
    },
  };

  let runtime = EnhancementPipeline::enhance(input);
  let actual = runtime.config.expect("runtime should contain config");
  let expected = MihomoConfig::parse(include_str!(
    "fixtures/golden/cvr-6219452/expected-cvr.yaml"
  ))
  .expect("CVR golden should parse");

  assert_eq!(
    serde_json::to_value(actual.mapping()).expect("actual config should convert to JSON"),
    serde_json::to_value(expected.mapping()).expect("expected config should convert to JSON"),
    "rsclash runtime diverged from CVR commit {CVR_REFERENCE_COMMIT} fixture at {FIXTURE_ROOT}"
  );
}

fn mapping(source: &str) -> Mapping {
  serde_yaml_ng::from_str(source).expect("fixture mapping should parse")
}

fn sequence(source: &str) -> SequenceEdit {
  serde_yaml_ng::from_str(source).expect("fixture sequence should parse")
}
