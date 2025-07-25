mod ntp_source;
mod server;
pub mod subnet;

use clock_steering::unix::UnixClock;
use ntp_proto::{
    AlgorithmConfig, NtpVersion, ProtocolVersion, SourceConfig, SynchronizationConfig,
};
pub use ntp_source::*;
use serde::{Deserialize, Deserializer};
pub use server::*;
use std::io;
use std::{
    fmt::Display,
    io::ErrorKind,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    str::FromStr,
};
use timestamped_socket::interface::InterfaceName;
use tracing::{info, warn};

use super::{clock::NtpClockWrapper, tracing::LogLevel};

const USAGE_MSG: &str = "\
usage: ntp-daemon [-c PATH] [-l LOG_LEVEL]
       ntp-daemon -h
       ntp-daemon -v";

const DESCRIPTOR: &str = "ntp-daemon - synchronize system time";

const HELP_MSG: &str = "Options:
  -c, --config=PATH             change the config .toml file
  -l, --log-level=LOG_LEVEL     change the log level
  -h, --help                    display this help text
  -v, --version                 display version information";

pub fn long_help_message() -> String {
    format!("{DESCRIPTOR}\n\n{USAGE_MSG}\n\n{HELP_MSG}")
}

#[derive(Debug, Default)]
pub(crate) struct NtpDaemonOptions {
    /// Path of the configuration file
    pub config: Option<PathBuf>,
    /// Level for messages to display in logs
    pub log_level: Option<LogLevel>,
    help: bool,
    version: bool,
    pub action: NtpDaemonAction,
}

pub enum CliArg {
    Flag(String),
    Argument(String, String),
    Rest(Vec<String>),
}

impl CliArg {
    pub fn normalize_arguments<I>(
        takes_argument: &[&str],
        takes_argument_short: &[char],
        iter: I,
    ) -> Result<Vec<Self>, String>
    where
        I: IntoIterator<Item = String>,
    {
        // the first argument is the ntp-daemon command - so we can skip it
        let mut arg_iter = iter.into_iter().skip(1);
        let mut processed = vec![];
        let mut rest = vec![];

        while let Some(arg) = arg_iter.next() {
            match arg.as_str() {
                "--" => {
                    rest.extend(arg_iter);
                    break;
                }
                long_arg if long_arg.starts_with("--") => {
                    // --config=/path/to/config.toml
                    let invalid = Err(format!("invalid option: '{long_arg}'"));

                    if let Some((key, value)) = long_arg.split_once('=') {
                        if takes_argument.contains(&key) {
                            processed.push(CliArg::Argument(key.to_string(), value.to_string()))
                        } else {
                            invalid?
                        }
                    } else if takes_argument.contains(&long_arg) {
                        if let Some(next) = arg_iter.next() {
                            processed.push(CliArg::Argument(long_arg.to_string(), next))
                        } else {
                            Err(format!("'{}' expects an argument", &long_arg))?;
                        }
                    } else {
                        processed.push(CliArg::Flag(arg));
                    }
                }
                short_arg if short_arg.starts_with('-') => {
                    // split combined shorthand options
                    for (n, char) in short_arg.trim_start_matches('-').chars().enumerate() {
                        let flag = format!("-{char}");
                        // convert option argument to separate segment
                        if takes_argument_short.contains(&char) {
                            let rest = short_arg[(n + 2)..].trim().to_string();
                            // assignment syntax is not accepted for shorthand arguments
                            if rest.starts_with('=') {
                                Err("invalid option '='")?;
                            }
                            if !rest.is_empty() {
                                processed.push(CliArg::Argument(flag, rest));
                            } else if let Some(next) = arg_iter.next() {
                                processed.push(CliArg::Argument(flag, next));
                            } else if char == 'h' {
                                // short version of --help has no arguments
                                processed.push(CliArg::Flag(flag));
                            } else {
                                Err(format!("'-{char}' expects an argument"))?;
                            }
                            break;
                        } else {
                            processed.push(CliArg::Flag(flag));
                        }
                    }
                }
                _argument => rest.push(arg),
            }
        }

        if !rest.is_empty() {
            processed.push(CliArg::Rest(rest));
        }

        Ok(processed)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub enum NtpDaemonAction {
    #[default]
    Help,
    Version,
    Run,
}

impl NtpDaemonOptions {
    const TAKES_ARGUMENT: &'static [&'static str] = &["--config", "--log-level"];
    const TAKES_ARGUMENT_SHORT: &'static [char] = &['c', 'l'];

    /// parse an iterator over command line arguments
    pub fn try_parse_from<I, T>(iter: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str> + Clone,
    {
        let mut options = NtpDaemonOptions::default();
        let arg_iter = CliArg::normalize_arguments(
            Self::TAKES_ARGUMENT,
            Self::TAKES_ARGUMENT_SHORT,
            iter.into_iter().map(|x| x.as_ref().to_string()),
        )?
        .into_iter()
        .peekable();

        for arg in arg_iter {
            match arg {
                CliArg::Flag(flag) => match flag.as_str() {
                    "-h" | "--help" => {
                        options.help = true;
                    }
                    "-v" | "--version" => {
                        options.version = true;
                    }
                    option => {
                        Err(format!("invalid option provided: {option}"))?;
                    }
                },
                CliArg::Argument(option, value) => match option.as_str() {
                    "-c" | "--config" => {
                        options.config = Some(PathBuf::from(value));
                    }
                    "-l" | "--log-level" => match LogLevel::from_str(&value) {
                        Ok(level) => options.log_level = Some(level),
                        Err(_) => return Err("invalid log level".into()),
                    },
                    option => {
                        Err(format!("invalid option provided: {option}"))?;
                    }
                },
                CliArg::Rest(_rest) => { /* do nothing, drop remaining arguments */ }
            }
        }

        options.resolve_action();
        // nothing to validate at the moment

        Ok(options)
    }

    /// from the arguments resolve which action should be performed
    fn resolve_action(&mut self) {
        if self.help {
            self.action = NtpDaemonAction::Help;
        } else if self.version {
            self.action = NtpDaemonAction::Version;
        } else {
            self.action = NtpDaemonAction::Run;
        }
    }
}

fn deserialize_ntp_clock<'de, D>(deserializer: D) -> Result<NtpClockWrapper, D::Error>
where
    D: Deserializer<'de>,
{
    let data: Option<PathBuf> = Deserialize::deserialize(deserializer)?;

    if let Some(path) = data {
        tracing::info!("using custom clock {path:?}");
        #[cfg(target_os = "linux")]
        return Ok(NtpClockWrapper::new(
            UnixClock::open(path).map_err(|e| serde::de::Error::custom(e.to_string()))?,
        ));

        #[cfg(not(target_os = "linux"))]
        panic!("Custom clock paths not supported on this platform");
    } else {
        tracing::debug!("using REALTIME clock");
        Ok(NtpClockWrapper::new(UnixClock::CLOCK_REALTIME))
    }
}

fn deserialize_interface<'de, D>(deserializer: D) -> Result<Option<InterfaceName>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt_interface_name: Option<InterfaceName> = Deserialize::deserialize(deserializer)?;

    if let Some(interface_name) = opt_interface_name {
        tracing::debug!("using custom interface {}", interface_name);
    } else {
        tracing::trace!("using default interface");
    }

    Ok(opt_interface_name)
}

/// Timestamping mode. This is a hint!
///
/// Your OS or hardware might not actually support some timestamping modes.
/// Unsupported timestamping modes are ignored.
#[derive(Default, Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum TimestampMode {
    #[cfg_attr(not(any(target_os = "linux", target_os = "freebsd")), default)]
    Software,
    #[cfg_attr(target_os = "freebsd", default)]
    KernelRecv,
    #[cfg_attr(target_os = "linux", default)]
    KernelAll,
    Hardware,
}

impl TimestampMode {
    #[cfg(target_os = "linux")]
    pub(crate) fn as_interface_mode(self) -> timestamped_socket::socket::InterfaceTimestampMode {
        use timestamped_socket::socket::InterfaceTimestampMode::*;
        match self {
            TimestampMode::Software => None,
            TimestampMode::KernelRecv => SoftwareRecv,
            TimestampMode::KernelAll => SoftwareAll,
            TimestampMode::Hardware => HardwareAll,
        }
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(crate) fn as_general_mode(self) -> timestamped_socket::socket::GeneralTimestampMode {
        use timestamped_socket::socket::GeneralTimestampMode::*;
        match self {
            TimestampMode::Software => None,
            TimestampMode::KernelRecv => SoftwareRecv,
            TimestampMode::KernelAll | TimestampMode::Hardware => SoftwareAll,
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    pub(crate) fn as_general_mode(self) -> timestamped_socket::socket::GeneralTimestampMode {
        use timestamped_socket::socket::GeneralTimestampMode::*;
        None
    }
}

#[derive(Deserialize, Debug, Copy, Clone, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ClockConfig {
    #[serde(deserialize_with = "deserialize_ntp_clock", default)]
    pub clock: NtpClockWrapper,
    #[serde(deserialize_with = "deserialize_interface", default)]
    pub interface: Option<InterfaceName>,
    pub timestamp_mode: TimestampMode,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub log_level: Option<LogLevel>,
    #[serde(default = "default_ansi_colors")]
    pub ansi_colors: bool,
    #[serde(default)]
    pub observation_path: Option<PathBuf>,
    #[serde(default = "default_observation_permissions")]
    pub observation_permissions: u32,
    #[serde(default = "default_metrics_exporter_listen")]
    pub metrics_exporter_listen: SocketAddr,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            log_level: Default::default(),
            ansi_colors: default_ansi_colors(),
            observation_path: Default::default(),
            observation_permissions: default_observation_permissions(),
            metrics_exporter_listen: default_metrics_exporter_listen(),
        }
    }
}

const fn default_ansi_colors() -> bool {
    true
}

const fn default_observation_permissions() -> u32 {
    0o666
}

fn default_metrics_exporter_listen() -> SocketAddr {
    "127.0.0.1:9975".parse().unwrap()
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DaemonSynchronizationConfig {
    #[serde(flatten)]
    pub synchronization_base: SynchronizationConfig,

    #[serde(default)]
    pub algorithm: AlgorithmConfig,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    #[serde(rename = "source", default)]
    pub sources: Vec<NtpSourceConfig>,
    #[serde(rename = "server", default)]
    pub servers: Vec<ServerConfig>,
    #[serde(rename = "nts-ke-server", default)]
    pub nts_ke: Vec<NtsKeConfig>,
    #[serde(default)]
    pub synchronization: DaemonSynchronizationConfig,
    #[serde(default)]
    pub source_defaults: SourceConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
    #[serde(default)]
    pub keyset: KeysetConfig,
    #[serde(default)]
    #[cfg(feature = "hardware-timestamping")]
    pub clock: ClockConfig,
}

impl Config {
    fn from_file(file: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let meta = std::fs::metadata(&file)?;
        let perm = meta.permissions();

        if perm.mode() as libc::mode_t & libc::S_IWOTH != 0 {
            warn!("Unrestricted config file permissions: Others can write.");
        }

        let contents = std::fs::read_to_string(file)?;
        Ok(toml::de::from_str(&contents)?)
    }

    fn from_first_file(file: Option<impl AsRef<Path>>) -> Result<Config, ConfigError> {
        // if an explicit file is given, always use that one
        if let Some(f) = file {
            let path: &Path = f.as_ref();
            info!(?path, "using config file");
            return Config::from_file(f);
        }

        // for the global file we also ignore it when there are permission errors
        let global_path = Path::new("/etc/ntpd-rs/ntp.toml");
        if global_path.exists() {
            info!("using config file at default location `{:?}`", global_path);
            match Config::from_file(global_path) {
                Err(ConfigError::Io(e)) if e.kind() == ErrorKind::PermissionDenied => {
                    warn!("permission denied on global config file! using default config ...");
                }
                other => {
                    return other;
                }
            }
        }

        Ok(Config::default())
    }

    pub fn from_args(
        file: Option<impl AsRef<Path>>,
        sources: Vec<NtpSourceConfig>,
        servers: Vec<ServerConfig>,
    ) -> Result<Config, ConfigError> {
        let mut config = Config::from_first_file(file.as_ref())?;

        if !sources.is_empty() {
            if !config.sources.is_empty() {
                info!("overriding sources from configuration");
            }
            config.sources = sources;
        }

        if !servers.is_empty() {
            if !config.servers.is_empty() {
                info!("overriding servers from configuration");
            }
            config.servers = servers;
        }

        Ok(config)
    }

    /// Count potential number of sources in configuration
    fn count_sources(&self) -> usize {
        let mut count = 0;
        for source in &self.sources {
            match source {
                NtpSourceConfig::Standard(_) => count += 1,
                NtpSourceConfig::Nts(_) => count += 1,
                NtpSourceConfig::Pool(config) => count += config.first.count,
                #[cfg(feature = "unstable_nts-pool")]
                NtpSourceConfig::NtsPool(config) => count += config.first.count,
                NtpSourceConfig::Sock(_) => count += 1,
                #[cfg(feature = "pps")]
                NtpSourceConfig::Pps(_) => {} // PPS sources don't count
            }
        }
        count
    }

    /// Check that the config is reasonable. This function may panic if the
    /// configuration is egregious, although it doesn't do so currently.
    pub fn check(&self) -> bool {
        let mut ok = true;

        // Note: since we only check once logging is fully configured,
        // using those fields should always work. This is also
        // probably a good policy in general (config should always work
        // but we may panic here to protect the user from themselves)
        if self.sources.is_empty() {
            info!("No sources configured. Daemon will not change system time.");
        }

        if !self.sources.is_empty()
            && self.count_sources()
                < self
                    .synchronization
                    .synchronization_base
                    .minimum_agreeing_sources
        {
            warn!(
                "Fewer sources configured than are required to agree on the current time. Daemon will not change system time."
            );
            ok = false;
        }

        if self.sources.iter().any(|config| match config {
            NtpSourceConfig::Sock(_) => false,
            NtpSourceConfig::Pps(_) => false,
            NtpSourceConfig::Standard(config) => {
                matches!(config.first.ntp_version, ProtocolVersion::V5)
            }
            NtpSourceConfig::Nts(config) => {
                matches!(config.first.ntp_version, ProtocolVersion::V5)
            }
            NtpSourceConfig::Pool(config) => {
                matches!(config.first.ntp_version, ProtocolVersion::V5)
            }
            #[cfg(feature = "unstable_nts-pool")]
            NtpSourceConfig::NtsPool(config) => {
                matches!(config.first.ntp_version, ProtocolVersion::V5)
            }
        }) {
            warn!(
                "Forcing a source into NTPv5, which is still a draft. There is no guarantee that the server will remain compatible with this or future versions of ntpd-rs."
            );
            ok = false;
        }

        // Check that the NTS configuration is consistent with the NTP configuration
        for ke_server in self
            .nts_ke
            .iter()
            .filter(|ke_server| ke_server.ntp_server.is_none())
        {
            if ke_server.accept_ntp_versions.contains(&NtpVersion::V4)
                && !self.servers.iter().any(|server| {
                    server.listen.port() == ke_server.ntp_port.unwrap_or(123)
                        && server.accept_ntp_versions.contains(&NtpVersion::V4)
                })
            {
                warn!(
                    "Configured NTS for NTPv4 on port {}, but have no server listening on that port for NTPv4 traffic. If this is for an external ntp server, consider configuring a value for `ntp-server`.",
                    ke_server.ntp_port.unwrap_or(123)
                );
                ok = false;
            }

            if ke_server.accept_ntp_versions.contains(&NtpVersion::V5)
                && !self.servers.iter().any(|server| {
                    server.listen.port() == ke_server.ntp_port.unwrap_or(123)
                        && server.accept_ntp_versions.contains(&NtpVersion::V5)
                })
            {
                warn!(
                    "Configured NTS for NTPv5 on port {}, but have no server listening on that port for NTPv5 traffic. If this is for an external ntp server, consider configuring a value for `ntp-server`.",
                    ke_server.ntp_port.unwrap_or(123)
                );
                ok = false;
            }
        }

        ok
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Toml(toml::de::Error),
}

impl std::error::Error for ConfigError {}

impl Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error while reading config: {e}"),
            Self::Toml(e) => write!(f, "config toml parsing error: {e}"),
        }
    }
}

impl From<io::Error> for ConfigError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(value: toml::de::Error) -> Self {
        Self::Toml(value)
    }
}

#[cfg(test)]
mod tests {
    use ntp_proto::{NtpDuration, ProtocolVersion, StepThreshold};

    use super::*;

    #[test]
    fn test_config() {
        let config: Config =
            toml::from_str("[[source]]\nmode = \"server\"\naddress = \"example.com\"").unwrap();
        assert_eq!(
            config.sources,
            vec![NtpSourceConfig::Standard(FlattenedPair {
                first: StandardSource {
                    address: NormalizedAddress::new_unchecked("example.com", 123).into(),
                    ntp_version: ProtocolVersion::V4,
                },
                second: Default::default()
            })]
        );
        assert!(config.observability.log_level.is_none());

        let config: Config = toml::from_str(
            "[observability]\nlog-level = \"info\"\n[[source]]\nmode = \"server\"\naddress = \"example.com\"",
        )
            .unwrap();
        assert_eq!(config.observability.log_level, Some(LogLevel::Info));
        assert_eq!(
            config.sources,
            vec![NtpSourceConfig::Standard(FlattenedPair {
                first: StandardSource {
                    address: NormalizedAddress::new_unchecked("example.com", 123).into(),
                    ntp_version: ProtocolVersion::V4,
                },
                second: Default::default()
            })]
        );

        let config: Config = toml::from_str(
            "[[source]]\nmode = \"server\"\naddress = \"example.com\"\n[synchronization]\nsingle-step-panic-threshold = 0",
        )
            .unwrap();
        assert_eq!(
            config.sources,
            vec![NtpSourceConfig::Standard(FlattenedPair {
                first: StandardSource {
                    address: NormalizedAddress::new_unchecked("example.com", 123).into(),
                    ntp_version: ProtocolVersion::V4,
                },
                second: Default::default()
            })]
        );
        assert_eq!(
            config
                .synchronization
                .synchronization_base
                .single_step_panic_threshold
                .forward,
            Some(NtpDuration::from_seconds(0.))
        );
        assert_eq!(
            config
                .synchronization
                .synchronization_base
                .single_step_panic_threshold
                .backward,
            Some(NtpDuration::from_seconds(0.))
        );

        let config: Config = toml::from_str(
            "[[source]]\nmode = \"server\"\naddress = \"example.com\"\n[synchronization]\nsingle-step-panic-threshold = \"inf\"",
        )
            .unwrap();
        assert_eq!(
            config.sources,
            vec![NtpSourceConfig::Standard(FlattenedPair {
                first: StandardSource {
                    address: NormalizedAddress::new_unchecked("example.com", 123).into(),
                    ntp_version: ProtocolVersion::V4,
                },
                second: Default::default()
            })]
        );
        assert!(
            config
                .synchronization
                .synchronization_base
                .single_step_panic_threshold
                .forward
                .is_none()
        );
        assert!(
            config
                .synchronization
                .synchronization_base
                .single_step_panic_threshold
                .backward
                .is_none()
        );

        let config: Config = toml::from_str(
            r#"
            [[source]]
            mode = "server"
            address = "example.com"
            [source-defaults]
            poll-interval-limits = { min = 5, max = 9 }
            initial-poll-interval = 5
            [observability]
            log-level = "info"
            observation-path = "/foo/bar/observe"
            observation-permissions = 0o567
            "#,
        )
        .unwrap();
        assert!(config.observability.log_level.is_some());

        assert_eq!(
            config.observability.observation_path,
            Some(PathBuf::from("/foo/bar/observe"))
        );
        assert_eq!(config.observability.observation_permissions, 0o567);

        assert_eq!(
            config.sources,
            vec![NtpSourceConfig::Standard(FlattenedPair {
                first: StandardSource {
                    address: NormalizedAddress::new_unchecked("example.com", 123).into(),
                    ntp_version: ProtocolVersion::V4,
                },
                second: Default::default()
            })]
        );

        let poll_interval_limits = config.source_defaults.poll_interval_limits;
        assert_eq!(poll_interval_limits.min.as_log(), 5);
        assert_eq!(poll_interval_limits.max.as_log(), 9);

        assert_eq!(config.source_defaults.initial_poll_interval.as_log(), 5);

        let config: Config = toml::from_str(
            "[[source]]\nmode = \"server\"\naddress = \"example.com\"\nntp-version = \"auto\"",
        )
        .unwrap();
        assert_eq!(
            config.sources,
            vec![NtpSourceConfig::Standard(FlattenedPair {
                first: StandardSource {
                    address: NormalizedAddress::new_unchecked("example.com", 123).into(),
                    ntp_version: ProtocolVersion::v4_upgrading_to_v5_with_default_tries(),
                },
                second: Default::default()
            })]
        );
        assert!(config.observability.log_level.is_none());
    }

    #[test]
    fn cli_no_arguments() {
        let arguments: [String; 0] = [];
        let parsed_empty = NtpDaemonOptions::try_parse_from(arguments).unwrap();

        assert!(parsed_empty.config.is_none());
        assert!(parsed_empty.log_level.is_none());
        assert_eq!(parsed_empty.action, NtpDaemonAction::Run);
    }

    #[test]
    fn cli_external_config() {
        let arguments = &["/usr/bin/ntp-daemon", "--config", "other.toml"];
        let parsed_empty = NtpDaemonOptions::try_parse_from(arguments).unwrap();

        assert_eq!(parsed_empty.config, Some("other.toml".into()));
        assert!(parsed_empty.log_level.is_none());
        assert_eq!(parsed_empty.action, NtpDaemonAction::Run);

        let arguments = &["/usr/bin/ntp-daemon", "-c", "other.toml"];
        let parsed_empty = NtpDaemonOptions::try_parse_from(arguments).unwrap();

        assert_eq!(parsed_empty.config, Some("other.toml".into()));
        assert!(parsed_empty.log_level.is_none());
        assert_eq!(parsed_empty.action, NtpDaemonAction::Run);
    }

    #[test]
    fn cli_log_level() {
        let arguments = &["/usr/bin/ntp-daemon", "--log-level", "debug"];
        let parsed_empty = NtpDaemonOptions::try_parse_from(arguments).unwrap();

        assert!(parsed_empty.config.is_none());
        assert_eq!(parsed_empty.log_level.unwrap(), LogLevel::Debug);

        let arguments = &["/usr/bin/ntp-daemon", "-l", "debug"];
        let parsed_empty = NtpDaemonOptions::try_parse_from(arguments).unwrap();

        assert!(parsed_empty.config.is_none());
        assert_eq!(parsed_empty.log_level.unwrap(), LogLevel::Debug);
    }

    #[test]
    fn toml_sources_invalid() {
        let config: Result<Config, _> = toml::from_str(
            r#"
            [[source]]
            mode = "server"
            address = ":invalid:ipv6:123"
            "#,
        );

        assert!(config.is_err());
    }

    #[test]
    fn toml_allow_no_sources() {
        let config: Result<Config, _> = toml::from_str(
            r#"
            [[server]]
            listen = "[::]:123"
            "#,
        );

        assert!(config.is_ok());
        assert!(config.unwrap().check());
    }

    #[test]
    fn system_config_accumulated_threshold() {
        let config: Result<SynchronizationConfig, _> = toml::from_str(
            r#"
            accumulated-step-panic-threshold = 0
            "#,
        );

        let config = config.unwrap();
        assert!(config.accumulated_step_panic_threshold.is_none());

        let config: Result<SynchronizationConfig, _> = toml::from_str(
            r#"
            accumulated-step-panic-threshold = 1000
            "#,
        );

        let config = config.unwrap();
        assert_eq!(
            config.accumulated_step_panic_threshold,
            Some(NtpDuration::from_seconds(1000.0))
        );
    }

    #[test]
    fn system_config_startup_panic_threshold() {
        let config: Result<SynchronizationConfig, _> = toml::from_str(
            r#"
            startup-step-panic-threshold = { forward = 10, backward = 20 }
            "#,
        );

        let config = config.unwrap();
        assert_eq!(
            config.startup_step_panic_threshold.forward,
            Some(NtpDuration::from_seconds(10.0))
        );
        assert_eq!(
            config.startup_step_panic_threshold.backward,
            Some(NtpDuration::from_seconds(20.0))
        );
    }

    #[test]
    fn duration_not_nan() {
        #[derive(Debug, Deserialize)]
        struct Helper {
            #[allow(unused)]
            duration: NtpDuration,
        }

        let result: Result<Helper, _> = toml::from_str(
            r#"
            duration = nan
            "#,
        );

        let error = result.unwrap_err();
        assert!(error.to_string().contains("expected a valid number"));
    }

    #[test]
    fn step_threshold_not_nan() {
        #[derive(Debug, Deserialize)]
        struct Helper {
            #[allow(unused)]
            threshold: StepThreshold,
        }

        let result: Result<Helper, _> = toml::from_str(
            r#"
            threshold = nan
            "#,
        );

        let error = result.unwrap_err();
        assert!(error.to_string().contains("expected a positive number"));
    }

    #[test]
    fn deny_unknown_fields() {
        let config: Result<SynchronizationConfig, _> = toml::from_str(
            r#"
            unknown-field = 42
            "#,
        );

        let error = config.unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn clock_config() {
        let config: Result<ClockConfig, _> = toml::from_str(
            r#"
            interface = "enp0s31f6"
            timestamp-mode = "software"
            "#,
        );

        let config = config.unwrap();

        let expected = InterfaceName::from_str("enp0s31f6").unwrap();
        assert_eq!(config.interface, Some(expected));

        assert_eq!(config.timestamp_mode, TimestampMode::Software);
    }

    #[test]
    fn daemon_synchronization_config() {
        let config: Result<DaemonSynchronizationConfig, _> = toml::from_str(
            r#"
            does_not_exist = 5
            "#,
        );

        assert!(config.is_err());

        let config: Result<DaemonSynchronizationConfig, _> = toml::from_str(
            r#"
            minimum-agreeing-sources = 2

            [algorithm]
            initial-wander = 1e-7
            "#,
        );

        let config = config.unwrap();
        assert_eq!(config.synchronization_base.minimum_agreeing_sources, 2);
        assert_eq!(config.algorithm.initial_wander, 1e-7);
    }
}
