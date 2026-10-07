# proverd

TLSN (TLSNotary) Proxy-TLS prover sidecar for routstr-core, plus the native
verifier binary the Routstr SDK/daemon uses.

This is the Rust half of Routstr's TLSN verified mode: a node runs `proverd`
alongside `routstr-core`; a client (routstrd / `@routstr/sdk`) runs the
verifier and thereby proves that the inference response it received is
byte-identical to what the claimed upstream API returned in a real TLS
session — with the node's upstream API key kept hidden.

## Components

| Binary | Role |
|---|---|
| `proverd` | Proxy-TLS **prover** sidecar. The node's routstr-core calls `POST /sessions`; the verifier dials `GET /ws?session_id=…`. |
| `tlsn-verifier` | Native proxy-TLS **verifier**, used by the SDK/daemon (Bun/Node) as the reliable alternative to the wasm verifier. Machine-readable `READY`/`VERIFIED` output. |
| `mock_upstream` (example) | Chat-shaped, OpenAI-compatible TLS mock upstream (fixture certs) for tests and local harnesses. |

## Protocol (proxy mode)

Three channels (see the design doc in the Routstr docs):

```
channel A — API call (HTTPS):      client ⇄ routstr-core
channel B — TLSN mux (WebSocket):  client ⇄ proverd        (verifier ⇄ prover protocol)
channel C — raw TCP relay:         client ⇄ upstream       (the verifier dials this)
```

The verifier — not the node — dials the upstream: it relays ciphertext between
the upstream and the prover, so the node needs no upstream network path at all
for a verified session. Only the prover holds the TLS session keys (the
verifier is blind to them), so the decrypted response is returned to the node
over channel A. After the response ends, the prover discloses the transcript
everything except the credential header values and completes the proof over
channel B.

### provverd HTTP surface

- `POST /sessions` — body: `{session_id, server_name, port, request: {method,
  path, headers, body}, redact: ["authorization"], max_recv_bytes}`. Returns
  the upstream status and streams the decrypted upstream response body as it
  arrives (`x-tlsn-session` header up front).
- `GET /ws?session_id=…` — WebSocket carrying the TLSN mux (paired to the
  `POST /sessions` call by `session_id`).
- `GET /sessions/{id}/status` — proof status and timings (the proof itself is
  delivered to the verifier over the mux, never through HTTP).
- `GET /healthz`.

Configuration (env): `PROVERD_BIND` (default `127.0.0.1:7047`),
`PROVERD_EXTRA_ROOT_CERT_PEM` (extra trusted root, for fixtures/testing).

### tlsn-verifier

```
tlsn-verifier --proverd ws://HOST:PORT --session ID \
              (--upstream IP:PORT | --upstream-host NAME [--upstream-port N]) \
              [--root-cert-pem PATH]... [--transcript-out PATH]
```

`--proverd` accepts either the base URL (the session id is appended) or a full
mux URL. With `--upstream-host` the verifier resolves DNS after the commit
reveals the server name and fails if the committed name differs.
`--transcript-out` writes the disclosed transcript (`sent_b64`, `recv_b64`,
timings) for the caller's comparator. `--upstream IP:PORT` is the test/fixture
path.

## Build

```sh
cargo build --bins --examples        # proverd, tlsn-verifier, mock_upstream
cargo test                           # unit + e2e (spawns proverd + mock upstream)
```

`tlsn` is pinned to the `tlsnotary/tlsn` main commit this was developed and
tested against (`v0.1.0-alpha.16-pre` line) because it is not published to
crates.io. See `Cargo.toml`.

## Notes and limits

- TLS 1.2 only for now (TLS 1.3 is disabled upstream in tlsn at this pin).
- `max_recv_bytes` caps the received transcript; a session past the cap is
  aborted and reported as unavailable rather than failing mid-stream.
- Redaction is range-selective: the request is disclosed in full except the
  values of the credential headers (default `authorization`), which stay bound
  by the transcript commitment. Hidden bytes prove presence, not meaning.
- `Connection: close` is used on the upstream request; keep-alive sessions are
  future work.
- The mux endpoints are unauthenticated in this version and assume a
  localhost/host-network sidecar deployment.

## License

MIT OR Apache-2.0. Test certificate fixtures under `fixtures/` are copied from
the TLSNotary repository (MIT OR Apache-2.0); see `fixtures/README.md`.
