//! proverd binary.

use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result};
use tlsn::webpki::{CertificateDer, RootCertStore};
use tracing::info;

use proverd::{AppState, Config, router};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "proverd=info,tlsn=warn".into()),
        )
        .init();

    let bind: SocketAddr = std::env::var("PROVERD_BIND")
        .unwrap_or_else(|_| "127.0.0.1:7047".to_string())
        .parse()
        .context("invalid PROVERD_BIND")?;

    // Root store: Mozilla roots, plus an optional extra PEM root (for local
    // testing against fixture servers with self-signed chains).
    let mut root_store = RootCertStore::mozilla();
    if let Ok(path) = std::env::var("PROVERD_EXTRA_ROOT_CERT_PEM") {
        let pem = std::fs::read(&path).context("reading PROVERD_EXTRA_ROOT_CERT_PEM")?;
        let cert = CertificateDer::from_pem_slice(&pem)
            .map_err(|e| anyhow::anyhow!("parsing {path}: {e}"))?;
        root_store.roots.push(cert);
        info!(path, "added extra root certificate");
    }

    let config = Config {
        root_store,
        ws_wait_timeout: Duration::from_secs(30),
        default_max_recv_bytes: 1 << 20,
    };

    let state = AppState::new(config);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!(%bind, "proverd listening");
    axum::serve(listener, router(state)).await?;
    Ok(())
}
