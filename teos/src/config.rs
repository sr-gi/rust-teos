//! Logic related to the tower configuration and command line parameter parsing.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fmt;
use std::ops::Deref;
use std::path::PathBuf;
use structopt::StructOpt;

pub fn data_dir_absolute_path(data_dir: String) -> PathBuf {
    if let Some(a) = data_dir.strip_prefix('~') {
        if let Some(b) = data_dir.strip_prefix("~/") {
            home::home_dir().unwrap().join(b)
        } else {
            home::home_dir().unwrap().join(a)
        }
    } else {
        PathBuf::from(&data_dir)
    }
}

pub fn from_file<T: Default + serde::de::DeserializeOwned>(path: &PathBuf) -> T {
    match std::fs::read(path) {
        Ok(file_content) => toml::from_slice::<T>(&file_content).map_or_else(
            |e| {
                eprintln!("Couldn't parse config file: {e}");
                T::default()
            },
            |config| config,
        ),
        Err(_) => T::default(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
enum ParamSource {
    ConfigFile,
    CommandLine,
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigParam<T> {
    inner: T,
    source: ParamSource,
    sensitive: bool,
}

impl<T> ConfigParam<T>
where
    T: DeserializeOwned,
{
    fn from_file(inner: T) -> Self {
        Self {
            inner,
            source: ParamSource::ConfigFile,
            sensitive: false,
        }
    }

    fn from_cmd(inner: T) -> Self {
        Self {
            inner,
            source: ParamSource::CommandLine,
            sensitive: false,
        }
    }

    fn from_default(inner: T) -> Self {
        Self {
            inner,
            source: ParamSource::Default,
            sensitive: false,
        }
    }

    fn set_inner(&mut self, inner: T) {
        self.inner = inner
    }

    fn is_default(&self) -> bool {
        matches!(&self.source, ParamSource::Default)
    }
}

impl<T> Deref for ConfigParam<T>
where
    T: DeserializeOwned,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

// impl<T> Serialize for ConfigParam<T>
// where
//     T: DeserializeOwned,
// {
//     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
//     where
//         S: serde::Serializer,
//     {
//         self.inner.serialize(serializer)
//     }
// }

impl<'de, T> Deserialize<'de> for ConfigParam<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: DeserializeOwned,
    {
        Ok(ConfigParam::from_file(T::deserialize(deserializer)?))
    }
}

impl<T> fmt::Display for ConfigParam<T>
where
    T: fmt::Display + DeserializeOwned + Serialize,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner)
    }
}

/// Error raised if something is wrong with the configuration.
#[derive(PartialEq, Eq, Debug)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Configuration error: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Holds all the command line options.
#[derive(StructOpt, Debug, Clone)]
#[structopt(rename_all = "lowercase")]
#[structopt(version = env!("CARGO_PKG_VERSION"), about = "The Eye of Satoshi - Lightning watchtower")]
pub struct Opt {
    /// Address teos HTTP(s) API will bind to [default: localhost]
    #[structopt(long)]
    pub api_bind: Option<String>,

    /// Port teos HTTP(s) API will bind to [default: 9814]
    #[structopt(long)]
    pub api_port: Option<u16>,

    /// Address teos RPC server will bind to [default: localhost]
    #[structopt(long)]
    pub rpc_bind: Option<String>,

    /// Port teos RPC server will bind to [default: 8814]
    #[structopt(long)]
    pub rpc_port: Option<u16>,

    /// Network bitcoind is connected to. Either mainnet, testnet, signet or regtest [default: mainnet]
    #[structopt(long)]
    pub btc_network: Option<String>,

    /// bitcoind rpcuser [default: user]
    #[structopt(long)]
    pub btc_rpc_user: Option<String>,

    /// bitcoind rpcpassword [default: passwd]
    #[structopt(long)]
    pub btc_rpc_password: Option<String>,

    /// bitcoind rpcconnect [default: localhost]
    #[structopt(long)]
    pub btc_rpc_connect: Option<String>,

    /// bitcoind rpcport [default: 8332]
    #[structopt(long)]
    pub btc_rpc_port: Option<u16>,

    /// Specify data directory
    #[structopt(long, default_value = "~/.teos")]
    pub data_dir: String,

    /// Runs teos in debug mode
    #[structopt(long)]
    pub debug: bool,

    /// Runs third party libs in debug mode
    #[structopt(long)]
    pub deps_debug: bool,

    /// Overwrites the tower secret key. THIS IS IRREVERSIBLE AND WILL CHANGE YOUR TOWER ID
    #[structopt(long)]
    pub overwrite_key: bool,

    /// If set, creates a Tor endpoint to serve API data. This endpoint is additional to the clearnet HTTP API
    #[structopt(long)]
    pub tor_support: bool,

    /// Forces the tower to run even if the underlying chain has gone too far out of sync. This can only happen
    /// if the node is being run in pruned mode.
    #[structopt(long)]
    pub force_update: bool,

    /// Tor control port [default: 9051]
    #[structopt(long)]
    pub tor_control_port: Option<u16>,

    /// Port for the onion hidden service to listen on [default: 9814]
    #[structopt(long)]
    pub onion_hidden_service_port: Option<u16>,
}

/// Holds all configuration options.
///
/// The overwrite policy goes, from less to more:
/// - Defaults
/// - Configuration file
/// - Command line options
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    // API
    pub api_bind: ConfigParam<String>,
    pub api_port: ConfigParam<u16>,

    // RPC
    pub rpc_bind: ConfigParam<String>,
    pub rpc_port: ConfigParam<u16>,

    // Bitcoind
    pub btc_network: ConfigParam<String>,
    pub btc_rpc_user: ConfigParam<String>,
    pub btc_rpc_password: ConfigParam<String>,
    pub btc_rpc_connect: ConfigParam<String>,
    pub btc_rpc_port: ConfigParam<u16>,

    // Flags
    pub debug: ConfigParam<bool>,
    pub deps_debug: ConfigParam<bool>,
    pub overwrite_key: ConfigParam<bool>,
    pub force_update: ConfigParam<bool>,

    // General
    pub subscription_slots: ConfigParam<u32>,
    pub subscription_duration: ConfigParam<u32>,
    pub expiry_delta: ConfigParam<u32>,
    pub min_to_self_delay: ConfigParam<u16>,
    pub polling_delta: ConfigParam<u16>,

    // Internal API
    pub internal_api_bind: ConfigParam<String>,
    pub internal_api_port: ConfigParam<u32>,

    // Tor
    pub tor_support: ConfigParam<bool>,
    pub tor_control_port: ConfigParam<u16>,
    pub onion_hidden_service_port: ConfigParam<u16>,
}

impl Config {
    /// Patches the configuration options with the command line options.
    pub fn patch_with_options(&mut self, options: Opt) {
        if options.api_bind.is_some() {
            self.api_bind = ConfigParam::from_cmd(options.api_bind.unwrap());
        }
        if options.api_port.is_some() {
            self.api_port = ConfigParam::from_cmd(options.api_port.unwrap());
        }
        if options.rpc_bind.is_some() {
            self.rpc_bind = ConfigParam::from_cmd(options.rpc_bind.unwrap());
        }
        if options.rpc_port.is_some() {
            self.rpc_port = ConfigParam::from_cmd(options.rpc_port.unwrap());
        }
        if options.btc_network.is_some() {
            self.btc_network = ConfigParam::from_cmd(options.btc_network.unwrap());
        }
        if options.btc_rpc_user.is_some() {
            self.btc_rpc_user = ConfigParam::from_cmd(options.btc_rpc_user.unwrap());
        }
        if options.btc_rpc_password.is_some() {
            self.btc_rpc_password = ConfigParam::from_cmd(options.btc_rpc_password.unwrap());
        }
        if options.btc_rpc_connect.is_some() {
            self.btc_rpc_connect = ConfigParam::from_cmd(options.btc_rpc_connect.unwrap());
        }
        if options.btc_rpc_port.is_some() {
            self.btc_rpc_port = ConfigParam::from_cmd(options.btc_rpc_port.unwrap());
        }
        if options.tor_control_port.is_some() {
            self.tor_control_port = ConfigParam::from_cmd(options.tor_control_port.unwrap());
        }
        if options.onion_hidden_service_port.is_some() {
            self.onion_hidden_service_port =
                ConfigParam::from_cmd(options.onion_hidden_service_port.unwrap());
        }
        // Bools
        if options.tor_support {
            self.tor_support = ConfigParam::from_cmd(options.tor_support);
        }
        if options.debug {
            self.debug = ConfigParam::from_cmd(options.debug);
        }
        if options.deps_debug {
            self.deps_debug = ConfigParam::from_cmd(options.deps_debug);
        }
        // FIXME:
        if options.overwrite_key {
            self.overwrite_key = ConfigParam::from_cmd(options.overwrite_key);
        }
        if options.force_update {
            self.force_update = ConfigParam::from_cmd(options.force_update);
        }
    }

    /// Verifies that [Config] is properly built.
    ///
    /// This includes:
    /// - `bitcoind` credentials have been set
    /// - The Bitcoin network has been properly set (to either bitcoin, testnet, signet or regtest)
    ///
    /// This will also assign the default `btc_rpc_port` depending on the network if it has not
    /// been overwritten at this point.
    pub fn verify(&mut self) -> Result<(), ConfigError> {
        if self.btc_rpc_user.is_default() {
            return Err(ConfigError("btc_rpc_user must be set".to_owned()));
        }
        if self.btc_rpc_password.is_default() {
            return Err(ConfigError("btc_rpc_password must be set".to_owned()));
        }

        // Normalize the network option to the ones used by bitcoind.
        if ["mainnet", "testnet"].contains(&self.btc_network.inner.as_str()) {
            self.btc_network
                .set_inner(self.btc_network.inner.trim_end_matches("net").into());
        }

        let default_rpc_port = match self.btc_network.inner.as_str() {
            "main" => 8332,
            "test" => 18332,
            "regtest" => 18443,
            "signet" => 38332,
            _ => return Err(ConfigError(format!("btc_network not recognized. Expected {{mainnet, testnet, signet, regtest}}, received {}", self.btc_network)))
        };

        // Set the port to it's default (depending on the network) if it has not been
        // overwritten at this point.
        if self.btc_rpc_port.is_default() {
            self.btc_rpc_port.set_inner(default_rpc_port);
        }

        Ok(())
    }

    /// Checks whether the config has been set with only with default values.
    pub fn is_default(&self) -> bool {
        self == &Config::default()
    }

    /// Logs non-default options.
    pub fn log_non_default_options(&self) {
        let json_default_config = serde_json::json!(&Config::default());
        let json_config = serde_json::json!(&self);
        let sensitive_args = ["btc_rpc_user", "btc_rpc_password"];

        for (key, value) in json_config.as_object().unwrap().iter() {
            if *value != json_default_config[key] {
                log::info!(
                    "Custom config arg: {}: {}",
                    key,
                    if sensitive_args.contains(&key.as_str()) {
                        "****".to_owned()
                    } else {
                        value.to_string()
                    }
                );
            }
        }
    }
}

impl Default for Config {
    /// Sets the tower [Config] defaults.
    ///
    /// Notice the defaults are not enough, and the tower will refuse to run on them.
    /// For instance, the defaults do set the `bitcoind` `rpu_user` and `rpc_password`
    /// to empty strings so the user is forced the set them (and most importantly so the
    /// user does not use any values provided here).
    fn default() -> Self {
        Self {
            api_bind: ConfigParam::from_default("127.0.0.1".into()),
            api_port: ConfigParam::from_default(9814),
            tor_support: ConfigParam::from_default(false),
            tor_control_port: ConfigParam::from_default(9051),
            onion_hidden_service_port: ConfigParam::from_default(9814),
            rpc_bind: ConfigParam::from_default("127.0.0.1".into()),
            rpc_port: ConfigParam::from_default(8814),
            btc_network: ConfigParam::from_default("mainnet".into()),
            btc_rpc_user: ConfigParam::from_default(String::new()),
            btc_rpc_password: ConfigParam::from_default(String::new()),
            btc_rpc_connect: ConfigParam::from_default("localhost".into()),
            btc_rpc_port: ConfigParam::from_default(0),

            debug: ConfigParam::from_default(false),
            deps_debug: ConfigParam::from_default(false),
            overwrite_key: ConfigParam::from_default(false),
            force_update: ConfigParam::from_default(false),
            subscription_slots: ConfigParam::from_default(10000),
            subscription_duration: ConfigParam::from_default(4320),
            expiry_delta: ConfigParam::from_default(6),
            min_to_self_delay: ConfigParam::from_default(20),
            polling_delta: ConfigParam::from_default(60),
            internal_api_bind: ConfigParam::from_default("127.0.0.1".into()),
            internal_api_port: ConfigParam::from_default(50051),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Default for Opt {
        fn default() -> Self {
            Self {
                api_bind: None,
                api_port: None,
                tor_support: false,
                tor_control_port: None,
                onion_hidden_service_port: None,
                rpc_bind: None,
                rpc_port: None,
                btc_network: None,
                btc_rpc_user: None,
                btc_rpc_password: None,
                btc_rpc_connect: None,
                btc_rpc_port: None,
                data_dir: String::from("~/.teos"),

                debug: false,
                deps_debug: false,
                overwrite_key: false,
                force_update: false,
            }
        }
    }

    #[test]
    fn test_config_patch_with_options() {
        // Tests that a given Config is overwritten with Opts if the options are present
        let mut config = Config::default();
        let config_clone = config.clone();
        let mut opt = Opt::default();

        let expected_value = String::from("test");
        opt.api_bind = Some(expected_value.clone());
        config.patch_with_options(opt);

        // Check the field has been updated
        assert_eq!(config.api_bind, expected_value);

        // Check the rest of fields are equal. The easiest is to just the field back and compare with a clone
        config.api_bind = config_clone.api_bind.clone();
        assert_eq!(config, config_clone);
    }

    #[test]
    fn test_config_default_not_verify() {
        // Tests that the default configuration does not pass verification checks. This is on purpose so some fields are
        // required to be updated by the user.
        let mut config = Config::default();
        assert!(
            matches!(config.verify(), Err(ConfigError(e)) if e.contains("btc_rpc_user must be set"))
        );
    }

    #[test]
    fn test_config_default_verify_overwrite_required() {
        // Tests that setting a some btc_rpc_user and btc_rpc_password results in a Config object that verifies
        let mut config = Config {
            btc_rpc_user: "user".to_owned(),
            btc_rpc_password: "password".to_owned(),
            ..Default::default()
        };
        config.verify().unwrap();
    }

    #[test]
    fn test_config_verify_wrong_network() {
        // Tests that setting a wrong network will make verify fail
        let mut config = Config {
            btc_rpc_user: "user".to_owned(),
            btc_rpc_password: "password".to_owned(),
            btc_network: "wrong_network".to_owned(),
            ..Default::default()
        };
        assert!(
            matches!(config.verify(), Err(ConfigError(e)) if e.contains("btc_network not recognized"))
        );
    }

    #[test]
    fn test_config_verify_tor_set() {
        let mut config = Config {
            btc_rpc_user: "user".to_owned(),
            btc_rpc_password: "password".to_owned(),
            tor_support: true,
            ..Default::default()
        };

        config.verify().unwrap()
    }
}
