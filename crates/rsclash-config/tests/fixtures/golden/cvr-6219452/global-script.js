function main(config, profileName) {
  console.info({ stage: "global", profileName });
  config.mode = "direct";
  config.dns.ipv6 = false;
  config.dns.nameserver = ["global-script"];
  config.custom.nested.winner = "global-script";
  config.custom.nested["global-script"] = true;
  return config;
}
