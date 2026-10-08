# Build Brief: End-to-End TLSN Verification for Routstr

You are the builder. I am the architect/orchestrator in the sibling pane — I wrote
the design and will steer. Work ONLY inside
`/home/user/projects/routstr_main/provable-ai/`. NEVER touch the original repos at
`/home/user/projects/routstr_main/{routstr-core,routstr-sdk,routstrd}`.

## Read first (in this order)

1. `TLSN-routstr.md` — THE design doc. Source of truth. Follow it; if something in
   it turns out to be wrong or unworkable, STOP and report the finding instead of
   silently redesigning.
2. This file.
3. `zkTLS-research.md` §5 (protocol flow) only if you need protocol background.

## Workspace layout

| Path | What |
|---|---|
| `tlsn/` | Vendored `tlsnotary/tlsn` @ `v0.1.0-alpha.16-pre` (Rust workspace, on main). Do NOT modify in place — depend on its crates by path. |
| `routstr-core/` | Copy of the Python FastAPI proxy (edit target, git baseline committed) |
| `routstr-sdk/` | Copy of `@routstr/sdk` TS (edit target, git baseline committed) |
| `routstrd/` | Copy of the Bun CLI/daemon (edit target, git baseline committed) |
| `proverd/` | NEW Rust crate you create |

## Architecture recap (Proxy-TLS mode, verifier-as-notary)

Three channels:

- **A**: SDK → routstr-core, normal HTTPS API (exists today)
- **B**: verifier (SDK, tlsn wasm) dials OUT via WebSocket to `proverd` — the tlsn mux
- **C**: verifier dials raw TCP to the upstream (e.g. `api.openai.com:443`) and
  relays ciphertext between C and B

`proverd` (node sidecar) authors the TLS session + HTTP request (it holds the
upstream API key), decrypts the response, forwards plaintext to routstr-core.
After stream end (`data: [DONE]` or connection close), `proverd` reveals all
transcript ranges EXCEPT the `authorization` header value and completes the proof
with the verifier over B. The SDK verifies the proof against the ciphertext it
personally relayed on C, then runs its comparator against what it sent/received
on A.

## Key API facts (verified in the vendored source — trust these)

- wasm verifier: `tlsn/crates/wasm/src/verifier/mod.rs` —
  `JsVerifier.connect(prover_url)` (outbound WebSocket). Proxy mode:
  `set_server_socket(jsIo)` must be called before `verify()`.
- `JsIo` interface (`tlsn/crates/wasm/src/io.rs`): JS object with
  `read(): Promise<Uint8Array|null>`, `write(data): Promise<void>`,
  `close(): Promise<void>`, `unread(data): void`.
- native verifier: `tlsn/crates/tlsn` — `TlsCommitConfig::Proxy(ProxyTlsConfig)`;
  after the commit is accepted, `Verifier::run(server_socket)` where the socket
  is any `AsyncRead + AsyncWrite`.
- TLS 1.2 only (TLS 1.3 is commented out of `ALL_VERSIONS` in `tls-core`).
- `tlsn/crates/examples/` (basic, attestation) show the prover/verifier flow
  shapes; `tlsn/crates/server-fixture/` is a local TLS test server.
- wasm build needs clang ≥ 16 (see `tlsn/README.md`).

## Build order — STOP and report after each milestone

### M1 — proverd + native Rust test verifier (Rust-only e2e)

- Create `proverd/` (binary crate, path deps on `tlsn/crates/tlsn` etc.):
  - `POST /sessions` — body: `{session_id, server_name, port, request: {method,
    path, headers, body}, redact: ["authorization"], max_recv_bytes}`. Response:
    200 + the decrypted upstream response streamed back as it arrives. Response
    headers include proof status after completion.
  - `GET /ws?session_id=...` — WebSocket endpoint the tlsn verifier connects to
    (this is the mux / channel B). Pair ws to `/sessions` call by `session_id`.
  - Proxy-TLS prover using `tlsn` with `TlsCommitConfig::Proxy`.
  - After the upstream response completes: reveal all ranges except the
    `authorization` header VALUE bytes; finish the proof protocol with the
    verifier over the mux.
- Add a native Rust integration test (`proverd/tests/` or an example binary):
  tlsn native `Verifier` + `TcpStream` against a LOCAL mock upstream (tlsn
  `server-fixture` or a tiny axum HTTPS server with a self-signed cert; pass the
  cert as a custom root to the verifier). No real API key needed.
- Success: the test verifier validates the proof against the ciphertext it
  relayed; the disclosed request/response match what was sent; print timings
  (handshake, transfer, proof).

### M2 — routstr-core verified mode (Python)

- Config: `TLSN_PROVERD_URL` env var; feature-flag everything (default off).
- `routstr/upstream/base.py`: verified-mode branch — forward via proverd
  instead of httpx. NON-STREAMING FIRST, then SSE.
- Byte-passthrough response body; skip `usage.cost` injection when verified;
  `request_correction` no-op except allowlisted param renames.
- Response headers: `x-routstr-verified: tlsn-proxy`, `x-routstr-upstream-host`,
  `x-routstr-tlsn-session: <id>`.
- Advertise `tlsn` capability in the model listing where practical.
- Don't run the full routstr-core test suite or docker — just the verified path
  with `uv run`.

### M3 — SDK verifier (TypeScript, Bun-first)

- Build the tlsn wasm package (`wasm-pack` or the crate's build setup).
- `routstr-sdk/client/TlsnVerifier.ts`:
  - `IoChannel` adapters: Bun TCP (`Bun.connect`) for channel C, WebSocket for
    channel B.
  - Policy: upstream host allowlist per model.
  - Comparator: `messages`/`tools` deep-equality vs the SDK's own request,
    model-mapping normalization table, response byte-equality (SSE:
    event-sequence equality), redaction check (reject if `authorization` value
    is opened).
  - `UpstreamVerification` type in `core/types.ts`:
    `{status:"verified"|"mismatch"|"unavailable", ...}`.
- `RoutstrClient.ts`: `verify: "tlsn"` option; hang the verifier off the
  existing stream tee (~line 739); attach the result to the response.
- Browser entrypoint: stub with a clear "raw TCP unavailable in browser" error.

### M4 — routstrd wiring

- `verifyUpstream` config flag; forward through `routeRequests`; CLI/daemon
  output shows `verified ✓ <host>` / `mismatch ✗` / `unavailable` next to the
  provider URL it already prints.

### M5 — e2e harness

- A script that starts: mock upstream + proverd + routstr-core (`uv run`) +
  routstrd (`bun`) pointed at the LOCAL node; runs a non-streaming then a
  streaming chat completion; asserts `verified ✓`.
- Leave hooks for a REAL API key later (`.env.example` entries for the node's
  upstream key). Do NOT invent or hardcode keys.

## Rules

- First action: verify toolchain (`cargo --version`, `wasm-pack --version` /
  `clang --version` ≥ 16, `bun --version`, `uv --version`) and report versions.
- Commit per milestone in the respective copy's git repo.
- Keep a running `STATUS.md` at `provable-ai/STATUS.md`: per milestone — what
  works, what doesn't, timings, what's next. Update it as you go; I read it to
  steer.
- Small, boring, working steps. Get M1 green before touching Python.
- No real API keys yet. Mock/fixture upstreams only.
- If you hit a design fork the doc doesn't answer, write it in STATUS.md and
  stop that thread of work; continue with something else or wait for steering.
