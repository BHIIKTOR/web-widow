use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use web_widow::config::{Config, Paths};
use web_widow::fetch::gate::{HostGate, PolitenessConfig};
use web_widow::fetch::http::HttpClient;
use web_widow::fetch::robots::RobotsService;
use web_widow::fetch::ssrf::{AllowList, SsrfGate};
use web_widow::pipeline::Pipeline;
use web_widow::render;
use web_widow::store::{DataDir, Store};

#[derive(Debug, Parser)]
#[command(
    name = "web-widow",
    version,
    about = "Agent-native web crawler exposed as an MCP server (stdio or loopback HTTP)"
)]
struct Cli {
    /// Configuration file (TOML). Defaults to $WEB_WIDOW_CONFIG, else built-in defaults.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Data directory. Defaults to $WEB_WIDOW_DATA, else $XDG_DATA_HOME/web-widow.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve MCP over stdio (default) or loopback streamable HTTP.
    Serve {
        /// Bind a loopback streamable-HTTP listener at this address (e.g. 127.0.0.1:8765).
        #[arg(long)]
        http: Option<SocketAddr>,
        /// File containing the HTTP bearer token (mode 0600). Env WEB_WIDOW_HTTP_TOKEN wins.
        #[arg(long)]
        http_token_file: Option<PathBuf>,
        /// Permit a non-loopback HTTP bind (deliberate local-network override).
        #[arg(long)]
        allow_non_loopback: bool,
    },
    /// Check every due monitor once and exit (cron/systemd timer entry point).
    RunDue {
        /// Maximum monitors to check in this pass (defaults to [monitor].max_checks_per_call).
        #[arg(long)]
        limit: Option<usize>,
        /// Check through a running server instead of opening the data directory
        /// (e.g. http://127.0.0.1:8765/mcp). Never takes the data-dir lock.
        #[arg(long, value_name = "URL")]
        http: Option<String>,
        /// File containing the HTTP bearer token (mode 0600). Env WEB_WIDOW_HTTP_TOKEN wins.
        #[arg(long)]
        http_token_file: Option<PathBuf>,
        /// Inline wait budget for the HTTP monitor_check call, in ms (default 30000).
        #[arg(long)]
        wait_ms: Option<u64>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let loaded = Config::load(cli.config.as_deref(), cli.data_dir)
        .context("configuration could not be loaded")?;
    init_tracing(&loaded.config);

    if let web_widow::config::ConfigOrigin::File(path) = &loaded.origin {
        tracing::info!(config = %path.display(), "configuration loaded");
        warn_if_readable(path);
    }
    let config = Arc::new(loaded.config);

    if config.fetch.contact.is_none() {
        tracing::warn!(
            "fetch.contact is not set: the User-Agent has no contact address; set WEB_WIDOW_CONTACT or fetch.contact"
        );
    }
    if !config.robots.enabled {
        tracing::warn!(
            "robots.enabled = false: robots.txt is NOT enforced for this process. This is a deliberate operator override."
        );
    }
    if !config.fetch.allow_hosts.is_empty() {
        tracing::info!(
            allow_hosts = ?config.fetch.allow_hosts,
            "private-address allowlist active; matches are logged at info"
        );
    }

    // `run-due --http` is a client: it must not touch the data directory or its
    // lock (the running server owns them). Branch before Paths/DataDir/Store.
    if let Some(Command::RunDue {
        http: Some(endpoint),
        http_token_file,
        wait_ms,
        limit,
        ..
    }) = &cli.command
    {
        if limit.is_some() {
            anyhow::bail!(
                "--limit applies to the local `run-due` pass only and cannot be combined with --http"
            );
        }
        let token = resolve_http_token(http_token_file.as_deref())
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let wait_ms = wait_ms.unwrap_or(30_000);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("cannot start the async runtime")?;
        let payload = runtime
            .block_on(web_widow::cli_http::run_due(endpoint, &token, wait_ms))
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        println!("{}", serde_json::to_string(&payload)?);
        return Ok(());
    }

    let paths = Paths::new(config.data_dir()?);
    let _data_dir = DataDir::open(paths.clone()).context("data directory is not usable")?;
    tracing::info!(data_dir = %paths.root().display(), "data directory ready");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;

    let outcome = runtime.block_on(async move {
        let store = Arc::new(
            Store::open(&paths, &config.fts).context("storage could not be opened")?,
        );
        match store.gc(config.retention.clone()).await {
            Ok(report) => tracing::info!(
                events_pruned = report.events_pruned,
                extractions_pruned = report.extractions_pruned,
                blobs_pruned = report.blobs_pruned,
                webhook_deliveries_pruned = report.webhook_deliveries_pruned,
                "startup retention pass complete"
            ),
            Err(error) => tracing::warn!(error = %error, "startup retention pass failed"),
        }

        let allow = AllowList::parse(&config.fetch.allow_hosts)?;
        let ssrf = Arc::new(SsrfGate::new(allow));
        let gate = Arc::new(HostGate::new(PolitenessConfig {
            min_interval: std::time::Duration::from_millis(config.fetch.min_interval_ms),
            jitter: std::time::Duration::from_millis(config.fetch.jitter_ms),
            max_concurrent: config.fetch.max_concurrent_per_host,
        }));
        let http = Arc::new(HttpClient::new(
            &config.fetch,
            Arc::clone(&ssrf),
            Arc::clone(&gate),
            config.user_agent(),
        )?);
        let robots = Arc::new(RobotsService::new(
            Arc::clone(&http),
            Arc::clone(&store.db),
            config.robots.clone(),
        ));
        let renderer = render::build_renderer(&config, paths.root())?;
        let pipeline = Arc::new(Pipeline::new(
            Arc::clone(&config),
            http,
            ssrf,
            gate,
            robots,
            Arc::clone(&store),
            Arc::clone(&renderer),
        ));

        let crawler = web_widow::crawl::CrawlManager::new(
            Arc::clone(&config),
            Arc::clone(&pipeline),
            Arc::clone(&store),
        );
        let monitors = web_widow::monitor::MonitorManager::new(
            Arc::clone(&config),
            Arc::clone(&pipeline),
            Arc::clone(&store),
        );
        let content = Arc::new(web_widow::fts::ContentIndex::new(
            Arc::clone(&config),
            Arc::clone(&store),
        ));
        let interactions = Arc::new(web_widow::interaction::InteractionManager::new(
            Arc::clone(&config),
            Arc::clone(&pipeline),
        ));
        let feed_runtime = Arc::new(web_widow::events::FeedRuntime::default());
        let events = Arc::new(web_widow::events::EventsService::new(
            Arc::clone(&store),
            config.events.clone(),
            Arc::clone(&feed_runtime),
        ));
        let robots_cache = Arc::new(web_widow::robots_cache::RobotsCacheService::new(
            Arc::clone(&store),
            config.robots.user_agent.clone(),
        ));
        let embeddings = Arc::new(web_widow::embeddings::EmbeddingService::new(
            Arc::clone(&config),
            Arc::clone(&store),
        ));

        match cli.command.unwrap_or(Command::Serve {
            http: None,
            http_token_file: None,
            allow_non_loopback: false,
        }) {
            Command::Serve {
                http,
                http_token_file,
                allow_non_loopback,
            } => {
                match crawler.resume().await {
                    Ok(0) => {}
                    Ok(resumed) => tracing::info!(resumed, "resumed unfinished crawl jobs"),
                    Err(error) => tracing::warn!(error = %error, "could not resume crawl jobs"),
                }
                match monitors.resume().await {
                    Ok(0) => {}
                    Ok(stale) => {
                        tracing::warn!(stale, "interrupted monitor check jobs were closed")
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "could not close interrupted monitor jobs")
                    }
                }

                // The scheduler is opt-in and serve-only; it drives the same
                // `run_due` pass a cron entry would, so it adds no service/tool.
                let scheduler_shutdown = tokio_util::sync::CancellationToken::new();
                let mut scheduler = if config.scheduler.enabled {
                    Some(web_widow::monitor::scheduler::spawn(
                        Arc::clone(&monitors),
                        Arc::clone(&config),
                        scheduler_shutdown.clone(),
                    ))
                } else {
                    None
                };

                // The feed URL is advertised in webhook payloads only when the
                // loopback listener actually serves the feed.
                let feed_url = http
                    .filter(|_| config.events.feed_enabled)
                    .map(|addr| format!("http://{addr}/feeds/changes.xml"));
                let webhooks = Arc::new(web_widow::webhooks::WebhookService::new(
                    Arc::clone(&config),
                    Arc::clone(&store),
                    feed_url,
                )?);

                let server = web_widow::mcp::WebWidowServer::new(
                    Arc::clone(&pipeline),
                    Arc::clone(&crawler),
                    Arc::clone(&monitors),
                    Arc::clone(&content),
                    Arc::clone(&interactions),
                    Arc::clone(&robots_cache),
                    Arc::clone(&events),
                    Arc::clone(&embeddings),
                    Arc::clone(&webhooks),
                );

                // The semantic backfill runs only in `serve`, only when enabled,
                // and is cancelled by the same clean shutdown as the jobs.
                let embeddings_shutdown = tokio_util::sync::CancellationToken::new();
                let mut backfill = if config.embeddings.enabled {
                    tracing::info!(
                        model = %config.embeddings.model,
                        interval_ms = config.embeddings.backfill_interval_ms,
                        "embedding backfill started"
                    );
                    Some(Arc::clone(&embeddings).spawn_backfill(embeddings_shutdown.clone()))
                } else {
                    None
                };

                // The webhook dispatcher is a separate task: a check only inserts
                // outbox rows, and a slow target can never delay or fail a check.
                let webhooks_shutdown = tokio_util::sync::CancellationToken::new();
                let mut dispatcher = if config.webhooks.enabled {
                    tracing::info!(
                        targets = config.webhooks.targets.len(),
                        timeout_ms = config.webhooks.timeout_ms,
                        "webhook dispatcher started"
                    );
                    Some(Arc::clone(&webhooks).spawn_dispatcher(webhooks_shutdown.clone()))
                } else {
                    None
                };

                if let Some(addr) = http {
                    #[cfg(feature = "http")]
                    {
                        if !addr.ip().is_loopback() && !allow_non_loopback {
                            anyhow::bail!(
                                "refusing a non-loopback HTTP bind ({addr}); pass --allow-non-loopback \
                                 only for a deliberate local-network deployment"
                            );
                        }
                        // The feed is loopback-only by grant (IMP-ITEM-004). A
                        // non-loopback listener is refused before any token is
                        // resolved or provisioned, so the posture cannot change
                        // silently.
                        if !addr.ip().is_loopback() && config.events.feed_enabled {
                            anyhow::bail!(
                                "refusing to serve the change feed on a non-loopback HTTP bind \
                                 ({addr}); set `[events].feed_enabled = false` for an MCP-only \
                                 network deployment"
                            );
                        }
                        let token = resolve_http_token(http_token_file.as_deref())?;
                        // The feed capability token is separate from the MCP
                        // bearer; it is only provisioned when the route is on.
                        let feed_token = web_widow::events::resolve_feed_token(
                            config.events.feed_enabled,
                            &paths,
                        )?;
                        let shutdown = tokio_util::sync::CancellationToken::new();
                        let serving = web_widow::mcp::http::serve(
                            server,
                            addr,
                            token,
                            web_widow::mcp::http::FeedRoute {
                                enabled: config.events.feed_enabled,
                                token: feed_token,
                                events: Arc::clone(&events),
                            },
                            shutdown.clone(),
                        );
                        tokio::pin!(serving);
                        tokio::select! {
                            result = &mut serving => {
                                result.map_err(|error| anyhow::anyhow!(error.to_string()))?;
                            }
                            () = shutdown_signal() => {
                                tracing::info!("shutdown signal: stopping HTTP listener and jobs");
                                shutdown.cancel();
                                crawler.shutdown();
                                monitors.shutdown();
                                interactions.shutdown().await;
                                if tokio::time::timeout(Duration::from_secs(10), &mut serving)
                                    .await
                                    .is_err()
                                {
                                    tracing::warn!("HTTP server did not stop within 10 s");
                                }
                            }
                        }
                    }
                    #[cfg(not(feature = "http"))]
                    {
                        let _ = (addr, http_token_file, allow_non_loopback, server);
                        anyhow::bail!(
                            "this binary was built without the `http` feature; rebuild with --features http"
                        );
                    }
                } else {
                    tracing::info!(version = web_widow::VERSION, "serving MCP over stdio");
                    let served = web_widow::mcp::serve_stdio(server);
                    tokio::pin!(served);
                    tokio::select! {
                        result = &mut served => {
                            result.map_err(|error| anyhow::anyhow!(error.to_string()))?;
                        }
                        () = shutdown_signal() => {
                            tracing::info!("shutdown signal: closing stdio server and jobs");
                            crawler.shutdown();
                            monitors.shutdown();
                            interactions.shutdown().await;
                        }
                    }
                }

                // Cooperative cancellation of job workers, then close the browser
                // and remove the throwaway profile. A SIGKILL leaves the profile;
                // the next start wipes it.
                embeddings_shutdown.cancel();
                webhooks_shutdown.cancel();
                scheduler_shutdown.cancel();
                if let Some(handle) = backfill.as_mut()
                    && tokio::time::timeout(Duration::from_secs(10), &mut *handle)
                        .await
                        .is_err()
                {
                    tracing::warn!("embedding backfill did not stop within 10 s");
                }
                if let Some(handle) = dispatcher.as_mut()
                    && tokio::time::timeout(Duration::from_secs(10), &mut *handle)
                        .await
                        .is_err()
                {
                    tracing::warn!("webhook dispatcher did not stop within 10 s");
                }
                if let Some(handle) = scheduler.as_mut()
                    && tokio::time::timeout(Duration::from_secs(10), &mut *handle)
                        .await
                        .is_err()
                {
                    tracing::warn!("monitor scheduler did not stop within 10 s");
                }
                monitors.shutdown();
                crawler.shutdown();
                interactions.shutdown().await;
                renderer.shutdown().await;
            }
            Command::RunDue { limit, .. } => {
                // A second process on the same data directory is refused by the
                // startup lock, so this pass is the only writer.
                if let Err(error) = monitors.resume().await {
                    tracing::warn!(error = %error, "could not close interrupted monitor jobs");
                }
                let limit = limit.unwrap_or(config.monitor.max_checks_per_call);
                let summary = monitors.run_due(limit).await?;
                // A local run-due pass owns one bounded flush; `run-due --http`
                // is a client and leaves delivery to the server.
                if config.webhooks.enabled {
                    let webhooks = web_widow::webhooks::WebhookService::new(
                        Arc::clone(&config),
                        Arc::clone(&store),
                        None,
                    )?;
                    let attempted = webhooks.flush_due(20).await?;
                    tracing::info!(attempted, "run-due webhook flush complete");
                }
                // stdout is data for this subcommand, not the MCP protocol.
                println!("{}", serde_json::to_string(&summary)?);
                monitors.shutdown();
                interactions.shutdown().await;
                renderer.shutdown().await;
            }
        }
        Ok::<(), anyhow::Error>(())
    });
    // Tokio's stdin reader is a blocking task that cannot be cancelled; on a
    // signal the explicit cleanup above is already done, so bound the runtime
    // shutdown instead of waiting forever for that read (B6-7: <=10 s).
    runtime.shutdown_timeout(Duration::from_secs(2));
    outcome
}

/// Resolve the HTTP bearer token: `WEB_WIDOW_HTTP_TOKEN` wins, then a `0600`
/// file. The token is never logged.
#[cfg_attr(not(feature = "http"), allow(dead_code))]
fn resolve_http_token(file: Option<&std::path::Path>) -> anyhow::Result<String> {
    if let Ok(token) = std::env::var("WEB_WIDOW_HTTP_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    if let Some(path) = file {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("HTTP token file {} is not readable", path.display()))?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "HTTP token file {} is mode {mode:o}; expected 0600",
                path.display()
            );
        }
        let token = std::fs::read_to_string(path)
            .with_context(|| format!("HTTP token file {} cannot be read", path.display()))?
            .trim()
            .to_string();
        if token.is_empty() {
            anyhow::bail!("HTTP token file {} is empty", path.display());
        }
        return Ok(token);
    }
    anyhow::bail!(
        "--http requires a bearer token: set WEB_WIDOW_HTTP_TOKEN or pass --http-token-file"
    )
}

/// Wait for SIGINT/SIGTERM. Used by both transports so a service manager can
/// stop the process cleanly (jobs cancelled, renderer closed, lock released).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(terminate) => terminate,
            Err(error) => {
                tracing::warn!(%error, "cannot install the SIGTERM handler");
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn init_tracing(config: &Config) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.logging.level.clone()));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false);
    // stdout is the MCP protocol; every log line goes to stderr.
    if config.logging.json {
        let _ = builder.json().try_init();
    } else {
        let _ = builder.try_init();
    }
}

fn warn_if_readable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                config = %path.display(),
                mode = format!("{mode:o}"),
                "configuration file is readable by group/other; expected 0600"
            );
        }
    }
}
