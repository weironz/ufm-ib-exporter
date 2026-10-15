use crate::config::TelemetryConfig;
use bytes::Bytes;
use futures_util::StreamExt;
use regex::bytes::{Regex, RegexSet};
use smallvec::SmallVec;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollError {
    Network,
    HttpStatus,
    Timeout,
    InputLimit,
    CacheLimit,
    LineLimit,
    CardinalityLimit,
    InvalidSample,
    DuplicateSample,
    SourceOverlap,
    Empty,
    Json,
}

impl PollError {
    pub fn label(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::HttpStatus => "http_status",
            Self::Timeout => "timeout",
            Self::InputLimit => "input_limit",
            Self::CacheLimit => "cache_limit",
            Self::LineLimit => "line_limit",
            Self::CardinalityLimit => "cardinality_limit",
            Self::InvalidSample => "invalid_sample",
            Self::DuplicateSample => "duplicate_sample",
            Self::Empty => "empty_or_stale",
            Self::SourceOverlap => "source_overlap",
            Self::Json => "invalid_json",
        }
    }
}

pub type PollResult<T> = Result<T, PollError>;

pub struct Collected {
    pub data: Bytes,
    pub metadata: Bytes,
    pub input_bytes: usize,
    pub series: usize,
    pub oldest_observation: Option<f64>,
    pub expired_samples: usize,
    pub metric_names: Vec<String>,
}

struct Observation {
    id: usize,
    timestamp: Option<f64>,
}

pub struct Filter {
    config: TelemetryConfig,
    keep: RegexSet,
    node: Option<Regex>,
    label_filters: Vec<(String, Regex)>,
    pending: Vec<u8>,
    body: Vec<u8>,
    labels: Vec<u8>,
    observations: HashMap<Bytes, Observation>,
    names: HashMap<String, usize>,
    identities: HashSet<(usize, usize)>,
    metadata: HashMap<(String, String), Vec<u8>>,
    metadata_bytes: usize,
    input_bytes: usize,
    expired_samples: usize,
    now: f64,
}

pub fn label_name(name: &[u8]) -> bool {
    !name.is_empty()
        && (name[0].is_ascii_alphabetic() || name[0] == b'_')
        && name.iter().all(|x| x.is_ascii_alphanumeric() || *x == b'_')
}

fn metric_name(name: &[u8]) -> bool {
    !name.is_empty()
        && (name[0].is_ascii_alphabetic() || matches!(name[0], b'_' | b':'))
        && name
            .iter()
            .all(|x| x.is_ascii_alphanumeric() || matches!(x, b'_' | b':'))
}

fn reserved(name: &str) -> bool {
    name.starts_with("ufm_")
}

#[derive(Debug)]
struct Sample<'a> {
    name: &'a str,
    labels: SmallVec<[(&'a str, &'a str); 24]>,
    value: &'a str,
    timestamp: Option<f64>,
}

fn parse_sample(line: &[u8]) -> PollResult<Sample<'_>> {
    let text = std::str::from_utf8(line).map_err(|_| PollError::InvalidSample)?;
    let end = line
        .iter()
        .position(|b| *b == b'{' || b.is_ascii_whitespace())
        .unwrap_or(line.len());
    let name = &text[..end];
    if !metric_name(name.as_bytes()) || name.len() > 256 || reserved(name) {
        return Err(PollError::InvalidSample);
    }
    let mut labels = SmallVec::<[(&str, &str); 24]>::new();
    let mut i = end;
    if line.get(i) == Some(&b'{') {
        i += 1;
        loop {
            while line.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            if line.get(i) == Some(&b'}') {
                i += 1;
                break;
            }
            let start = i;
            while line
                .get(i)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
            {
                i += 1;
            }
            if !label_name(&line[start..i]) {
                return Err(PollError::InvalidSample);
            }
            let key = &text[start..i];
            while line.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            if line.get(i) != Some(&b'=') {
                return Err(PollError::InvalidSample);
            }
            i += 1;
            while line.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            if line.get(i) != Some(&b'"') {
                return Err(PollError::InvalidSample);
            }
            i += 1;
            let start = i;
            while let Some(&b) = line.get(i) {
                match b {
                    b'"' => break,
                    b'\\' => {
                        i += 1;
                        if !matches!(line.get(i), Some(b'n' | b'"' | b'\\')) {
                            return Err(PollError::InvalidSample);
                        }
                    }
                    0..=31 => return Err(PollError::InvalidSample),
                    _ => {}
                }
                i += 1;
            }
            if key == "__name__"
                || line.get(i) != Some(&b'"')
                || labels.len() >= 64
                || labels.iter().any(|(k, _)| *k == key)
            {
                return Err(PollError::InvalidSample);
            }
            labels.push((key, &text[start..i]));
            i += 1;
            while line.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            match line.get(i) {
                Some(b',') => i += 1,
                Some(b'}') => {
                    i += 1;
                    break;
                }
                _ => return Err(PollError::InvalidSample),
            }
        }
    }
    if !line.get(i).is_some_and(u8::is_ascii_whitespace) {
        return Err(PollError::InvalidSample);
    }
    let mut fields = text[i..].split_ascii_whitespace();
    let value = fields.next().ok_or(PollError::InvalidSample)?;
    value.parse::<f64>().map_err(|_| PollError::InvalidSample)?;
    let timestamp = fields
        .next()
        .map(|v| v.parse::<f64>().map(|n| n / 1000.0))
        .transpose()
        .map_err(|_| PollError::InvalidSample)?;
    if fields.next().is_some() || timestamp.is_some_and(|v| !v.is_finite() || v <= 0.0) {
        return Err(PollError::InvalidSample);
    }
    Ok(Sample {
        name,
        labels,
        value,
        timestamp,
    })
}

fn decode_label(value: &str) -> std::borrow::Cow<'_, str> {
    if !value.contains('\\') {
        return value.into();
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(if next == 'n' { '\n' } else { next });
            }
        } else {
            out.push(c);
        }
    }
    out.into()
}

impl Filter {
    pub fn new(config: &TelemetryConfig, now: f64) -> anyhow::Result<Self> {
        let (keep, node) = config.filters()?;
        Ok(Self {
            config: config.clone(),
            keep,
            node,
            label_filters: config
                .label_filters
                .iter()
                .map(|(k, v)| Ok((k.clone(), Regex::new(v)?)))
                .collect::<anyhow::Result<_>>()?,
            pending: vec![],
            body: vec![],
            labels: vec![],
            observations: HashMap::new(),
            names: HashMap::new(),
            identities: HashSet::new(),
            metadata: HashMap::new(),
            metadata_bytes: 0,
            input_bytes: 0,
            expired_samples: 0,
            now,
        })
    }

    pub fn feed(&mut self, chunk: &[u8]) -> PollResult<()> {
        self.input_bytes = self
            .input_bytes
            .checked_add(chunk.len())
            .ok_or(PollError::InputLimit)?;
        if self.input_bytes > self.config.max_input_bytes {
            return Err(PollError::InputLimit);
        }
        let mut remaining = chunk;
        while let Some(end) = memchr::memchr(b'\n', remaining) {
            let line = &remaining[..end];
            if self.pending.len() + line.len() > self.config.max_line_bytes {
                return Err(PollError::LineLimit);
            }
            if self.pending.is_empty() {
                self.line(line.strip_suffix(b"\r").unwrap_or(line))?;
            } else {
                let mut complete = std::mem::take(&mut self.pending);
                complete.extend_from_slice(line);
                self.line(complete.strip_suffix(b"\r").unwrap_or(&complete))?;
                complete.clear();
                self.pending = complete;
            }
            remaining = &remaining[end + 1..];
        }
        if self.pending.len() + remaining.len() > self.config.max_line_bytes {
            return Err(PollError::LineLimit);
        }
        self.pending.extend_from_slice(remaining);
        Ok(())
    }

    fn cache_room(&self, addition: usize) -> PollResult<()> {
        if self
            .body
            .len()
            .saturating_add(self.metadata_bytes)
            .saturating_add(addition)
            > self.config.max_cache_bytes
        {
            Err(PollError::CacheLimit)
        } else {
            Ok(())
        }
    }

    fn line(&mut self, line: &[u8]) -> PollResult<()> {
        if line.is_empty() {
            return Ok(());
        }
        if line[0] == b'#' {
            let mut fields = line
                .split(|b| b.is_ascii_whitespace())
                .filter(|s| !s.is_empty());
            let (_, kind, name) = (fields.next(), fields.next(), fields.next());
            if let (Some(kind @ (b"HELP" | b"TYPE")), Some(name)) = (kind, name) {
                if !self.keep.is_match(name) {
                    return Ok(());
                }
                let name = std::str::from_utf8(name).map_err(|_| PollError::InvalidSample)?;
                if !metric_name(name.as_bytes()) || reserved(name) {
                    return Err(PollError::InvalidSample);
                }
                std::str::from_utf8(line).map_err(|_| PollError::InvalidSample)?;
                if kind == b"TYPE"
                    && (!matches!(
                        fields.next(),
                        Some(b"counter" | b"gauge" | b"untyped" | b"histogram" | b"summary")
                    ) || fields.next().is_some())
                {
                    return Err(PollError::InvalidSample);
                }
                let key = (name.to_owned(), String::from_utf8_lossy(kind).into_owned());
                if let Some(existing) = self.metadata.get(&key) {
                    if existing != line {
                        return Err(PollError::InvalidSample);
                    }
                } else {
                    if self.metadata.len() >= self.config.max_metric_names.saturating_mul(2) {
                        return Err(PollError::CardinalityLimit);
                    }
                    self.cache_room(line.len() + 1)?;
                    self.metadata_bytes += line.len() + 1;
                    self.metadata.insert(key, line.to_vec());
                }
            }
            return Ok(());
        }
        let end = line
            .iter()
            .position(|b| *b == b'{' || b.is_ascii_whitespace())
            .unwrap_or(line.len());
        if !self.keep.is_match(&line[..end]) {
            return Ok(());
        }
        let mut sample = parse_sample(line)?;
        if self.node.as_ref().is_some_and(|filter| {
            !sample.labels.iter().any(|(k, v)| {
                matches!(*k, "node_description" | "Port_Name")
                    && filter.is_match(decode_label(v).as_bytes())
            })
        }) {
            return Ok(());
        }
        if !self.label_filters.iter().all(|(key, filter)| {
            sample
                .labels
                .iter()
                .any(|(k, v)| k == key && filter.is_match(decode_label(v).as_bytes()))
        }) {
            return Ok(());
        }
        if self.config.require_timestamps && sample.timestamp.is_none() {
            return Err(PollError::InvalidSample);
        }
        if sample.timestamp.is_some_and(|t| {
            t > self.now + 60.0 || self.now - t > self.config.max_observation_age_seconds as f64
        }) {
            self.expired_samples += 1;
            return Ok(());
        }
        // Canonical label order makes duplicate detection independent of input order.
        sample
            .labels
            .retain(|(k, _)| !self.config.drop_labels.iter().any(|drop| drop == k));
        if sample.labels.iter().any(|(k, _)| *k == "telemetry_source") {
            return Err(PollError::InvalidSample);
        }
        sample.labels.sort_unstable_by_key(|(k, _)| *k);
        self.labels.clear();
        for (i, (key, value)) in sample.labels.iter().enumerate() {
            if i > 0 {
                self.labels.push(b',');
            }
            self.labels.extend_from_slice(key.as_bytes());
            self.labels.extend_from_slice(b"=\"");
            self.labels.extend_from_slice(value.as_bytes());
            self.labels.push(b'"');
        }
        let observation_id =
            if let Some(observation) = self.observations.get_mut(self.labels.as_slice()) {
                observation.timestamp = observation
                    .timestamp
                    .zip(sample.timestamp)
                    .map(|(old, ts)| old.min(ts));
                observation.id
            } else {
                if self.observations.len() >= self.config.max_label_sets {
                    return Err(PollError::CardinalityLimit);
                }
                let id = self.observations.len();
                self.observations.insert(
                    Bytes::copy_from_slice(&self.labels),
                    Observation {
                        id,
                        timestamp: sample.timestamp,
                    },
                );
                id
            };
        let name_id = if let Some(id) = self.names.get(sample.name) {
            *id
        } else {
            if self.names.len() >= self.config.max_metric_names {
                return Err(PollError::CardinalityLimit);
            }
            let id = self.names.len();
            self.names.insert(sample.name.to_owned(), id);
            id
        };
        if self.identities.len() >= self.config.max_series {
            return Err(PollError::CardinalityLimit);
        }
        if !self.identities.insert((name_id, observation_id)) {
            return Err(PollError::DuplicateSample);
        }
        // Reserve all bytes before appending; no unbounded intermediate formatted line.
        self.cache_room(sample.name.len() + self.labels.len() + sample.value.len() + 40)?;
        self.body.extend_from_slice(sample.name.as_bytes());
        if !self.labels.is_empty() {
            self.body.push(b'{');
            self.body.extend_from_slice(&self.labels);
            self.body.push(b'}');
        }
        self.body.push(b' ');
        self.body.extend_from_slice(sample.value.as_bytes());
        if self.config.preserve_timestamps
            && let Some(ts) = sample.timestamp
        {
            self.body
                .extend_from_slice(format!(" {:.0}", ts * 1000.0).as_bytes());
        }
        self.body.push(b'\n');
        Ok(())
    }

    pub fn end_endpoint(&mut self) -> PollResult<()> {
        if !self.pending.is_empty() {
            let mut pending = std::mem::take(&mut self.pending);
            self.line(&pending)?;
            pending.clear();
            self.pending = pending;
        }
        Ok(())
    }

    pub fn finish(mut self, source: &str) -> PollResult<Collected> {
        self.end_endpoint()?;
        if self.identities.is_empty() {
            return Err(PollError::Empty);
        }
        // Send TYPE/HELP before samples using a separate shared buffer.
        let mut metadata = Vec::with_capacity(self.metadata_bytes);
        let mut metric_names: HashSet<String> =
            std::mem::take(&mut self.names).into_keys().collect();
        for ((name, _), line) in std::mem::take(&mut self.metadata) {
            metric_names.insert(name);
            metadata.extend_from_slice(&line);
            metadata.push(b'\n');
        }
        let mut oldest: Option<f64> = None;
        for (labels, observation) in std::mem::take(&mut self.observations) {
            if let Some(timestamp) = observation.timestamp {
                oldest = Some(oldest.map_or(timestamp, |old| old.min(timestamp)));
                let prefix = b"ufm_telemetry_observed_at_seconds{";
                let suffix = format!("telemetry_source=\"{source}\"}} {timestamp:.3}\n");
                self.cache_room(prefix.len() + labels.len() + 1 + suffix.len())?;
                self.body.extend_from_slice(prefix);
                self.body.extend_from_slice(&labels);
                if !labels.is_empty() {
                    self.body.push(b',');
                }
                self.body.extend_from_slice(suffix.as_bytes());
            }
        }
        Ok(Collected {
            data: self.body.into(),
            metadata: metadata.into(),
            input_bytes: self.input_bytes,
            series: self.identities.len(),
            oldest_observation: oldest,
            expired_samples: self.expired_samples,
            metric_names: metric_names.into_iter().collect(),
        })
    }
}

pub async fn poll(
    client: &reqwest::Client,
    config: &TelemetryConfig,
    source: &str,
) -> PollResult<Collected> {
    let mut filter =
        Filter::new(config, crate::unix_time()).map_err(|_| PollError::InvalidSample)?;
    let urls = if config.urls.is_empty() {
        vec![config.url.as_str()]
    } else {
        config.urls.iter().map(String::as_str).collect()
    };
    for url in urls {
        let response = client
            .get(url)
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
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| PollError::Network)?;
            for piece in chunk.chunks(64 * 1024) {
                filter.now = crate::unix_time();
                filter.feed(piece)?;
                tokio::task::yield_now().await;
            }
        }
        // An HTTP endpoint boundary is also a line boundary, even if a shard omits LF.
        filter.end_endpoint()?;
    }
    filter.finish(source)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cfg() -> TelemetryConfig {
        TelemetryConfig {
            keep: vec![".*".into()],
            ..TelemetryConfig::counters()
        }
    }
    fn run(c: &TelemetryConfig, data: &[u8], chunk: usize) -> PollResult<Collected> {
        let mut f = Filter::new(c, 1000.0).unwrap();
        for b in data.chunks(chunk) {
            f.feed(b)?;
        }
        f.finish("9002")
    }

    #[test]
    fn chunk_boundaries_utf8_labels_and_native_time() {
        let data = "# TYPE cable_temperature gauge\ncable_temperature{Port_Name=\"B300-01端口\\\"x\",SN=\"a}b\"} 42 999000\n";
        for n in 1..=data.len() {
            let result = run(&cfg(), data.as_bytes(), n).unwrap();
            let text = std::str::from_utf8(&result.data).unwrap();
            assert!(text.contains("cable_temperature{Port_Name="));
            assert!(text.contains("telemetry_source=\"9002\"} 999.000"));
            assert_eq!(result.oldest_observation, Some(999.0));
        }
    }

    #[test]
    fn filters_before_parsing_and_accepts_cableinfo_names() {
        let c = TelemetryConfig {
            node_filter: Some("(?i)b300".into()),
            ..TelemetryConfig::optics()
        };
        let data = b"irrelevant_this_is_not_valid_prometheus\nrx_power_lane0{Port_Name=\"b300-01\",Temperature=\"42\"} 1.2 999000\n";
        let result = run(&c, data, 7).unwrap();
        assert_eq!(result.series, 1);
        assert!(
            !std::str::from_utf8(&result.data)
                .unwrap()
                .contains("Temperature=")
        );
    }

    #[test]
    fn rejects_duplicates_after_label_drop_and_permutation() {
        let mut c = cfg();
        c.drop_labels = vec!["Temperature".into()];
        for data in [
            "x{a=\"b\",Temperature=\"1\"} 1 999000\nx{Temperature=\"2\",a=\"b\"} 2 999000\n",
            "x{a=\"b\",c=\"d\"} 1 999000\nx{c=\"d\",a=\"b\"} 2 999000\n",
        ] {
            assert_eq!(
                run(&c, data.as_bytes(), 17).err(),
                Some(PollError::DuplicateSample)
            );
        }
    }

    #[test]
    fn expired_future_missing_and_malformed_observations() {
        for data in [b"x 1 100000\n".as_slice(), b"x 1 1100000\n"] {
            assert_eq!(run(&cfg(), data, 4).err(), Some(PollError::Empty));
        }
        for data in [
            "x 1\n",
            "x{a=\"b\",a=\"c\"} 1 999000\n",
            "x{a=\"bad\\q\"} 1 999000\n",
            "ufm_up 1 999000\n",
            "x 1 NaN\n",
        ] {
            assert_eq!(
                run(&cfg(), data.as_bytes(), 9).err(),
                Some(PollError::InvalidSample)
            );
        }
    }

    #[test]
    fn enforces_input_line_cache_and_cardinality_budgets() {
        let data = b"x{a=\"b\"} 1 999000\n";
        let mut c = cfg();
        c.max_input_bytes = 2;
        assert_eq!(run(&c, data, 1).err(), Some(PollError::InputLimit));
        c = cfg();
        c.max_line_bytes = 3;
        assert_eq!(run(&c, data, 1).err(), Some(PollError::LineLimit));
        c = cfg();
        c.max_cache_bytes = 16;
        assert_eq!(run(&c, data, 2).err(), Some(PollError::CacheLimit));
        c = cfg();
        c.max_label_sets = 1;
        assert_eq!(
            run(&c, b"x{a=\"1\"} 1 999000\nx{a=\"2\"} 1 999000\n", 30).err(),
            Some(PollError::CardinalityLimit)
        );
    }

    #[test]
    fn empty_and_http_fresh_but_frozen_data_are_not_success() {
        for data in [b"".as_slice(), b"# HELP x hello\n", b"x 1 100000\n"] {
            assert_eq!(run(&cfg(), data, 1).err(), Some(PollError::Empty));
        }
    }

    #[test]
    fn preserve_timestamp_and_no_trailing_newline() {
        let c = TelemetryConfig {
            preserve_timestamps: true,
            ..cfg()
        };
        let result = run(&c, b"x 1 999000", 1).unwrap();
        assert!(result.data.starts_with(b"x 1 999000\n"));
    }

    #[test]
    fn label_filters_and_metadata_are_validated() {
        let mut c = cfg();
        c.label_filters.insert("port_num".into(), "^2$".into());
        let result = run(
            &c,
            b"x{port_num=\"1\"} 1 999000\nx{port_num=\"2\"} 2 999000\n",
            3,
        )
        .unwrap();
        assert_eq!(result.series, 1);
        assert!(
            !std::str::from_utf8(&result.data)
                .unwrap()
                .contains("port_num=\"1\"")
        );
        assert_eq!(
            run(&cfg(), b"# TYPE x not_a_type\nx 1 999000\n", 4).err(),
            Some(PollError::InvalidSample)
        );
    }

    #[test]
    fn endpoint_boundaries_do_not_join_lines_or_inflate_input_bytes() {
        let mut filter = Filter::new(&cfg(), 1000.0).unwrap();
        filter.feed(b"x{port=\"1\"} 1 999000").unwrap();
        filter.end_endpoint().unwrap();
        filter.feed(b"x{port=\"2\"} 2 999000").unwrap();
        let result = filter.finish("9001").unwrap();
        assert_eq!(result.series, 2);
        assert_eq!(result.input_bytes, 40);
    }
}
