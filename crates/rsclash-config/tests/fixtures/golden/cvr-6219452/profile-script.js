function main(config, profileName) {
  console.warn(profileName);
  config["mixed-port"] = 9999;
  config.dns.ipv6 = false;
  config.dns.nameserver = ["profile-script"];
  config.custom.nested.winner = "profile-script";
  config.custom.nested["profile-script"] = true;
  return config;
}
