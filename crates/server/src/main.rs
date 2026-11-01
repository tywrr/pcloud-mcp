//! `chance-of-files`: the MCP server, in a pod next to `rclone rcd` and cloudflared.
//!
//! ```text
//! chance-of-files [--config PATH]
//! ```
//!
//! The config file comes from `--config`, else `CHANCE_OF_FILES_CONFIG`, else
//! `/etc/chance-of-files/config.toml`. Secrets come from the environment: `INTERNAL_TOKEN`
//! (what the auth Worker sends as a bearer token) and `RC_PASS` (rclone's rc password).
//! Logs are JSON lines on stdout; `RUST_LOG` filters them (default `info`).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chance_of_files_core::{Health, McpServer};
use chance_of_files_rclone::RcloneStorage;
use chance_of_files_server::config::{self, DEFAULT_CONFIG_PATH};
use chance_of_files_server::{App, health_loop, router};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: chance-of-files [--config PATH]\n\nSee config.example.toml for the \
                     file; INTERNAL_TOKEN and RC_PASS come from the environment.";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(false)
        .with_target(false)
        .with_writer(std::io::stdout)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_path = match config_path(std::env::args().skip(1)) {
        Ok(Some(path)) => path,
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let settings = match config::load(&config_path, &|name| std::env::var(name).ok()) {
        Ok(settings) => settings,
        Err(message) => {
            tracing::error!(error = %message, "can't start: bad configuration");
            return ExitCode::from(78);
        }
    };
    for key in &settings.unknown_keys {
        tracing::warn!(key = %key, "unknown config key ignored");
    }
    if settings.internal_token.len() < 32 {
        tracing::warn!("INTERNAL_TOKEN is shorter than 32 characters; use a long random value");
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(error = %e, "can't start the async runtime");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(serve(settings)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            tracing::error!(error = %message, "stopped");
            ExitCode::FAILURE
        }
    }
}

/// The config file from the arguments or the environment; `None` after `--help`/`--version`.
fn config_path(mut args: impl Iterator<Item = String>) -> Result<Option<PathBuf>, String> {
    let mut path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("chance-of-files {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--config" => path = Some(args.next().ok_or("--config needs a path")?),
            other => match other.strip_prefix("--config=") {
                Some(value) => path = Some(value.to_owned()),
                None => return Err(format!("unknown argument {other:?}")),
            },
        }
    }
    let path = path
        .or_else(|| std::env::var("CHANCE_OF_FILES_CONFIG").ok())
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_owned());
    Ok(Some(PathBuf::from(path)))
}

async fn serve(settings: config::Settings) -> Result<(), String> {
    if let Some(dir) = &settings.state_dir {
        // Phase 3 keeps state.db here; creating it now shows a volume problem at startup.
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "can't create the state directory");
        }
    }
    let storage = Arc::new(RcloneStorage::new(settings.rclone)?);
    let fs = storage.fs().to_owned();
    let health = Arc::new(Health::new());
    let mut mcp = McpServer::new(storage.clone(), settings.core)
        .with_policy(settings.policy.clone())
        .with_health(health.clone());

    // Without a usable index the server still works, live, as v0.2 did.
    let mut indexer = None;
    match settings.index {
        None if settings.index_needs_dir => tracing::warn!(
            "the index is off: neither [index].dir nor [server].state_dir is set, so there is \
             nowhere to keep it; every tool asks rclone directly and the audit tools are missing"
        ),
        None => tracing::info!("the index is turned off; every tool asks rclone directly"),
        Some(config) => {
            let dir = config.dir.display().to_string();
            let roots: Vec<String> = config.roots.iter().map(|r| r.path.clone()).collect();
            match chance_of_files_index::open(
                config,
                settings.policy,
                health.clone(),
                storage.clone(),
            ) {
                Ok((handle, background)) => {
                    tracing::info!(dir = %dir, ?roots, "index open");
                    mcp = mcp.with_index(handle);
                    indexer = Some(background);
                }
                Err(e) => {
                    tracing::error!(dir = %dir, error = %e, "can't open the index; serving without it");
                }
            }
        }
    }
    let app = Arc::new(App::new(
        mcp,
        settings.internal_token,
        settings.max_body_bytes,
    ));

    // rclone may start after us in the pod; capabilities are looked up until it answers, and
    // only then do health checks and walks start.
    let probe = app.clone();
    let health_every = settings.health_every;
    tokio::spawn(async move {
        let mut wait = std::time::Duration::from_secs(1);
        loop {
            match probe.mcp.storage().load_capabilities().await {
                Ok(caps) => {
                    let hashes: Vec<&str> = caps.hashes.iter().map(|h| h.name()).collect();
                    tracing::info!(remote = %fs, ?hashes, dir_move = caps.server_dir_move, "rclone is up");
                    break;
                }
                Err(e) => {
                    tracing::warn!(remote = %fs, error = %e, "rclone not reachable yet");
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(std::time::Duration::from_secs(60));
                }
            }
        }
        tokio::spawn(health_loop(
            probe.mcp.storage().clone(),
            probe.mcp.health().clone(),
            health_every,
        ));
        if let Some(indexer) = indexer {
            indexer.run().await;
        }
    });

    let listener = tokio::net::TcpListener::bind(settings.listen)
        .await
        .map_err(|e| format!("can't listen on {}: {e}", settings.listen))?;
    tracing::info!(
        listen = %settings.listen,
        version = env!("CARGO_PKG_VERSION"),
        "chance-of-files listening"
    );
    axum::serve(listener, router(app))
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(|e| format!("server error: {e}"))?;
    tracing::info!("shut down");
    Ok(())
}

/// Resolves on SIGTERM (Kubernetes stopping the pod) or Ctrl-C.
async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
    tracing::info!("shutting down");
}
