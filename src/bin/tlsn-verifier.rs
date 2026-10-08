//! tlsn-verifier — native TLSN proxy-TLS verifier binary.
//!
//! The Bun/Node backend for the SDK's TlsnVerifier: spawned per session,
//! runs the verifier protocol against proverd, prints machine-readable
//! progress on stdout:
//!
//!   READY <server_name>     — commit accepted; this is the host we relay to
//!   VERIFIED <server_name>  — proof verified; transcript written
//!   (anything else to stderr; exit code != 0 on failure)
//!
//! Usage:
//!   tlsn-verifier --proverd ws://HOST:PORT --session ID --upstream ADDR \
//!       [--root-cert-pem PATH]... --transcript-out PATH

use std::io::Write;
use std::process::ExitCode;

use tlsn::webpki::CertificateDer;

use proverd::testverifier::{DialTarget, redacted_string, run_verifier};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> ExitCode {
    // rustls ends up with both `ring` (via tlsn) and `aws-lc-rs` (via
    // tokio-tungstenite) crypto providers enabled through feature
    // unification; rustls then refuses to auto-pick one. Pin aws-lc-rs.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "proverd=info,tlsn=warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let mut proverd: Option<String> = None;
    let mut session: Option<String> = None;
    let mut upstream: Option<String> = None;
    let mut upstream_host: Option<String> = None;
    let mut upstream_port: u16 = 443;
    let mut root_cert_pems: Vec<String> = Vec::new();
    let mut transcript_out: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--proverd" => proverd = args.next(),
            "--session" => session = args.next(),
            "--upstream" => upstream = args.next(),
            "--upstream-host" => upstream_host = args.next(),
            "--upstream-port" => {
                upstream_port = args.next().expect("--upstream-port value").parse().expect("port")
            }
            "--root-cert-pem" => root_cert_pems.push(args.next().expect("--root-cert-pem value")),
            "--transcript-out" => transcript_out = args.next(),
            other => {
                eprintln!("unknown arg: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    let dial = match (upstream, upstream_host) {
        (Some(addr), None) => DialTarget::Addr(addr.parse().expect("--upstream addr")),
        (None, Some(host)) => DialTarget::Host(host, upstream_port),
        _ => {
            eprintln!(
                "usage: tlsn-verifier --proverd ws://HOST:PORT --session ID (--upstream IP:PORT | --upstream-host NAME [--upstream-port N]) [--root-cert-pem PATH]... [--transcript-out PATH]"
            );
            return ExitCode::FAILURE;
        }
    };

    let Some(proverd) = proverd else {
        eprintln!("missing --proverd");
        return ExitCode::FAILURE;
    };

    let mut extra_roots = Vec::new();
    for path in &root_cert_pems {
        let pem = std::fs::read(path).expect("read root cert");
        extra_roots.push(CertificateDer::from_pem_slice(&pem).expect("parse root cert PEM"));
    }

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<String>();
    tokio::spawn(async move {
        if let Ok(server_name) = ready_rx.await {
            println!("READY {server_name}");
            let _ = std::io::stdout().flush();
        }
    });

    // --proverd may be the base URL (append /ws?session_id=) or the full
    // mux URL already carrying the session id.
    let ws_url = if proverd.contains("session_id=") {
        proverd.clone()
    } else {
        let session = session.expect("--session required unless --proverd carries session_id");
        format!("{proverd}/ws?session_id={session}")
    };
    // Watchdog: callers are supposed to cancel() the verifier on error
    // paths, but a crashed or buggy caller leaves this process parked in an
    // async wait forever (observed in production: orphaned verifiers at
    // multiple GB RSS each). A verified session takes seconds-to-minutes;
    // bound the whole thing so the process is self-terminating.
    let watchdog_secs: u64 = std::env::var("TLSN_VERIFIER_WATCHDOG_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);
    let session_future = tokio::time::timeout(
        std::time::Duration::from_secs(watchdog_secs),
        run_verifier(&ws_url, dial, extra_roots, Some(ready_tx)),
    );
    let run_outcome = match session_future.await {
        Ok(outcome) => outcome,
        Err(_) => {
            eprintln!(
                "VERIFICATION FAILED: watchdog timeout after {watchdog_secs}s (caller abandoned the session?)"
            );
            return ExitCode::FAILURE;
        }
    };
    match run_outcome {
        Ok(result) => {
            if let Some(path) = transcript_out {
                use base64::Engine;
                let sent = result
                    .transcript
                    .sent_unsafe(result.transcript.len_sent())
                    .expect("sent_unsafe");
                let recv = result
                    .transcript
                    .received_unsafe(result.transcript.len_received())
                    .expect("received_unsafe");
                let json = serde_json::json!({
                    "server_name": result.server_name,
                    "sent_b64": base64::engine::general_purpose::STANDARD.encode(&sent),
                    "recv_b64": base64::engine::general_purpose::STANDARD.encode(&recv),
                    "commit_ms": result.commit_ms,
                    "proof_ms": result.proof_ms,
                });
                std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap())
                    .expect("write transcript");
            }
            println!("VERIFIED {}", result.server_name);
            println!("commit={}ms proof={}ms", result.commit_ms, result.proof_ms);
            let _ = std::io::stdout().flush();
            eprintln!(
                "disclosed request:\n{}",
                redacted_string(
                    &result
                        .transcript
                        .sent_unsafe(result.transcript.len_sent())
                        .unwrap()
                )
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("VERIFICATION FAILED: {e:#}");
            ExitCode::FAILURE
        }
    }
}
