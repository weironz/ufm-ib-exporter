mod config;
mod rest;
mod telemetry;

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::Response,
    routing::get,
};
use bytes::Bytes;
use config::Config;
use std::{
    collections::VecDeque,
    convert::Infallible,
    fmt::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use telemetry::{Collected, PollError};
use tokio::sync::{Mutex, RwLock, Semaphore, watch};

pub fn unix_time() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

struct Cached {
    result: Collected,
    finished: Instant,
    generation: u64,
}

#[derive(Default)]
struct SourceState {
    cache: Option<Arc<Cached>>,
    success: bool,
    last_error: Option<PollError>,
    duration: f64,
    successful_polls: u64,
    failed_polls: u64,
}

struct App {
    config: RwLock<Arc<Config>>,
    generation: AtomicU64,
    sources: [RwLock<SourceState>; 3],
    publication: Mutex<()>,
    heavy: Arc<Semaphore>,
    scrapes: Arc<Semaphore>,
    reload_successes: AtomicU64,
    reload_failures: AtomicU64,
}

fn enabled(c: &Config, index: usize) -> bool {
    match index {
        0 => c.rest.enabled,
        1 => c.telemetry_9001.enabled,
        _ => c.telemetry_9002.enabled,
    }
}

fn budgets(c: &Config, index: usize) -> (u64, u64, u64) {
    match index {
        0 => (
            c.rest.interval_seconds,
            c.rest.timeout_seconds,
            c.rest.max_age_seconds,
        ),
        1 => (
            c.telemetry_9001.interval_seconds,
            c.telemetry_9001.timeout_seconds,
            c.telemetry_9001.max_age_seconds,
        ),
        _ => (
            c.telemetry_9002.interval_seconds,
            c.telemetry_9002.timeout_seconds,
            c.telemetry_9002.max_age_seconds,
        ),
    }
}

fn healthy(state: &SourceState, config: &Config, index: usize, generation: u64, now: f64) -> bool {
    if !enabled(config, index) || !state.success {
        return false;
    }
    let Some(cache) = &state.cache else {
        return false;
    };
    if cache.generation != generation
        || cache.finished.elapsed().as_secs_f64() > budgets(config, index).2 as f64
    {
        return false;
    }
    if index > 0 {
        let c = if index == 1 {
            &config.telemetry_9001
        } else {
            &config.telemetry_9002
        };
        if cache
            .result
            .oldest_observation
            .is_some_and(|ts| ts > now + 60.0 || now - ts > c.max_observation_age_seconds as f64)
        {
            return false;
        }
    }
    true
}

async fn worker(
    app: Arc<App>,
    client: reqwest::Client,
    index: usize,
    mut updates: watch::Receiver<(Arc<Config>, u64)>,
) {
    let source = ["rest", "9001", "9002"][index];
    loop {
        let (config, generation) = updates.borrow_and_update().clone();
        if !enabled(&config, index) {
            if updates.changed().await.is_err() {
                return;
            }
            continue;
        }
        let (interval, timeout, _) = budgets(&config, index);
        let poll = async {
            // The heavy-data permit is held through parsing and publication.
            let permit = if index > 0 {
                Some(app.heavy.clone().acquire_owned().await.unwrap())
            } else {
                None
            };
            let start = Instant::now();
            let request = async {
                match index {
                    0 => rest::poll(&client, &config.rest).await,
                    1 => telemetry::poll(&client, &config.telemetry_9001, source).await,
                    _ => telemetry::poll(&client, &config.telemetry_9002, source).await,
                }
            };
            let mut result = tokio::time::timeout(Duration::from_secs(timeout), request)
                .await
                .unwrap_or(Err(PollError::Timeout));
            let _publication = app.publication.lock().await;
            if app.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            if index > 0
                && let Ok(candidate) = &result
            {
                let other = if index == 1 { 2 } else { 1 };
                let state = app.sources[other].read().await;
                if healthy(&state, &config, other, generation, unix_time())
                    && state.cache.as_ref().is_some_and(|cached| {
                        candidate
                            .metric_names
                            .iter()
                            .any(|n| cached.result.metric_names.contains(n))
                    })
                {
                    result = Err(PollError::SourceOverlap);
                }
            }
            let mut state = app.sources[index].write().await;
            state.duration = start.elapsed().as_secs_f64();
            match result {
                Ok(result) => {
                    state.cache = Some(Arc::new(Cached {
                        result,
                        finished: Instant::now(),
                        generation,
                    }));
                    state.success = true;
                    state.last_error = None;
                    state.successful_polls += 1;
                    tracing::info!(source, "collection completed");
                }
                Err(error) => {
                    state.success = false;
                    state.last_error = Some(error);
                    state.failed_polls += 1;
                    // Retain the snapshot for diagnostics, but never emit it while failed.
                    tracing::warn!(source, reason = error.label(), "collection unavailable");
                }
            }
            drop(permit);
        };
        tokio::select! {
            _ = poll => {},
            change = updates.changed() => { if change.is_err() { return; } continue; }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {},
            change = updates.changed() => { if change.is_err() { return; } }
        }
    }
}

async fn metrics(State(app): State<Arc<App>>) -> Response {
    let Ok(permit) = app.scrapes.clone().try_acquire_owned() else {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("scrape concurrency limit\n"))
            .unwrap();
    };
    let current = app.config.read().await;
    let config = current.clone();
    let generation = app.generation.load(Ordering::SeqCst);
    drop(current);
    let now = unix_time();
    let mut header_text = String::new();
    for name in [
        "ufm_up",
        "ufm_scrape_error",
        "ufm_scrape_duration_seconds",
        "ufm_data_age_seconds",
        "ufm_telemetry_up",
        "ufm_telemetry_scrape_error",
        "ufm_telemetry_data_age_seconds",
        "ufm_telemetry_series",
        "ufm_telemetry_poll_seconds",
        "ufm_telemetry_observed_at_seconds",
        "ufm_source_enabled",
        "ufm_source_input_bytes",
        "ufm_source_cache_bytes",
        "ufm_source_observation_age_seconds",
        "ufm_source_expired_samples",
        "ufm_source_last_error",
    ] {
        writeln!(header_text, "# TYPE {name} gauge").unwrap();
    }
    for name in [
        "ufm_telemetry_successful_polls_total",
        "ufm_source_successful_polls_total",
        "ufm_source_failed_polls_total",
        "ufm_config_reload_successes_total",
        "ufm_config_reload_failures_total",
    ] {
        writeln!(header_text, "# TYPE {name} counter").unwrap();
    }
    writeln!(
        header_text,
        "ufm_config_reload_successes_total {}\nufm_config_reload_failures_total {}",
        app.reload_successes.load(Ordering::Relaxed),
        app.reload_failures.load(Ordering::Relaxed)
    )
    .unwrap();
    let mut chunks = VecDeque::new();
    for (index, source) in ["rest", "9001", "9002"].iter().enumerate() {
        let state = app.sources[index].read().await;
        let ok = healthy(&state, &config, index, generation, now);
        let cache = state.cache.as_ref();
        let age = cache.map_or(-1.0, |c| c.finished.elapsed().as_secs_f64());
        let observation_age = cache
            .and_then(|c| c.result.oldest_observation)
            .map_or(-1.0, |t| (now - t).max(0.0));
        for (metric, value) in [
            ("enabled", usize::from(enabled(&config, index)) as f64),
            (
                "input_bytes",
                cache.map_or(0, |c| c.result.input_bytes) as f64,
            ),
            (
                "cache_bytes",
                cache.map_or(0, |c| c.result.data.len() + c.result.metadata.len()) as f64,
            ),
            ("observation_age_seconds", observation_age),
            (
                "expired_samples",
                cache.map_or(0, |c| c.result.expired_samples) as f64,
            ),
            ("successful_polls_total", state.successful_polls as f64),
            ("failed_polls_total", state.failed_polls as f64),
        ] {
            writeln!(
                header_text,
                "ufm_source_{metric}{{source=\"{source}\"}} {value}"
            )
            .unwrap();
        }
        let reason = if !enabled(&config, index) {
            "disabled"
        } else {
            state.last_error.map_or(
                if ok { "none" } else { "not_ready_or_expired" },
                PollError::label,
            )
        };
        writeln!(
            header_text,
            "ufm_source_last_error{{source=\"{source}\",reason=\"{reason}\"}} {}",
            usize::from(enabled(&config, index) && !ok)
        )
        .unwrap();
        if index == 0 && config.rest.enabled {
            writeln!(header_text, "ufm_up {}\nufm_scrape_error {}\nufm_scrape_duration_seconds {}\nufm_data_age_seconds {age}",
                usize::from(ok), usize::from(!ok), state.duration).unwrap();
        } else if index > 0 && enabled(&config, index) {
            for (name, value) in [
                ("up", usize::from(ok) as f64),
                ("scrape_error", usize::from(!ok) as f64),
                ("data_age_seconds", age),
                (
                    "series",
                    if ok {
                        cache.map_or(0, |c| c.result.series)
                    } else {
                        0
                    } as f64,
                ),
                ("poll_seconds", state.duration),
                ("successful_polls_total", state.successful_polls as f64),
            ] {
                writeln!(
                    header_text,
                    "ufm_telemetry_{name}{{source=\"{source}\"}} {value}"
                )
                .unwrap();
            }
        }
        if ok && let Some(cached) = cache {
            if !cached.result.metadata.is_empty() {
                chunks.push_back(cached.result.metadata.clone());
            }
            chunks.push_back(cached.result.data.clone());
        }
    }
    // A small header plus reference-counted immutable buffers; no full-body clone per scrape.
    chunks.push_front(Bytes::from(header_text));
    let stream =
        futures_util::stream::unfold((chunks, permit), |(mut chunks, permit)| async move {
            chunks
                .pop_front()
                .map(|chunk| (Ok::<_, Infallible>(chunk), (chunks, permit)))
        });
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn ready(State(app): State<Arc<App>>) -> (StatusCode, &'static str) {
    let current = app.config.read().await;
    let config = current.clone();
    let generation = app.generation.load(Ordering::SeqCst);
    drop(current);
    for index in 0..3 {
        if enabled(&config, index)
            && !healthy(
                &*app.sources[index].read().await,
                &config,
                index,
                generation,
                unix_time(),
            )
        {
            return (StatusCode::SERVICE_UNAVAILABLE, "source unavailable\n");
        }
    }
    (StatusCode::OK, "ready\n")
}

async fn run(config: Config, path: Option<PathBuf>) -> Result<()> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .gzip(true);
    if let Some(file) = &config.ca_file {
        let pem = std::fs::read(file).context("cannot read CA certificate bundle")?;
        for certificate in
            reqwest::Certificate::from_pem_bundle(&pem).context("invalid CA certificate bundle")?
        {
            builder = builder.add_root_certificate(certificate);
        }
    }
    let client = builder
        .build()
        .context("cannot create verified HTTP client")?;
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .context("cannot bind metrics listener")?;
    let app = Arc::new(App {
        config: RwLock::new(Arc::new(config.clone())),
        generation: AtomicU64::new(0),
        sources: std::array::from_fn(|_| RwLock::new(SourceState::default())),
        publication: Mutex::new(()),
        heavy: Arc::new(Semaphore::new(config.max_heavy_polls)),
        scrapes: Arc::new(Semaphore::new(config.max_concurrent_scrapes)),
        reload_successes: AtomicU64::new(0),
        reload_failures: AtomicU64::new(0),
    });
    let (updates, initial) = watch::channel((Arc::new(config), 0));
    for index in 0..3 {
        tokio::spawn(worker(app.clone(), client.clone(), index, initial.clone()));
    }
    #[cfg(unix)]
    {
        let app = app.clone();
        tokio::spawn(async move {
            let Ok(mut signal) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            else {
                return;
            };
            while signal.recv().await.is_some() {
                let next = Config::load(path.as_deref());
                if let Ok(next) = next {
                    let _publication = app.publication.lock().await;
                    let mut current = app.config.write().await;
                    if current.same_runtime(&next) {
                        let next = Arc::new(next);
                        let generation = app.generation.fetch_add(1, Ordering::SeqCst) + 1;
                        *current = next.clone();
                        for state in &app.sources {
                            let mut state = state.write().await;
                            // Counters survive configuration reload; snapshots do not.
                            state.cache = None;
                            state.success = false;
                            state.last_error = None;
                        }
                        let _ = updates.send((next, generation));
                        app.reload_successes.fetch_add(1, Ordering::Relaxed);
                        tracing::info!("configuration reloaded");
                        continue;
                    }
                }
                app.reload_failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("configuration reload rejected; previous configuration retained");
            }
        });
    }
    let router = Router::new()
        .route("/metrics", get(metrics))
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/readyz", get(ready))
        .with_state(app);
    tracing::info!(address = %listener.local_addr()?, "metrics listener ready");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .unwrap();
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
        })
        .await?;
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let mut path = std::env::var_os("UFM_CONFIG").map(PathBuf::from);
    let mut check = false;
    let mut healthcheck = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => path = Some(args.next().context("--config requires a file")?.into()),
            "--check-config" => check = true,
            "--healthcheck" => healthcheck = true,
            "--version" | "-V" => {
                println!("ufm-ib-exporter {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--print-default-config" => {
                print!("{}", serde_yaml::to_string(&Config::default())?);
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "ufm-ib-exporter [--config FILE] [--check-config] [--healthcheck] [--print-default-config] [--version]\nSend SIGHUP to validate and reload source settings; listening/runtime limits require restart."
                );
                return Ok(());
            }
            _ => bail!("unknown argument; use --help"),
        }
    }
    let config = Config::load(path.as_deref())?;
    if check {
        println!("Configuration valid (no upstream requests)");
        return Ok(());
    }
    if healthcheck {
        let mut address = config.listen;
        if address.ip().is_unspecified() {
            address.set_ip(if address.is_ipv4() {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(3))
                    .build()?;
                let response = client
                    .get(format!("http://{address}/healthz"))
                    .send()
                    .await?;
                anyhow::ensure!(response.status().is_success(), "liveness check failed");
                Ok(())
            });
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.worker_threads)
        .enable_all()
        .build()?
        .block_on(run(config, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_http_does_not_refresh_old_observation() {
        let mut config = Config::default();
        config.telemetry_9001.enabled = true;
        let state = SourceState {
            success: true,
            cache: Some(Arc::new(Cached {
                result: Collected {
                    data: Bytes::from_static(b"x 1\n"),
                    metadata: Bytes::new(),
                    input_bytes: 1,
                    series: 1,
                    oldest_observation: Some(1.0),
                    expired_samples: 0,
                    metric_names: vec!["x".into()],
                },
                finished: Instant::now(),
                generation: 0,
            })),
            ..SourceState::default()
        };
        assert!(!healthy(&state, &config, 1, 0, 1000.0));
        assert!(healthy(&state, &config, 1, 0, 2.0));
        assert!(!healthy(&state, &config, 1, 1, 2.0));
    }
}
