//! Chat-shaped mock upstream (OpenAI-compatible) over TLS with the fixture
//! certificates (domain: test-server.io). Thin CLI wrapper around
//! [`proverd::testfixture`].
//!
//! Usage: mock_upstream [--port 0]   (prints the bound port as PORT=<n>)

#[tokio::main]
async fn main() {
    proverd::testfixture::ensure_crypto_provider();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut port: u16 = 0;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--port" {
            port = args.next().unwrap().parse().unwrap();
        }
    }

    let addr = proverd::testfixture::serve(port)
        .await
        .expect("bind mock upstream");
    println!("PORT={}", addr.port());
    futures::future::pending::<()>().await;
}
