use crate::{
    config::RestConfig,
    telemetry::{Collected, PollError, PollResult},
};
use bytes::Bytes;
use futures_util::StreamExt;
use regex::Regex;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write;

#[derive(Deserialize)]
struct Port {
    #[serde(default)]
    has_plane_ports: Option<bool>,
    #[serde(default)]
    node_description: Option<String>,
    #[serde(default)]
    peer_node_name: Option<String>,
    #[serde(default)]
    peer_port_dname: Option<String>,
    #[serde(default)]
    active_speed: Option<String>,
    #[serde(default)]
    logical_state: Option<String>,
}

pub fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn gauge(out: &mut String, name: &str, help: &str, value: usize) {
    writeln!(
        out,
        "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
    )
    .unwrap();
}

pub fn parse(data: &[u8], config: &RestConfig) -> PollResult<Collected> {
    let ports: Vec<Port> = serde_json::from_slice(data).map_err(|_| PollError::Json)?;
    let matcher = Regex::new(&format!(
        "(?i)^{}-(\\d+)(?:$|[^0-9])",
        regex::escape(&config.node_prefix)
    ))
    .map_err(|_| PollError::InvalidSample)?;
    let mut links = String::new();
    let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut seen = HashSet::new();
    let mut series = 0;
    for port in ports {
        if port.has_plane_ports != Some(true) {
            continue;
        }
        let description = port.node_description.as_deref().unwrap_or("");
        let Some(capture) = matcher.captures(description) else {
            continue;
        };
        let raw_number = capture.get(1).unwrap();
        let number: usize = raw_number
            .as_str()
            .parse()
            .map_err(|_| PollError::InvalidSample)?;
        // Use the numeric group's end, not the regex boundary delimiter, to slice the card.
        let node = format!("{}-{number:02}", config.node_prefix);
        let card = description[raw_number.end()..].trim_start_matches([':', ' ']);
        let card = if card.is_empty() { "?" } else { card };
        if !seen.insert((node.clone(), card.to_owned())) {
            return Err(PollError::DuplicateSample);
        }
        let up = usize::from(port.logical_state.as_deref() == Some("Active"));
        let labels = [
            node.as_str(),
            card,
            port.peer_node_name.as_deref().unwrap_or(""),
            port.peer_port_dname.as_deref().unwrap_or(""),
            port.active_speed.as_deref().unwrap_or(""),
        ]
        .map(escape);
        writeln!(
            links,
            "ufm_ib_link_up{{node=\"{}\",card=\"{}\",peer=\"{}\",cage=\"{}\",speed=\"{}\"}} {up}",
            labels[0], labels[1], labels[2], labels[3], labels[4]
        )
        .unwrap();
        let count = counts.entry(node).or_default();
        count.0 += up;
        count.1 += 1;
        series += 1;
        if links.len() > config.max_cache_bytes {
            return Err(PollError::CacheLimit);
        }
    }
    let mut out = String::from(
        "# HELP ufm_ib_link_up Physical CX8 link state from UFM\n# TYPE ufm_ib_link_up gauge\n",
    );
    out.push_str(&links);
    for (metric, which) in [
        ("ufm_ib_node_links_up", 0),
        ("ufm_ib_node_links_total", 1),
        ("ufm_ib_node_links_expected", 2),
    ] {
        writeln!(
            out,
            "# HELP {metric} Per-node physical CX8 links\n# TYPE {metric} gauge"
        )
        .unwrap();
        for number in 1..=config.expected_nodes {
            let node = format!("{}-{number:02}", config.node_prefix);
            let count = counts.get(&node).copied().unwrap_or_default();
            writeln!(
                out,
                "{metric}{{node=\"{}\"}} {}",
                escape(&node),
                match which {
                    0 => count.0,
                    1 => count.1,
                    _ => config.ports_per_node,
                }
            )
            .unwrap();
            if out.len() > config.max_cache_bytes {
                return Err(PollError::CacheLimit);
            }
        }
    }
    let up: usize = counts.values().map(|c| c.0).sum();
    let expected = config.expected_nodes * config.ports_per_node;
    gauge(
        &mut out,
        "ufm_ib_links_expected",
        "Expected physical CX8 links",
        expected,
    );
    gauge(
        &mut out,
        "ufm_ib_links_up",
        "Visible active physical CX8 links",
        up,
    );
    gauge(
        &mut out,
        "ufm_ib_links_missing",
        "Baseline shortfall; not a hardware fault count",
        expected.saturating_sub(up),
    );
    if out.len() > config.max_cache_bytes {
        return Err(PollError::CacheLimit);
    }
    Ok(Collected {
        data: Bytes::from(out),
        metadata: Bytes::new(),
        input_bytes: data.len(),
        series: series + 3 * config.expected_nodes + 3,
        oldest_observation: None,
        expired_samples: 0,
        metric_names: vec![],
    })
}

pub async fn poll(client: &reqwest::Client, config: &RestConfig) -> PollResult<Collected> {
    let response = client
        .get(&config.url)
        .basic_auth(&config.username, Some(&config.password))
        .send()
        .await
        .map_err(|_| PollError::Network)?;
    if !response.status().is_success() {
        return Err(PollError::HttpStatus);
    }
    if response
        .content_length()
        .is_some_and(|n| n > config.max_input_bytes as u64)
    {
        return Err(PollError::InputLimit);
    }
    let mut stream = response.bytes_stream();
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| PollError::Network)?;
        if data.len().saturating_add(chunk.len()) > config.max_input_bytes {
            return Err(PollError::InputLimit);
        }
        data.extend_from_slice(&chunk);
    }
    parse(&data, config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_ports_normalization_and_missing_nodes() {
        let data = br#"[
          {"has_plane_ports":true,"node_description":"b300-1:card0","logical_state":"Active","peer_node_name":"Q3400","peer_port_dname":"1/1","active_speed":"800"},
          {"has_plane_ports":false,"node_description":"B300-1:plane0","logical_state":"Active"},
          {"has_plane_ports":true,"node_description":"B300-01 mlx5_8","logical_state":"Down"},
          {"has_plane_ports":true,"node_description":"Q3400-1:port1","logical_state":"Active"}
        ]"#;
        let c = RestConfig {
            expected_nodes: 2,
            ports_per_node: 8,
            ..RestConfig::default()
        };
        let result = parse(data, &c).unwrap();
        let s = std::str::from_utf8(&result.data).unwrap();
        assert!(s.contains("node=\"B300-01\",card=\"card0\""));
        assert!(s.contains("ufm_ib_node_links_up{node=\"B300-02\"} 0"));
        assert!(s.contains("ufm_ib_links_missing 15"));
        assert!(s.contains("ufm_ib_node_links_total{node=\"B300-01\"} 2"));
        assert!(!s.contains("plane0"));
    }

    #[test]
    fn empty_fabric_is_valid_but_invalid_json_is_not() {
        let c = RestConfig::default();
        assert!(parse(b"[]", &c).is_ok());
        assert_eq!(parse(b"{}", &c).err(), Some(PollError::Json));
    }
}
