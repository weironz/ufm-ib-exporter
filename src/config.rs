use anyhow::{Context, Result, bail, ensure};
use regex::bytes::{Regex, RegexSet};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, env, net::SocketAddr, path::Path};

pub const ERRORS: &str = "^(SymbolErrorCounter|LinkErrorRecoveryCounter|LinkDownedCounter|PortRcvErrors|PortRcvRemotePhysicalErrors|PortRcvSwitchRelayErrors|PortXmitDiscards|LocalLinkIntegrityErrors|ExcessiveBufferOverrunErrors|PortXmitWait|phy_c_symbol_errors|phy_c_sync_headers_errors|phy_c_rs_fec_corrected_symbols_total)";
pub const OPTICS: &str = "^(rx_power_lane[0-7]|tx_power_lane[0-7]|cable_temperature)$";

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub worker_threads: usize,
    pub max_concurrent_scrapes: usize,
    pub max_heavy_polls: usize,
    pub ca_file: Option<String>,
    pub rest: RestConfig,
    pub telemetry_9001: TelemetryConfig,
    #[serde(
        default = "TelemetryConfig::optics",
        deserialize_with = "deserialize_optics"
    )]
    pub telemetry_9002: TelemetryConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:9627".parse().unwrap(),
            worker_threads: 2,
            max_concurrent_scrapes: 2,
            max_heavy_polls: 1,
            ca_file: None,
            rest: RestConfig::default(),
            telemetry_9001: TelemetryConfig::counters(),
            telemetry_9002: TelemetryConfig::optics(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RestConfig {
    pub enabled: bool,
    pub url: String,
    pub username: String,
    #[serde(skip_serializing)]
    pub password: String,
    pub interval_seconds: u64,
    pub timeout_seconds: u64,
    pub max_age_seconds: u64,
    pub max_input_bytes: usize,
    pub max_cache_bytes: usize,
    pub node_prefix: String,
    pub expected_nodes: usize,
    pub ports_per_node: usize,
}

impl Default for RestConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: "https://127.0.0.1/ufmRest/resources/ports".into(),
            username: "admin".into(),
            password: String::new(),
            interval_seconds: 60,
            timeout_seconds: 30,
            max_age_seconds: 180,
            max_input_bytes: 16 * 1024 * 1024,
            max_cache_bytes: 8 * 1024 * 1024,
            node_prefix: "B300".into(),
            expected_nodes: 32,
            ports_per_node: 8,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub url: String,
    /// Optional sequential native shards/counter sets. A failed part rejects the entire update.
    pub urls: Vec<String>,
    pub interval_seconds: u64,
    pub timeout_seconds: u64,
    pub max_age_seconds: u64,
    pub max_observation_age_seconds: u64,
    pub require_timestamps: bool,
    pub preserve_timestamps: bool,
    pub max_input_bytes: usize,
    pub max_cache_bytes: usize,
    pub max_line_bytes: usize,
    pub max_series: usize,
    pub max_label_sets: usize,
    pub max_metric_names: usize,
    pub keep: Vec<String>,
    pub node_filter: Option<String>,
    pub label_filters: BTreeMap<String, String>,
    pub drop_labels: Vec<String>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self::counters()
    }
}

impl TelemetryConfig {
    pub fn counters() -> Self {
        Self {
            enabled: false,
            url: "http://127.0.0.1:9001/metrics".into(),
            urls: vec![],
            interval_seconds: 180,
            timeout_seconds: 120,
            max_age_seconds: 420,
            max_observation_age_seconds: 420,
            require_timestamps: true,
            preserve_timestamps: false,
            max_input_bytes: 512 * 1024 * 1024,
            max_cache_bytes: 64 * 1024 * 1024,
            max_line_bytes: 256 * 1024,
            max_series: 250_000,
            max_label_sets: 30_000,
            max_metric_names: 1024,
            keep: vec![ERRORS.into()],
            node_filter: None,
            label_filters: BTreeMap::new(),
            drop_labels: vec![],
        }
    }

    pub fn optics() -> Self {
        Self {
            url: "http://127.0.0.1:9002/metrics/CableInfo".into(),
            interval_seconds: 600,
            timeout_seconds: 300,
            max_age_seconds: 1260,
            max_observation_age_seconds: 1260,
            max_input_bytes: 1536 * 1024 * 1024,
            max_cache_bytes: 128 * 1024 * 1024,
            keep: vec![OPTICS.into()],
            drop_labels: vec!["Temperature".into()],
            ..Self::counters()
        }
    }

    pub fn filters(&self) -> Result<(RegexSet, Option<Regex>)> {
        let keep = RegexSet::new(&self.keep).context("invalid metric whitelist")?;
        let node = self
            .node_filter
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(Regex::new)
            .transpose()
            .context("invalid node filter")?;
        Ok((keep, node))
    }
}

fn deserialize_optics<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<TelemetryConfig, D::Error> {
    let supplied = serde_yaml::Value::deserialize(deserializer)?;
    let mut base =
        serde_yaml::to_value(TelemetryConfig::optics()).map_err(serde::de::Error::custom)?;
    let supplied = supplied
        .as_mapping()
        .ok_or_else(|| serde::de::Error::custom("telemetry_9002 must be a mapping"))?;
    base.as_mapping_mut().unwrap().extend(supplied.clone());
    serde_yaml::from_value(base).map_err(serde::de::Error::custom)
}

fn get(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn number<T: std::str::FromStr>(key: &str, value: &mut T) -> Result<()> {
    if let Some(v) = get(key) {
        *value = v
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid value for {key}"))?;
    }
    Ok(())
}

fn boolean(key: &str, value: &mut bool) -> Result<()> {
    if let Some(v) = get(key) {
        *value = match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => bail!("invalid boolean for {key}"),
        };
    }
    Ok(())
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut c = if let Some(path) = path {
            ensure!(
                std::fs::metadata(path)
                    .context("cannot stat configuration")?
                    .len()
                    <= 1024 * 1024,
                "configuration exceeds 1 MiB"
            );
            let data = std::fs::read(path).context("cannot read configuration")?;
            serde_yaml::from_slice::<Self>(&data)
                .map_err(|e| anyhow::anyhow!("invalid YAML configuration at {:?}", e.location()))?
        } else {
            Self::default()
        };
        if let Some(addr) = get("LISTEN_ADDRESS") {
            c.listen
                .set_ip(addr.parse().context("invalid LISTEN_ADDRESS")?);
        }
        let mut port = c.listen.port();
        number("LISTEN_PORT", &mut port)?;
        c.listen.set_port(port);
        if let Some(host) = get("UFM_HOST") {
            ensure!(!host.contains(['/', '@', '?', '#']), "invalid UFM_HOST");
            c.rest.url = format!("https://{host}/ufmRest/resources/ports");
        }
        if let Some(v) = get("UFM_REST_URL") {
            c.rest.url = v;
        }
        if let Some(v) = get("UFM_USERNAME") {
            c.rest.username = v;
        }
        if let Ok(v) = env::var("UFM_PASSWORD") {
            c.rest.password = v;
        }
        if let Some(v) = get("UFM_CA_FILE") {
            c.ca_file = Some(v);
        }
        boolean("UFM_COLLECT_REST", &mut c.rest.enabled)?;
        number("UFM_REST_TIMEOUT", &mut c.rest.timeout_seconds)?;
        number("UFM_REST_INTERVAL", &mut c.rest.interval_seconds)?;
        number("UFM_REST_MAX_AGE", &mut c.rest.max_age_seconds)?;
        if let Some(v) = get("UFM_NODE_PREFIX") {
            c.rest.node_prefix = v;
        }
        number("UFM_EXPECTED_NODES", &mut c.rest.expected_nodes)?;
        number("UFM_PORTS_PER_NODE", &mut c.rest.ports_per_node)?;
        for (name, source) in [
            ("9001", &mut c.telemetry_9001),
            ("9002", &mut c.telemetry_9002),
        ] {
            boolean(&format!("UFM_COLLECT_{name}"), &mut source.enabled)?;
            if let Some(v) = get(&format!("UFM_{name}_URL")) {
                source.url = v;
                source.urls.clear();
            }
            if let Some(v) = get(&format!("UFM_{name}_KEEP")) {
                source.keep = vec![v];
            }
            if let Some(v) = get(&format!("UFM_{name}_NODE_FILTER")) {
                source.node_filter = Some(v);
            }
            number(
                &format!("UFM_{name}_INTERVAL"),
                &mut source.interval_seconds,
            )?;
            number(
                &format!("UFM_{name}_MAX_POLL_SECONDS"),
                &mut source.timeout_seconds,
            )?;
            number(
                &format!("UFM_{name}_MAX_BYTES"),
                &mut source.max_input_bytes,
            )?;
            number(
                &format!("UFM_{name}_MAX_CACHE_BYTES"),
                &mut source.max_cache_bytes,
            )?;
            number(
                &format!("UFM_{name}_MAX_OBSERVATION_AGE"),
                &mut source.max_observation_age_seconds,
            )?;
            boolean(
                &format!("UFM_{name}_REQUIRE_TIMESTAMPS"),
                &mut source.require_timestamps,
            )?;
        }
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=16).contains(&self.worker_threads),
            "worker_threads must be 1..16"
        );
        ensure!(
            (1..=16).contains(&self.max_concurrent_scrapes),
            "max_concurrent_scrapes must be 1..16"
        );
        ensure!(
            (1..=2).contains(&self.max_heavy_polls),
            "max_heavy_polls must be 1..2"
        );
        check_url(&self.rest.url)?;
        ensure!(
            !self.rest.enabled || !self.rest.password.is_empty(),
            "REST is enabled: set UFM_PASSWORD through the environment"
        );
        ensure!(
            self.rest.interval_seconds > 0
                && self.rest.timeout_seconds > 0
                && self.rest.max_age_seconds > 0,
            "REST time budgets must be positive"
        );
        ensure!(
            [
                self.rest.interval_seconds,
                self.rest.timeout_seconds,
                self.rest.max_age_seconds
            ]
            .iter()
            .all(|n| *n <= 31_536_000),
            "REST time budgets must not exceed one year"
        );
        ensure!(
            self.rest.max_input_bytes > 0 && self.rest.max_cache_bytes > 0,
            "REST byte budgets must be positive"
        );
        ensure!(
            !self.rest.node_prefix.is_empty()
                && self.rest.expected_nodes <= 100_000
                && self.rest.ports_per_node <= 100_000,
            "invalid REST baseline"
        );
        for s in [&self.telemetry_9001, &self.telemetry_9002] {
            check_url(&s.url)?;
            ensure!(
                s.urls.len() <= 64,
                "at most 64 sequential endpoints per source"
            );
            for url in &s.urls {
                check_url(url)?;
            }
            ensure!(
                s.interval_seconds > 0
                    && s.timeout_seconds > 0
                    && s.max_age_seconds > 0
                    && s.max_observation_age_seconds > 0,
                "telemetry time budgets must be positive"
            );
            ensure!(
                [
                    s.interval_seconds,
                    s.timeout_seconds,
                    s.max_age_seconds,
                    s.max_observation_age_seconds
                ]
                .iter()
                .all(|n| *n <= 31_536_000),
                "telemetry time budgets must not exceed one year"
            );
            ensure!(
                s.max_input_bytes > 0
                    && s.max_cache_bytes > 0
                    && s.max_line_bytes >= 128
                    && s.max_series > 0
                    && s.max_label_sets > 0
                    && s.max_metric_names > 0,
                "invalid telemetry budgets"
            );
            ensure!(
                !s.keep.is_empty(),
                "telemetry whitelist cannot be empty; use .* explicitly to retain everything"
            );
            ensure!(
                s.drop_labels
                    .iter()
                    .all(|x| super::telemetry::label_name(x.as_bytes())),
                "invalid label drop list"
            );
            s.filters()?;
            for (label, pattern) in &s.label_filters {
                ensure!(
                    super::telemetry::label_name(label.as_bytes()),
                    "invalid label filter name"
                );
                Regex::new(pattern).context("invalid label filter regex")?;
            }
        }
        Ok(())
    }

    pub fn same_runtime(&self, next: &Self) -> bool {
        self.listen == next.listen
            && self.worker_threads == next.worker_threads
            && self.max_heavy_polls == next.max_heavy_polls
            && self.max_concurrent_scrapes == next.max_concurrent_scrapes
            && self.ca_file == next.ca_file
    }
}

fn check_url(value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value).map_err(|_| anyhow::anyhow!("invalid source URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "source URLs must be HTTP(S), without embedded credentials or fragments"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_fields_and_unsafe_urls() {
        assert!(serde_yaml::from_str::<Config>("typo: true").is_err());
        assert!(check_url("https://user:secret@example.com/metrics").is_err());
        assert!(check_url("file:///etc/passwd").is_err());
        assert!(check_url("http://127.0.0.1:9001/metrics?num_shards=2&shard=0").is_ok());
    }

    #[test]
    fn defaults_are_bounded_and_optics_specific() {
        let mut c = Config::default();
        c.rest.password = "synthetic-test-password".into();
        c.validate().unwrap();
        assert_eq!(c.max_heavy_polls, 1);
        let (keep, _) = c.telemetry_9002.filters().unwrap();
        assert!(keep.is_match(b"cable_temperature"));
        assert!(!keep.is_match(b"dev_temperature"));
        let encoded = serde_yaml::to_string(&c).unwrap();
        assert!(!encoded.contains("password:"));
        let partial: Config = serde_yaml::from_str("telemetry_9002:\n  enabled: true\n").unwrap();
        assert_eq!(partial.telemetry_9002.url, c.telemetry_9002.url);
        assert_eq!(partial.telemetry_9002.keep, c.telemetry_9002.keep);
        assert!(serde_yaml::from_str::<Config>("telemetry_9002:\n  misspelled: 1\n").is_err());
    }
}
