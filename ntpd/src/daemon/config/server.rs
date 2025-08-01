use std::{
    net::{AddrParseError, SocketAddr},
    path::PathBuf,
    str::FromStr,
    time::Duration,
};

use ntp_proto::{FilterAction, FilterList, NtpVersion};
use serde::{Deserialize, Deserializer};

#[derive(Debug, PartialEq, Eq, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct KeysetConfig {
    /// Number of old keys to keep around
    #[serde(default = "default_stale_key_count")]
    pub stale_key_count: usize,
    /// How often to rotate keys (seconds between rotations)
    #[serde(default = "default_key_rotation_interval")]
    pub key_rotation_interval: usize,
    #[serde(default)]
    pub key_storage_path: Option<String>,
}

impl Default for KeysetConfig {
    fn default() -> Self {
        Self {
            stale_key_count: default_stale_key_count(),
            key_rotation_interval: default_key_rotation_interval(),
            key_storage_path: None,
        }
    }
}

fn default_key_rotation_interval() -> usize {
    // 1 day in seconds
    86400
}

fn default_stale_key_count() -> usize {
    // 1 weeks worth at 1 key per day
    7
}

#[derive(Debug, PartialEq, Eq, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    #[serde(default = "default_denylist")]
    pub denylist: FilterList,
    #[serde(default = "default_allowlist")]
    pub allowlist: FilterList,
    #[serde(default)]
    pub rate_limiting_cache_size: usize,
    #[serde(
        default,
        rename = "rate-limiting-cutoff-ms",
        deserialize_with = "deserialize_rate_limiting_cutoff"
    )]
    pub rate_limiting_cutoff: Duration,
    #[serde(default, deserialize_with = "deserialize_require_nts")]
    pub require_nts: Option<FilterAction>,
    #[serde(
        default = "default_accepted_ntp_versions",
        deserialize_with = "deserialize_accepted_ntp_versions"
    )]
    pub accept_ntp_versions: Vec<NtpVersion>,
}

fn default_accepted_ntp_versions() -> Vec<NtpVersion> {
    vec![NtpVersion::V3, NtpVersion::V4]
}

fn deserialize_accepted_ntp_versions<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<NtpVersion>, D::Error> {
    let data = Vec::<u8>::deserialize(deserializer)?;

    data.into_iter()
        .map(|v| match v {
            3 => Ok(NtpVersion::V3),
            4 => Ok(NtpVersion::V4),
            5 => Ok(NtpVersion::V5),
            e => Err(serde::de::Error::custom(format!(
                "{e} is not a valid NTP version, version must be 4 and/or 5"
            ))),
        })
        .collect::<Result<Vec<NtpVersion>, D::Error>>()
}

fn deserialize_require_nts<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<FilterAction>, D::Error> {
    struct FilterActionVisitor;
    impl serde::de::Visitor<'_> for FilterActionVisitor {
        type Value = Option<FilterAction>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string (`ignore` or `deny`), or boolean")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            match value {
                "ignore" => Ok(Some(FilterAction::Ignore)),
                "deny" => Ok(Some(FilterAction::Deny)),
                _ => Err(serde::de::Error::unknown_variant(
                    value,
                    &["ignore", "deny"],
                )),
            }
        }

        fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if v {
                Ok(Some(FilterAction::Ignore))
            } else {
                Ok(None)
            }
        }
    }

    deserializer.deserialize_any(FilterActionVisitor)
}

fn default_denylist() -> FilterList {
    FilterList {
        filter: vec![],
        action: ntp_proto::FilterAction::Deny,
    }
}

fn default_allowlist() -> FilterList {
    FilterList {
        filter: vec!["::/0".parse().unwrap(), "0.0.0.0/0".parse().unwrap()],
        action: ntp_proto::FilterAction::Ignore,
    }
}

fn deserialize_rate_limiting_cutoff<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Duration, D::Error> {
    Ok(Duration::from_millis(u64::deserialize(deserializer)?))
}

impl TryFrom<&str> for ServerConfig {
    type Error = AddrParseError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Ok(ServerConfig {
            listen: SocketAddr::from_str(value)?,
            denylist: default_denylist(),
            allowlist: default_allowlist(),
            rate_limiting_cache_size: Default::default(),
            rate_limiting_cutoff: Default::default(),
            require_nts: None,
            accept_ntp_versions: default_accepted_ntp_versions(),
        })
    }
}

#[cfg(test)]
impl From<SocketAddr> for ServerConfig {
    fn from(listen: SocketAddr) -> Self {
        ServerConfig {
            listen,
            denylist: default_denylist(),
            allowlist: default_allowlist(),
            rate_limiting_cache_size: Default::default(),
            rate_limiting_cutoff: Default::default(),
            require_nts: None,
            accept_ntp_versions: default_accepted_ntp_versions(),
        }
    }
}

impl From<ServerConfig> for ntp_proto::ServerConfig {
    fn from(value: ServerConfig) -> Self {
        ntp_proto::ServerConfig {
            denylist: value.denylist,
            allowlist: value.allowlist,
            rate_limiting_cache_size: value.rate_limiting_cache_size,
            rate_limiting_cutoff: value.rate_limiting_cutoff,
            require_nts: value.require_nts,
            accepted_versions: value.accept_ntp_versions,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct NtsKeConfig {
    pub certificate_chain_path: PathBuf,
    pub private_key_path: PathBuf,
    #[serde(default)]
    #[cfg(feature = "unstable_nts-pool")]
    pub additional_pool_ca_certificates: Vec<PathBuf>,
    #[serde(default)]
    #[cfg(feature = "unstable_nts-pool")]
    pub accepted_pool_domains: Vec<String>,
    #[serde(default = "default_nts_ke_timeout")]
    pub key_exchange_timeout_ms: u64,
    #[serde(default = "default_concurrent_connections")]
    pub concurrent_connections: usize,
    pub listen: SocketAddr,
    pub ntp_port: Option<u16>,
    pub ntp_server: Option<String>,
    #[serde(
        default = "default_accept_ntp_versions",
        deserialize_with = "deserialize_accepted_ntp_versions_for_nts"
    )]
    pub accept_ntp_versions: Vec<NtpVersion>,
}

fn default_accept_ntp_versions() -> Vec<NtpVersion> {
    vec![NtpVersion::V4]
}

fn deserialize_accepted_ntp_versions_for_nts<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<NtpVersion>, D::Error> {
    let data = Vec::<u8>::deserialize(deserializer)?;

    data.into_iter()
        .map(|v| match v {
            3 => Err(serde::de::Error::custom(
                "Ntp version 3 does not support NTS!",
            )),
            4 => Ok(NtpVersion::V4),
            5 => Ok(NtpVersion::V5),
            e => Err(serde::de::Error::custom(format!(
                "{e} is not a valid NTP version, version must be 4 and/or 5"
            ))),
        })
        .collect::<Result<Vec<NtpVersion>, D::Error>>()
}

fn default_nts_ke_timeout() -> u64 {
    1000
}

fn default_concurrent_connections() -> usize {
    512
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_server() {
        #[derive(Deserialize, Debug)]
        struct TestConfig {
            server: ServerConfig,
        }

        let test: TestConfig = toml::from_str(
            r#"
            [server]
            listen = "0.0.0.0:123"
            "#,
        )
        .unwrap();
        assert_eq!(test.server.listen, "0.0.0.0:123".parse().unwrap());
        // Defaults
        assert_eq!(
            test.server.allowlist.action,
            ntp_proto::FilterAction::Ignore
        );
        assert_eq!(test.server.denylist.action, ntp_proto::FilterAction::Deny);

        let test: TestConfig = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:123"
            rate-limiting-cutoff-ms = 1000
            rate-limiting-cache-size = 32
            "#,
        )
        .unwrap();
        assert_eq!(test.server.listen, "127.0.0.1:123".parse().unwrap());
        assert_eq!(test.server.rate_limiting_cache_size, 32);
        assert_eq!(
            test.server.rate_limiting_cutoff,
            Duration::from_millis(1000)
        );
        assert_eq!(
            test.server.accept_ntp_versions,
            vec![NtpVersion::V3, NtpVersion::V4]
        );

        let test: TestConfig = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:123"

            [server.denylist]
            filter = ["192.168.33.34/24"]
            action = "deny"
            "#,
        )
        .unwrap();
        assert_eq!(test.server.listen, "127.0.0.1:123".parse().unwrap());
        assert_eq!(test.server.denylist.action, ntp_proto::FilterAction::Deny);

        let test = toml::from_str::<TestConfig>(
            r#"
            [server]
            listen = "127.0.0.1:123"

            [server.allowlist]
            filter = ["192.168.33.34/24"]
            "#,
        );
        assert!(test.is_err());

        let test = toml::from_str::<TestConfig>(
            r#"
            [server]
            listen = "127.0.0.1:123"

            [server.denylist]
            action = "deny"
            "#,
        );
        assert!(test.is_err());

        let test = toml::from_str::<TestConfig>(
            r#"
            [server]
            listen = "127.0.0.1:123"
            accept-ntp-versions = [3,4,5]
            "#,
        )
        .unwrap();
        assert_eq!(
            test.server.accept_ntp_versions,
            vec![NtpVersion::V3, NtpVersion::V4, NtpVersion::V5]
        );

        let test = toml::from_str::<TestConfig>(
            r#"
            [server]
            listen = "127.0.0.1:123"
            accept-ntp-versions = [1]
            "#,
        );
        assert!(test.is_err());
    }

    #[test]
    fn test_deserialize_keyset() {
        #[derive(Deserialize, Debug)]
        #[serde(rename_all = "kebab-case", deny_unknown_fields)]
        struct TestConfig {
            keyset: KeysetConfig,
        }

        let test: TestConfig = toml::from_str(
            r#"
            [keyset]
            stale-key-count = 5
            key-rotation-interval = 500
            key-storage-path = "key/storage/path.key"
            "#,
        )
        .unwrap();

        assert_ne!(test.keyset, KeysetConfig::default())
    }

    #[test]
    fn test_deserialize_nts_ke() {
        #[derive(Deserialize, Debug)]
        #[serde(rename_all = "kebab-case", deny_unknown_fields)]
        struct TestConfig {
            nts_ke_server: NtsKeConfig,
        }

        let test: TestConfig = toml::from_str(
            r#"
            [nts-ke-server]
            listen = "0.0.0.0:4460"
            certificate-chain-path = "/foo/bar/baz.pem"
            private-key-path = "spam.der"
            "#,
        )
        .unwrap();

        let pem = PathBuf::from("/foo/bar/baz.pem");
        assert_eq!(test.nts_ke_server.certificate_chain_path, pem);
        assert_eq!(
            test.nts_ke_server.private_key_path,
            PathBuf::from("spam.der")
        );
        assert_eq!(test.nts_ke_server.key_exchange_timeout_ms, 1000,);
        assert_eq!(test.nts_ke_server.listen, "0.0.0.0:4460".parse().unwrap(),);
    }

    #[cfg(feature = "unstable_nts-pool")]
    #[test]
    fn test_deserialize_nts_ke_pool_member() {
        #[derive(Deserialize, Debug)]
        #[serde(rename_all = "kebab-case", deny_unknown_fields)]
        struct TestConfig {
            nts_ke_server: NtsKeConfig,
        }

        let test: TestConfig = toml::from_str(
            r#"
            [nts-ke-server]
            listen = "0.0.0.0:4460"
            certificate-chain-path = "/foo/bar/baz.pem"
            private-key-path = "spam.der"
            additional-pool-ca-certificates = [ "foo.pem", "bar.pem" ]
            accepted-pool-domains = ["a.test", "b.test"]
            "#,
        )
        .unwrap();

        assert_eq!(
            test.nts_ke_server.additional_pool_ca_certificates,
            vec![PathBuf::from("foo.pem"), PathBuf::from("bar.pem")]
        );

        assert_eq!(
            test.nts_ke_server.accepted_pool_domains,
            vec!["a.test".to_string(), "b.test".to_string()]
        );
    }
}
