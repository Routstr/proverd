# proverd — TLSN Proxy-TLS prover sidecar.
#
# Multi-stage: rust builder → slim runtime. Builds only the `proverd` binary
# (the verifier runs client-side; mock_upstream is a test fixture).
#
#   docker build -t proverd .
#   docker run -p 7047:7047 proverd          # PROVERD_BIND preset below
#
# Runtime env:
#   PROVERD_BIND               listen address (default 0.0.0.0:7047 here)
#   PROVERD_EXTRA_ROOT_CERT_PEM  extra CA bundle for fixture/self-signed
#                                upstreams (testing only)
#   RUST_LOG                   e.g. proverd=info,tlsn=warn

# ── build ─────────────────────────────────────────────────────────────────
FROM rust:1.95-bookworm AS builder
# cmake/clang: aws-lc-rs (via tlsn's rustls) — C build + bindgen.
# pkg-config/libssl-dev: native-tls fallback paths in ws deps.
RUN apt-get update && apt-get install -y --no-install-recommends \
      cmake clang pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples
COPY fixtures ./fixtures
RUN cargo build --release --locked --bin proverd

# ── runtime ───────────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --no-create-home proverd
COPY --from=builder /build/target/release/proverd /usr/local/bin/proverd
USER proverd
ENV PROVERD_BIND=0.0.0.0:7047 \
    RUST_LOG=proverd=info,tlsn=warn
EXPOSE 7047
ENTRYPOINT ["/usr/local/bin/proverd"]
