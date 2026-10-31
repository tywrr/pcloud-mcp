//! Runs the network spike server. Env: SPIKE_TOKEN (required, >= 16 chars), SPIKE_ADDR
//! (default 127.0.0.1:8080), SPIKE_PUBLIC_BASE (optional fallback for `blob_url` when the
//! Worker's X-Spike-Public-Base header is absent, e.g. when testing locally).

use cof_network_spike::{AppState, app};

#[tokio::main]
async fn main() {
    let token = std::env::var("SPIKE_TOKEN").unwrap_or_default();
    let token = token.trim();
    if token.len() < 16 {
        eprintln!("SPIKE_TOKEN must be set (16+ characters)");
        std::process::exit(2);
    }
    let addr = std::env::var("SPIKE_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let public_base = std::env::var("SPIKE_PUBLIC_BASE")
        .ok()
        .filter(|s| !s.is_empty());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("bind {addr}: {e}");
            std::process::exit(1);
        });
    println!(
        "cof-network-spike {} listening on {addr}",
        env!("CARGO_PKG_VERSION")
    );
    axum::serve(listener, app(AppState::new(token, public_base)))
        .with_graceful_shutdown(async {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        })
        .await
        .expect("server");
}
