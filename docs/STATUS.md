# TLSN-for-Routstr Build Status

Builder agent log. Updated as work progresses. Architect steers via this file.

## Toolchain check (2026-10-06)

| Tool | Version | Status |
|---|---|---|
| cargo | 1.95.0 | OK |
| rustc | 1.95.0 | OK (matches tlsn rust-toolchain 1.95.0) |
| clang | 21.1.8 | OK (>= 16 for wasm) |
| bun | 1.3.13 | OK |
| uv | 0.11.14 | OK |
| node | 24.15.0 | OK |
| wasm-pack | NOT INSTALLED | needed at M3 only |

## Source-of-truth deltas found while reading vendored tlsn

- `JsVerifier.connect()` takes a `JsIo` object (IoChannel JS interface), NOT a
  `prover_url` string. The SDK will wrap its WebSocket in an IoChannel itself.
  (Brief/design said `connect(prover_url)`; actual:
  `tlsn/crates/wasm/src/verifier/mod.rs`.)
- wasm verifier flow is: `connect(prover_io)` → `setup()` (returns server name
  in proxy mode) → `set_server_socket(server_io)` → `run()` → `verify()`.
- `ProverConfig` is currently empty (no fields); transcript limits live
  elsewhere (SessionConfig has `max_num_streams`; no max_sent/max_recv on
  SessionConfig in this rev — TBD where recv cap is enforced; proverd will
  enforce `max_recv_bytes` itself by aborting the response read).
- Native proxy-mode example exists: `tlsn/crates/examples/proxy/proxy.rs` —
  proverd + the M1 test verifier are modeled directly on it.
- tlsn workspace `[patch.crates-io]` only touches `ws_stream_wasm`
  (wasm-only); proverd native path deps are unaffected.

## M1 — proverd + native Rust test verifier ✅ GREEN (2026-10-06)

Built:
- `proverd/` standalone crate (NOT in tlsn workspace; path deps into it;
  tlsn `mozilla-certs` feature on for real-world roots).
- axum: `POST /sessions`, `GET /ws?session_id=`, `GET /sessions/{id}/status`,
  `GET /healthz`. Binary: `proverd`, env `PROVERD_BIND` (default
  127.0.0.1:7047), `PROVERD_EXTRA_ROOT_CERT_PEM` for fixture/self-signed roots.
- `ws_io::WsPipe` — generic ws→AsyncRead/Write adapter matching
  ws_stream_wasm framing (1 write = 1 binary msg; control frames skipped, no
  empty-chunk EOF hazard). Shared between server (axum) and test client
  (tungstenite via newtype).
- Proxy-TLS prover flow per `examples/proxy/proxy.rs`; reveal sent minus
  redacted header VALUES + reveal recv-all + server identity. Redaction scan
  is case-insensitive on header names.
- Session matching POST↔ws by `session_id` (either arrival order), 30s
  ws-wait timeout, per-session status store.
- `proverd/tests/e2e.rs`: fixture TLS server (tlsn-server-fixture,
  self-signed, `/protected` route requiring a bearer token) + proverd +
  native verifier over REAL WebSocket (tokio-tungstenite) + raw TCP relay.

Verified assertions in e2e:
- POST /sessions → 200, body streamed.
- Disclosed request shows `GET /protected`, `authorization:` header present
  with value redacted (🙈); token string absent from revealed bytes.
- Disclosed response body == body returned by POST (byte-equal).
- Server identity = test-server.io enforced by verifier (it dialed it).
- SessionStatus::Complete with timings.

Timings (debug build, localhost, 509B response):
- ws_wait=0ms tls_setup=2224ms ttfb=169ms transfer≈0ms proof=1404ms
  total=3799ms (sent=109B recv=509B)
- Note: debug build; release will cut tls_setup/proof substantially. Also
  first-hit latency includes JIT-free cold codepaths; steady state TBD in P2.

Architect decisions ack'd: status endpoint (no trailers), connect(JsIo)
delta, max_recv_bytes enforced by proverd abort (implemented: aborts body
forwarding + session past cap).

Known gaps / notes for later milestones:
- `Connection: close` hardcoded on upstream request (M1). Keep-alive /
  multi-request sessions = later.
- ws sitting in matcher with no POST never times out (leak, M1-acceptable).
- POST body is a JSON string field (`request.body`) — binary bodies need
  base64 later if we verify non-JSON endpoints.
- No auth on proverd's HTTP/ws endpoints (localhost sidecar assumption).

### Progress
- [x] Toolchain check
- [x] proverd scaffold + ws adapter
- [x] prover session runner (proxy mode)
- [x] native e2e test green + timings
- [x] unit tests (redaction ranges, complement ranges)

## M2 — routstr-core verified mode ✅ GREEN (2026-10-06)

Contract implemented per architect note (session id from SDK header,
`x-routstr-verify: tlsn-proxy` opt-in, server name from provider base_url,
billing intact).

Changes:
- `routstr/core/settings.py`: `TLSN_PROVERD_URL` env (empty = disabled;
  verified requests then fail loud 400).
- `routstr/upstream/tlsn_verified.py` (new): `forward_verified_via_proverd`
  — builds the proverd payload, byte-passthrough non-streaming, SSE
  passthrough streaming, header-only cost metadata.
- `routstr/upstream/base.py`: verified branch in `forward_request` after
  `prepare_request_body`; `prepare_headers` strips the two control headers.
- `routstr/payment/models.py`: `/v1/models` annotates each model with
  `tlsn: {mode: "proxy", upstream_hosts: [...]}` when proverd configured;
  `false` for nested routstr providers.

Non-streaming verified mode:
- Body byte-passthrough (no model rewrite, no usage.cost/metadata/cost
  injection); cost only in `x-routstr-cost-*` headers.
- Forces `accept-encoding: identity` upstream (httpx transparent
  decompression would break byte-equality with the transcript).
- Upstream errors map through the existing `forward_upstream_error_response`.
- Reads proverd response via `aiter_raw` (no decompression).

SSE verified mode:
- Bytes forwarded untouched (no chunk rewriting/swallowing), parsed on the
  side for usage; billing settles post-stream via a fresh DB session
  (`PersistentStreamFinalizer`, same ownership pattern as unverified).

### DESIGN DELTA — SSE cost transport (DEFERRED per architect 2026-10-06)
Cost headers can't be emitted for streaming (headers precede the stream; the
usage arrives at the end). Current behavior: verified SSE settles billing
**silently** — client observes the balance delta on its next request.
Architect direction (revisit later, not blocking): the SDK can make a
**separate post-response request** to the node to fetch the usage/cost for a
settled session (e.g. `GET /v1/usage/{session_id}`-shaped; needs a small
cost-lookup endpoint keyed by tlsn session id — park for P2/P3).

### COMPARATOR NOTE for M3 (SDK) — confirmed by architect
Disclosed transcript = raw HTTP/1.1 wire bytes. SSE responses are usually
`transfer-encoding: chunked`, so the comparator must do HTTP response parsing
(status line, headers, de-chunk) before comparing to channel-A body. (The
live test does exactly this.)

Tests (all with `uv run --python 3.13`; see python note below):
- `tests/unit/test_tlsn_verified.py` — 6 green: 400s (no proverd config, no
  session header), payload shape (server_name, redact list, identity
  encoding, control headers stripped), byte-passthrough + cost/verified
  headers, upstream-error mapping, SSE passthrough + post-stream billing from
  captured usage chunk.
- `tests/integration/test_tlsn_verified_live.py` — 2 green, FULL PROOF LOOP:
  real proverd + `mock_upstream` (new Rust example: chat-shaped OpenAI-ish
  TLS server with fixture certs, non-stream + SSE) + native `test_verifier`
  (new proverd CLI example; prints transcript JSON). Asserts: response
  byte-equal to proven transcript body, request JSON deep-equal, credential
  value absent from disclosure, verifier exit 0.
- Regression: test_completions_billing + test_cost_response_metadata 17 green.

Live timings (debug): non-stream 2.47s, SSE 2.26s end-to-end incl. proof
(verifier proof ~0.2-0.6s).

### Env note
`uv run` with the project-pinned Python 3.14 fails here (no httptools wheel,
no Python.h on system). Used `uv run --python 3.13` — did NOT touch
.python-version. M5 harness should document this or install python3.14-dev.
## M3 — SDK verifier (IN PROGRESS — DESIGN FORK, needs architect decision)

Built so far:
- wasm pkg built from vendored tlsn (`tlsn/crates/wasm/build.sh`, wasm-pack
  0.14 + nightly — architect's installs work).
- `routstr-sdk/client/tlsn/`: IoChannel adapters (ws + Bun TCP), wasm loader,
  comparator (HTTP parse + de-chunk, SSE event-sequence equality,
  messages/tools deep-equality, model-mapping normalization, credential
  redaction check), `TlsnVerifier` facade, wasm verifier running in a
  dedicated Worker (upstream's architecture — see below).
- `core/types.ts`: `UpstreamVerification` (verified/mismatch/unavailable).
- tests/bun/tlsnVerifier.test.ts: full e2e vs real proverd + mock upstream
  (verified / SSE verified / request tamper→mismatch / response
  tamper→mismatch / policy→unavailable) — PASSES when the wasm cooperates.

### THE FORK: tlsn wasm verifier is unreliable in Bun (JSC)

Evidence (2026-10-06, ~25 sessions):
- Native Rust verifier vs proverd: **10/10 reliable** (hammer test).
- Wasm verifier in Bun (Worker, per upstream's own architecture): ~50%
  sessions fail, non-deterministically, three failure modes:
  1. prover-side `SPCOT receiver error: invalid consistency check` during
     proxy-tls preprocessing (verifier computed different OT values),
  2. verifier-side preprocessing EOF (cascade of 1),
  3. occasional post-`starting Proxy-TLS` hang.
  Not fixed by: single rayon thread, warmup sessions, empty-frame guards
  (real bug found + fixed: zero-length ws frames were being treated as EOF
  by the wasm adapter — fixed on both ends).
- Wasm verifier in Chrome: cannot run on main thread at all
  (`Atomics.wait cannot be called in this context` — parking_lot nightly
  backend). Upstream runs it in a Worker via Comlink (harness/static).
  Chrome-worker not yet attempted (requires COOP/COEP page harness).
- Conclusion: wasm verifier is only upstream-tested in real Chrome workers;
  Bun/JSC executes the MPC preprocessing incorrectly ~50% of the time
  (suspected wasm JIT tiering/atomics issue; no JS-level lever found).

### Options (architect decision needed)
1. **Native verifier backend for Bun/Node (RECOMMENDED for routstrd)**: SDK
   spawns a small Rust `tlsn-verifier` binary (shipped from proverd repo)
   and runs the TS comparator on its transcript output. 10/10 reliable
   today. Wasm backend stays for browsers (Chrome), where channel C needs a
   ws→TCP bridge anyway. Cost: routstrd ships/depends on a Rust binary.
2. Chrome-CDP sidecar for Bun: run the wasm in headless Chrome workers.
   Heavy dependency; untested here.
3. Escalate to tlsn upstream with a JSC repro and wait.

### M3 status update — CORE DELIVERABLE GREEN with native backend (2026-10-06)

Implemented the fork's option 1 (native backend) pending architect ratification:
- `proverd` repo: new `tlsn-verifier` binary (src/bin) — per-session native
  verifier, prints `READY <server>` / `VERIFIED <server>`, writes transcript
  JSON (`--transcript-out`); `--upstream IP:PORT` (tests) or
  `--upstream-host NAME` with post-commit DNS + committed-name check.
  testverifier lib now always compiled; e2e green.
- SDK `client/tlsn/`: `TlsnVerifierBackend` interface;
  `NativeVerifierBackend` (Bun.spawn per session, READY-line policy
  kill-switch); `WasmWorkerBackend` (retained; flaky under Bun/JSC per the
  fork analysis); comparator + IoChannel adapters unchanged.
- `RoutstrClient`: `verify: "tlsn"` param + client/request tlsn config;
  sends `x-routstr-verify`/`x-routstr-tlsn-session` headers; verifier begins
  pre-fetch (non-blocking); `(response).upstreamVerification` promise
  attached for non-SSE (clone+complete) and SSE (tee capture → complete
  after [DONE]); failover → `unavailable` + session cancelled.
- routstr-core: `/v1/models` tlsn annotation now includes `proverd_ws`
  (channel-B endpoint) alongside mode/upstream_hosts.
- Browser: default backend = wasm worker; channel-C dial fails with the
  explicit error "raw TCP unavailable: Bun.connect not found (browser
  builds cannot verify)" — the documented stub until a ws→TCP bridge exists.

Tests (all green):
- tests/bun/tlsnVerifier.test.ts — 5/5 with native backend (~16s): verified,
  SSE verified, request tamper→mismatch, response tamper→mismatch,
  policy→unavailable. (wasm backend opt-in via TLSN_TEST_WASM=1.)
- tests/bun/routstrClientTlsn.test.ts — 2/2: full routeRequest wiring vs a
  Bun mock node + real proverd + mock upstream; headers asserted;
  `verified ✓ test-server.io` non-SSE and SSE.
- vitest tests/unit — 624/624 (no regressions).
- tsc: zero new errors vs baseline; tsup build: zero errors from tlsn files
  (pre-existing dts failure unrelated, present on baseline).

Open items for architect:
- Ratify native-backend default for Bun/Node (option 1) or steer to option
  2/3. The wasm path + repro artifacts are in place either way.
- M4 needs the tlsn-verifier binary distributed with routstrd (build step,
  or proverd repo release artifact).
## M4 — routstrd wiring (not started)
## M5 — e2e harness (not started)

---

## Architect decisions (2026-10-06, in response to M1 notes)

1. **Proof status transport — your proposal approved.** Skip HTTP trailers
   (httpx trailer support on the Python side is poor). Final shape:
   `POST /sessions` returns `x-tlsn-session` up front + streams the response;
   proof outcome lives at `GET /sessions/{id}/status` for routstr-core
   bookkeeping. Authoritative proof delivery stays on the mux (the verifier is
   who actually cares). routstr-core only needs the session id for log
   correlation — don't overbuild its side.
2. **`JsVerifier.connect(JsIo)` delta accepted** — SDK wraps its own WebSocket
   in an IoChannel. I'll update TLSN-routstr.md to match (also: flow is
   `connect → setup → set_server_socket → run → verify`; `ProverConfig` empty,
   limits elsewhere).
3. **wasm-pack + nightly** — I'm installing wasm-pack 0.14 (prebuilt) and the
   nightly toolchain with the wasm32 target myself, in the background. Don't
   block M1 on it; it'll be ready before M3.
4. **proverd enforcing `max_recv_bytes` itself** — approved. Cap by aborting
   the upstream read past the limit and mark the session `unavailable`
   (per design doc decision 6).

— Architect

## Architect note — M1 accepted, GO for M2 (2026-10-06)

M1 review: e2e assertions are the right ones (redaction of the credential
value, byte-equality of disclosed response vs delivered body, server identity
enforced by verifier-dialed connection). Timings are debug-build localhost —
fine for M1; we'll re-measure in release at M5.

M2 contract specifics (so M3/M4 line up):

1. **Session id is generated by the SDK** (uuid). It travels on the API
   request as header `x-routstr-tlsn-session: <uuid>` AND is used by the SDK
   verifier for the ws connect (`GET /ws?session_id=<uuid>`). routstr-core
   just forwards it into `POST /sessions`. This gives you pairing without the
   node inventing anything.
2. **Opt-in header**: `x-routstr-verify: tlsn-proxy` on the API request
   triggers verified mode (when `TLSN_PROVERD_URL` is set; otherwise respond
   400 `tlsn not available` — fail loud, never silently unverified).
3. **Server name source**: routstr-core passes the upstream host from the
   provider's `base_url` into `POST /sessions.server_name`. The SDK verifier
   learns it from `setup()` and enforces its policy then — you don't need to
   negotiate it in advance.
4. **Billing must keep working**: verified mode changes the *transport* of the
   upstream call, not payment. Cost headers still emitted; only `usage.cost`
   body injection is skipped in verified mode.
5. Keep `Connection: close` for now (matches M1); SSE comes after
   non-streaming is green end-to-end.

— Architect

---

## Upstream conversion (2026-10-07)

Everything from M1–M3 now exists as pushable artifacts; the lab copies are
untouched (work preserved, per architect).

### 1. New repo: `proverd` (staged at `/home/user/projects/routstr_main/proverd`)
Decision: dedicated repo (Rust component, own release artifacts), not a
subdir of routstr-core. Made self-contained:
- `tlsn` via git dep pinned to `tlsnotary/tlsn@4415391333bd…` (not on
  crates.io); no `../tlsn` path deps.
- Test certs copied into `fixtures/`; the TLS fixture server moved into the
  lib (`src/testfixture.rs`), so no `publish = false` tlsn crates needed.
- README (protocol, HTTP surface, CLI, limits), `.gitignore`, fresh
  single-commit history. `cargo test` green; `proverd`, `tlsn-verifier`,
  `mock_upstream` all build.
- License: MIT OR Apache-2.0 (matches tlsn + SDK). routstr-core is GPL-3.0 —
  separate-process boundary; architect may prefer GPL for consistency.

### 2. Worktrees (created from the originals at origin/main)
- `routstr-sdk/.worktrees/tlsn-verifier` — branch `feat/tlsn-verifier`
  (2 commits). 624/624 vitest, 8/8 bun e2e — verified against the new
  proverd repo's binaries.
- `routstr-core/.worktrees/tlsn-verified-mode` — branch
  `feat/tlsn-verified-mode` (3 commits). 2393 unit tests pass, 8/8 TLSN
  tests pass (live proofs) — verified against the new repo's binaries.
  One conflict resolved (settings.py); base.py/models.py auto-merged.
- `routstrd` — untouched; its working tree is dirty (package.json,
  recovery-probe) so M4 should start in its own worktree too.

### 3. Portability fixes folded into the PR commits
Bun/Python e2e tests resolve binaries/fixtures from env
(`PROVERD_BIN`, `TLSN_VERIFIER_BIN`, `MOCK_UPSTREAM_BIN`,
`TLSN_FIXTURE_CA_{DER,PEM}`, `TLSN_LAB_DIR`) with lab-layout fallback, and
skip loudly when absent — so they are meaningful in CI, not just here.
(Also fixed: the routstr-core live test was silently skipping after the
verifier moved from `examples/test_verifier` to `src/bin/tlsn-verifier`.)

### Not done yet (needs architect actions / M4)
- No pushes: local branches only. proverd needs a GitHub repo + remote
  (`Routstr/proverd`), then `push -u origin main`.
- PRs: feat/tlsn-verifier → routstr-sdk; feat/tlsn-verified-mode →
  routstr-core.
- routstrd M4 work + binary packaging (fetch/build tlsn-verifier).
- Forward-dev note: the worktrees are now the live branches; the lab copies
  in provable-ai are frozen snapshots for reference.

### Pushed (2026-10-07)
- `Routstr/proverd` — `main` @ 5ab1b37 (initial commit, whole repo).
- `Routstr/routstr-sdk` — `feat/tlsn-verifier` @ d5543a5 (2 commits).
  PR: https://github.com/Routstr/routstr-sdk/pull/new/feat/tlsn-verifier
- `Routstr/routstr-core` — `feat/tlsn-verified-mode` @ 7eb7e43 (3 commits).
  PR: https://github.com/Routstr/routstr-core/pull/new/feat/tlsn-verified-mode
  (routstr-core `origin` push URL switched to the SSH alias
  `git@github.com-red:…` so pushes work non-interactively.)
PRs not opened yet — awaiting architect.

---

## Architect verification — M1–M3 ACCEPTED (2026-10-07)

Independent spot-checks by the architect (not the builder's claims):

- `proverd` release e2e (`proxy_tls_e2e`): **pass, 1.56s** (vs 3.8s debug —
  release cut the compute cost as predicted).
- SDK bun tlsn suite (worktree, env pointed at `Routstr/proverd` binaries):
  **7/7 pass** — non-streaming verifies, SSE verifies post-stream,
  request-tamper → mismatch, response-tamper → mismatch,
  host-policy → unavailable, RoutstrClient wiring ×2.
- Artifacts confirmed: `Routstr/proverd@5ab1b37`,
  `routstr-sdk/.worktrees/tlsn-verifier@d5543a5`,
  `routstr-core/.worktrees/tlsn-verified-mode@7eb7e43c` (+1329 lines incl.
  `routstr/upstream/tlsn_verified.py` 510, unit 357, live 397).

Issues found during verification (for M4 agent / proverd repo):

1. **SDK test LAB fallback breaks from worktrees**: `TLSN_LAB_DIR` default
   resolves relative to the checkout, so from `.worktrees/*` it lands on
   `routstr-sdk/provable-ai/…`. Must export `PROVERD_BIN`,
   `TLSN_VERIFIER_BIN`, `MOCK_UPSTREAM_BIN`, `TLSN_FIXTURE_CA_{DER,PEM}`.
   Fix the fallback or document the env contract in the test README.
2. **proverd repo ships only `.der` fixtures** but `PROVERD_EXTRA_ROOT_CERT_PEM`
   needs a PEM. Ship `fixtures/root_ca.crt` (PEM) in the proverd repo so the
   e2e path is self-contained.
3. proverd silently fails to serve when the extra-root PEM path is unreadable
   — make that a loud startup error.

Builder's pending confirmations — answered:

1. **Wasm backend stays browser-only** (needs ws→TCP bridge); native backend
   is the Bun/Node path. CONFIRMED.
2. **Verified SSE billing may stay silent** until the post-response
   usage-lookup endpoint (keyed by tlsn session id) exists. CONFIRMED — P3.

M4/M5 handoff: see `HANDOFF-M4M5.md`. The lab copies in provable-ai are now
frozen snapshots; the live branches are the three locations above.

— Architect

---

## M4 builder — setup report + plan (2026-10-07)

Setup state (all verified, baseline green):
- routstrd worktree: `routstrd/.worktrees/tlsn-verify` @ 7c53ebc (branch
  `feat/tlsn-verify` from origin/main). Dirty main tree untouched.
  `.worktrees/` already in routstrd `.gitignore`.
- SDK linked via `bun link` → `routstr-sdk/.worktrees/tlsn-verifier`
  @ d5543a5, dist rebuilt (tsup clean).
- Baseline: routstrd `tsc --noEmit` clean, 476/476 bun tests pass.
- proverd @ 5ab1b37: debug + release binaries present; built release
  `mock_upstream` (`cargo build --release --examples`).
- Gotcha audit: #2 already fixed (`fixtures/root_ca.crt` committed);
  #3 already loud (`src/main.rs:29` errors with context on unreadable PEM);
  #1 (SDK test LAB fallback from worktrees) still open — will fix fallback
  to try the sibling `routstr_main/proverd` repo layout.
- Toolchain: bun 1.3.13, node 24.15.0, rustc 1.95.0, uv 0.11.14.

Integration plan (gaps found while reading the code):

1. **SDK additions needed first** (small commits on `feat/tlsn-verifier`):
   - `routeRequests()` does not thread `verify`/`tlsnOptions` through to
     `client.routeRequest` — add `verify?: "tlsn"` + `tlsn` options, derive
     `proverWsUrl`/`upstreamHostAllowlist` from the selected model's `tlsn`
     annotation (`/v1/models` passthrough preserves it), pass into
     `resolveRequestContext` so the created `RoutstrClient` gets the backend.
   - `NativeVerifierBackend` throws without `dialTo` ("no DNS resolution
     yet") — the binary has `--upstream-host` (post-commit DNS + committed
     name check) since M3; add production path: no `dialTo` → dial
     `--upstream-host allowlist[0]`.
   - `Model` type: declare `tlsn?: { mode; upstream_hosts; proverd_ws } | false`.
   - Export `client/tlsn` from package index (routstrd needs
     `NativeVerifierBackend` + `UpstreamVerification`).
2. **routstrd** (`feat/tlsn-verify`): config (`verifyUpstream`,
   `tlsn.verifierBin`, `tlsn.upstreamHostAllowlist`, `tlsn.tor` deferred),
   verifier-bin resolution (env → config → `~/.routstrd/bin/tlsn-verifier` →
   loud error w/ install instructions), http wiring + `x-routstr-tlsn-status`
   header, logs, usage-record field, tests, build script, README.
3. **Header semantics fork** (flagging for architect): for SSE the verdict
   only exists after `[DONE]`, but response headers go out before the stream.
   Plan: non-SSE → await verification, then send final header
   (`verified:<host>` / `mismatch:<reason>` / `unavailable:<reason>`); SSE →
   send `x-routstr-tlsn-status: pending` up front, log the final verdict
   post-stream (`verified ✓ <host>` etc.). No trailers (architect already
   rejected them for the core hop; same argument applies to Node clients).

— M4 builder

## Architect note — SSE status transport (M4) (2026-10-07)

M4 builder's plan endorsed as-is: non-SSE → await verification, then final
`x-routstr-tlsn-status`; SSE → `pending` up front + final verdict in daemon
logs post-stream. Correct call on trailers (same httpx/client-compat argument
as the core hop). One cheap addition if it's already wired: if the daemon's
request/response log sink is queryable over HTTP, expose the final verdict
through that record (keyed by request/session id) so API consumers can poll
post-stream. If the sink isn't queryable today, log-only is fine for v1 —
don't build a new endpoint for it.

— Architect

---

## M4 — routstrd wiring: CODE GREEN (2026-10-08)

All M4 items implemented and committed in `routstrd/.worktrees/tlsn-verify`
(branch `feat/tlsn-verify`):

- `96f5bf9` M4 main commit: config (`verifyUpstream`, `tlsn.verifierBin`,
  `tlsn.upstreamHostAllowlist`, `tlsn.tor` = loud deferred error), binary
  resolution (env → config → `~/.routstrd/bin/tlsn-verifier` → loud error w/
  install instructions), daemon startup validation (refuses to start when
  verifyUpstream is on but the binary is missing), http wiring with
  `x-routstr-tlsn-status` header, per-request `[tlsn]` verdict log lines
  (`verified ✓ <host>` / `mismatch ✗` / `unavailable`), usage-record
  `upstreamVerification` field (type only — SDK persistence flagged),
  `scripts/build-tlsn-verifier.sh` (clones Routstr/proverd @ 5ab1b37, release
  build → `~/.routstrd/bin/`; tested end-to-end: fresh clone, 47s build,
  installed binary runs), README section.
- `8a233f5` `tlsn.rootCertPemFiles` → NativeVerifierBackend (fixture CA for
  M5, corporate MITM CAs).
- SDK side (`feat/tlsn-verifier`, needed by the above): `bff94e4`
  routeRequests verify/tlsn passthrough + model-advertisement resolution +
  native backend production dial (`--upstream-host`) + test path resolver
  (fixes gotcha #1 — bun e2e now green from worktrees with NO env set);
  `dec7ddc` caller allowlist intersects (never expands) the node
  advertisement.

Verification state:
- routstrd: `tsc --noEmit` clean; **494/494 bun tests** (476 baseline + 18
  new: utils/tlsn resolution/formatting + daemon http header/log semantics
  via a new `routeRequestsImpl` test seam).
- SDK: tsc clean; 631/631 vitest (same as baseline); 7/7 bun tlsn e2e
  (release binaries, ~9s, no env needed).
- Header semantics per the M4 plan (fork flagged earlier): non-SSE final
  verdict header; SSE `pending` + post-stream log line. Awaiting architect
  ratification.

Open for architect:
1. Usage-record persistence of the verdict needs an SDK driver hook (append
   happens inside the SDK before the SSE verdict exists). Type field is in;
   wiring is P3-cheap. Defer?
2. `routeRequestsImpl` test seam added to daemon deps — acceptable?
3. Gotcha #1 fixed on the SDK branch; gotchas #2/#3 were already resolved
   in Routstr/proverd@5ab1b37.

— M4 builder

## M5 — e2e harness (in progress)

Local stack: podman nutshell mint (cashubtc/nutshell:0.17.0, FakeWallet —
verified serving /v1/info), mock_upstream, proverd, routstr-core (uv run,
admin-API provider + batch-override model registration), routstrd worktree
daemon. Adversarial case via a Bun channel-A tamper proxy. Details when green.

## Architect answers to M4 builder questions (2026-10-07)

1. **Usage-record persistence of the verdict — DEFER, confirmed.** v1
   surfaces: `x-routstr-tlsn-status` header + daemon logs. Persistent
   per-request verdict records are P3 (together with the queryable sink).
2. **`routeRequestsImpl` test seam — acceptable.** DI seams for tests are
   fine; keep it undocumented-internal (not part of the public daemon API)
   and note it in the commit message.
3. Gotcha fixes ack — thanks for pushing #2/#3 into Routstr/proverd and #1
   onto the SDK branch.

Mint via podman + the compose.testing.yml recipe was the right call.

— Architect

---

## M5 — e2e harness: GREEN (2026-10-08)

`routstrd/.worktrees/tlsn-verify/scripts/e2e-tlsn.ts` (commit `82355ab`):
full local stack in one command — **ALL E2E CHECKS PASSED, twice
back-to-back**, cleanup verified (no leftover containers/run dirs).

Stack: podman nutshell mint (FakeWallet) → mock_upstream (TLS, fixture CA,
test-server.io) → proverd → routstr-core (verified mode) → routstrd
(worktree daemon, `verifyUpstream: true`).

Assertions (all green):
1. Non-streaming chat completion: HTTP 200,
   `x-routstr-tlsn-status: verified:test-server.io`, daemon log
   `verified ✓ test-server.io (gpt-mock) [proof ~500ms]`. Cost headers
   intact (billing works; design constraint #4 from M2 notes).
2. Streaming: header `pending`, SSE delivers through `data: [DONE]`,
   post-stream log `verified ✓` (~480ms proof).
3. Adversarial (channel-A tamper proxy rewrites the response body):
   `x-routstr-tlsn-status: mismatch:proven response body != response
   received on channel A`, log `mismatch ✗`, tampered body still delivered
   (post-hoc, never blocks).

### Harness findings (env/setup gotchas, all baked into the script)

1. **nutshell must be ≥ 0.20.x** — the compose.testing.yml image
   (0.17.0) serves `/v1/keys` keysets WITHOUT `active`, which cashu
   0.20.0's wallet model requires → every token redemption fails
   client-side ("mint unreachable"). Harness uses
   `cashubtc/nutshell:0.20.3` + `MINT_RATE_LIMIT=False` +
   `MINT_INPUT_FEE_PPK=0`. **compose.testing.yml is stale** for
   wallet-redemption paths — worth a core PR bumping the image.
2. **SDK/node pricing race**: core recomputes sats pricing from the BTC
   rate continuously; the SDK spends its cached estimate → 402 by
   sub-sat deltas, and the topup logic then deadlocks on a stale
   "sufficient" check. Harness sets tiny model pricing via admin
   batch-override so any SDK spend covers the charge. (Real-network
   implication: price drift between listing cache and node charge is a
   live UX issue, not harness-specific — flagging.)
3. **Nostr review sync fails CLOSED**: providers without a positive
   kind-38425 review are disabled — including a local/private node.
   Harness enables the pinned provider explicitly (manual overrides are
   exempt). VPS phase: if the user's node lacks a review from the default
   pubkey, routstrd needs `providers enable` too.
4. **Pinned private nodes were broken**: daemon never passed the pinned
   provider to ModelManager.includeProviderUrls AND the SDK's warm-cache
   bootstrap dropped includeProviderUrls → "does not offer model"
   forever. Fixed in routstrd `82355ab` + SDK `bcf4a2a`.
5. **Provider base_url needs /v1** (`.env.example` style) or core builds
   upstream paths the mock 404s.
6. **Core venv**: worktree pins python 3.14 (httptools has no wheel,
   build fails without headers); `uv sync --python 3.11` works and is
   what the harness uses.
7. routstrd daemon on this machine needs `COCOD_DIR` pointed away from
   the live legacy cocod (exclusion lock).

### Fixes shipped during M5 (beyond the harness)

- routstrd `82355ab`: harness + includeProviderUrls for pinned provider +
  `tlsn.dialTo` config (channel-C override; committed-name policy still
  enforced) + docs.
- SDK `bcf4a2a`: ModelManager honors includeProviderUrls on cache-valid
  bootstrap. 624/624 vitest, 7/7 bun tlsn e2e re-verified after the change.

### Branch state (all local, unpushed — awaiting architect)

- `Routstr/proverd` — untouched @ 5ab1b37 (gotchas #2/#3 were already in).
- SDK `feat/tlsn-verifier`: d5543a5 → `bff94e4` → `dec7ddc` → `bcf4a2a`.
- routstrd `feat/tlsn-verify`: 7c53ebc → `96f5bf9` → `8a233f5` → `82355ab`.
- routstr-core: untouched (no changes needed for M4/M5; the
  compose.testing.yml image bump is a candidate small PR).

### Open items for architect (carried + new)

1. Ratify: SSE header `pending` + post-stream log (vs no header).
2. Ratify: `routeRequestsImpl` test seam in daemon deps.
3. Usage-record verdict persistence (needs SDK driver hook) — defer to P3?
4. NEW: SDK topup deadlock on sub-sat price drift (finding 2) — real bug,
   worth an SDK issue; not TLSN-specific.
5. NEW: compose.testing.yml nutshell 0.17.0 → ≥0.20.x bump (core repo).
6. Push branches + PRs when ratified.
7. VPS phase: channel-B proxy in core (`GET /v1/tlsn/ws` → proverd) is the
   known deployment gap; then deploy + measure TTFT premium.

— M4/M5 builder

---

## Architect verification — M4+M5 ACCEPTED; M1–M5 COMPLETE (2026-10-08)

Independent run of `bun scripts/e2e-tlsn.ts` (routstrd worktree @ 82355ab):

- **Non-streaming**: 200, `x-routstr-tlsn-status: verified:test-server.io`,
  daemon log `verified ✓`.
- **Streaming**: 200, header `pending` up front, SSE ends `[DONE]`, post-stream
  log `verified ✓` (the endorsed SSE semantics).
- **Adversarial** (channel-A tamper proxy): 200 (delivery never blocked),
  `mismatch:proven response body != response received on channel A`,
  log `mismatch ✗`. Post-hoc fraud detection works exactly as designed.

Stack proven end-to-end: podman nutshell mint (FakeWallet) → routstrd wallet
payment → local routstr-core (verified mode) → proverd → mock upstream, with
the SDK verifier inside routstrd dialing channel C and validating the proof.

M1–M5 all green and independently verified. Next phase: VPS live testing —
close the channel-B proxy gap in routstr-core first (client can't reach
proverd's localhost bind on a remote node; advertise a node-proxied
`/v1/tlsn/ws` in `proverd_ws`), then deploy proverd + node on the VPS with
the real upstream key and measure the TTFT premium against the design table.

— Architect

---

## VPS live test — Phase 1: channel-B proxy GREEN, pushed (2026-10-08)

Core `feat/tlsn-verified-mode` commit `440a3357` (pushed to origin):
- `routstr/tlsn_ws.py`: `GET /v1/tlsn/ws?session_id=<uuid>` bidirectional ws
  proxy to proverd's `GET /ws` (binary passthrough, close propagation both
  ways, uuid-validated ids → 1008, proverd-unreachable → 1011, 16 MiB
  frames). Registered only when `TLSN_PROVERD_URL` is set.
- `/v1/models` now advertises `tlsn.proverd_ws` as the node's OWN public
  proxy URL (`wss://<HTTP_URL host>/v1/tlsn/ws`); direct proverd address is
  the fallback when `HTTP_URL` is unset (local dev).
- Tests: 9 new (`tests/unit/test_tlsn_ws.py`, real websockets mock proverd
  in a thread). 2402 unit tests pass, ruff clean.
- Gotcha: starlette TestClient raw `receive()` returns `websocket.close` as
  a message — only the typed helpers raise WebSocketDisconnect.
- **Full M5 harness re-run against this core with `HTTP_URL` set, so the
  real verifier dials `/v1/tlsn/ws` through the new proxy: ALL E2E CHECKS
  PASSED** (verified/mismatch paths identical). routstrd harness commit
  `fdd3821`.

DoS note documented in code: unauthenticated connects with unknown session
ids die at proverd's 30s pairing timeout.

### Phase 2: VPS deployment LIVE (2026-10-08)

- proverd `f3ea316` (pushed to Routstr/proverd main): multi-stage Dockerfile
  (rust:1.95-bookworm → debian-slim, PROVERD_BIND=0.0.0.0:7047) +
  .dockerignore. Built green locally under podman AND on the VPS docker;
  `/healthz` smoke-tested.
- Core branch merged origin/main (was 16 behind — a plain checkout would
  have regressed the live node): merge `aa86656d`, 2416 unit tests pass,
  pushed.
- VPS: `/home/debian/projects/routstr-core` checked out
  `feat/tlsn-verified-mode` @ aa86656d (tree was clean; no clobber).
  proverd cloned to `/home/debian/projects/proverd` @ f3ea316.
- `compose.override.yml` (untracked, documented inline): proverd service
  (build from the local clone, restart unless-stopped, stop_grace_period 3s
  — proverd ignores SIGTERM >10s, noted for a future fix) +
  `TLSN_PROVERD_URL=http://proverd:7047` on the routstr service. Tracked
  `compose.yml` and `.env` untouched.
- **nginx needed NO changes**: ppq.redsh1ft.com already proxies
  Upgrade/Connection on `location /` with `proxy_read_timeout 86400`.
- Stack rebuilt (`up -d --build`), brief downtime as sanctioned.
  `/v1/info` healthy ("PPQ Mirror", v0.5.0). **All 387 models advertise
  `tlsn: {mode: proxy, upstream_hosts: [api.ppq.ai], proverd_ws:
  wss://ppq.redsh1ft.com/v1/tlsn/ws}`.**
- Public channel-B smoke test from here: ws upgrade to
  `wss://ppq.redsh1ft.com/v1/tlsn/ws?session_id=<uuid>` succeeds; server
  logs show the full chain (core `tlsn ws proxy: session paired` → proverd
  `verifier websocket connecting`).
- Pre-existing noise: "Binance API error" price-feed warnings in core logs
  (unrelated to TLSN; sats pricing falls back).
- Rollback: `git checkout main && rm compose.override.yml && docker compose
  up -d --build` in the core dir.

### Phase 3: live verified test — FIRST LIVE VERIFIED INFERENCE (2026-10-08)

Stack: local routstrd (worktree build, `verifyUpstream: true`, provider
pinned to `https://ppq.redsh1ft.com`, allowlist `api.ppq.ai`) → VPS node →
proverd sidecar → api.ppq.ai. Model: `glm-5.3-flash` (node forwards it as
`z-ai/glm-5.3-flash`). Wallet: mint.cubabitcoin.org. All requests
`max_tokens: 16`, each billed 14 msats.

**Results:**
1. Non-streaming: HTTP 200, `x-routstr-tlsn-status: verified:api.ppq.ai`,
   `x-routstr-tlsn-session: ecf9e634-…`, log `verified ✓ api.ppq.ai
   (z-ai/glm-5.3-flash) [proof 4198ms]`.
2. Streaming: header `pending`, SSE through `data: [DONE]`, post-stream
   `verified ✓ api.ppq.ai [proof 3596ms]`. proverd logs confirm
   session-id pairing POST↔ws (1446993a-…; core `tlsn ws proxy: session
   paired`).
3. TTFT comparison (streaming, same model/prompt):
   **unverified 1.14s vs verified 11.97s → +10.8s premium.**
   proverd SessionTimings for the verified stream: ws_wait 636ms,
   tls_setup 7262ms (channel-C TLS+relay from HERE to api.ppq.ai — the
   dominant cost; verifier and upstream are on different continents),
   ttfb 2927ms, transfer 99ms, proof 3295ms, total 14835ms. Consistent
   with the design doc's latency table, scaled up by geography: the
   constant is dominated by verifier↔upstream RTT, not compute.
   Expect much less from a verifier co-located with the upstream.

**Bugs found by live testing (all fixed, all pushed):**
1. proverd `36d4d40`: tokio-tungstenite had no TLS feature → verifier
   could not dial `wss://` channel B ("TLS support not compiled in") →
   every verified request 424'd node-side ("timed out waiting for
   verifier websocket"). Enabled `rustls-tls-webpki-roots`; pinned
   aws-lc-rs CryptoProvider in both binaries (feature unification left
   rustls with ring+aws-lc-rs and it refuses to auto-select).
2. SDK `3f4e32d`: comparator rejected legitimate node request mapping —
   (a) path: core strips `/v1` before joining the provider base_url
   (`https://api.ppq.ai` has none) → proven path `/chat/completions` vs
   channel-A `/v1/chat/completions`; now tolerates exactly one `/v1`
   segment difference. (b) model id: core forwards the provider
   candidate's id (`glm-5.3-flash` → `z-ai/glm-5.3-flash`).
3. Core `93a08f4c`: `/v1/models` tlsn block now advertises
   `upstream_models` (the exact ids the proxy would put on the wire, from
   `get_candidates` forwarded_model_id/id); SDK comparator accepts exactly
   the advertised ids (modelMapping now `string|string[]`). 385/387 live
   models advertise it. **Protocol addition — architect ratify.**
4. SDK `7d618d2`: verifier handle was never cancelled on terminal request
   failure → during the 424 storm each failed request leaked a
   tlsn-verifier process (multi-GB RSS each, ~20GB observed, orphaned to
   init). Now cancelled in the `_makeRequest` catch.
5. proverd `36d4d40`: `tlsn-verifier` watchdog (default 600s,
   `TLSN_VERIFIER_WATCHDOG_SECS`) — the process self-terminates even when
   its caller abandons it.

**Ops gotchas (live-fire lessons):**
- routstrd CLI `daemon`/`start`/`restart` spawn semantics differ: `start`
  from the worktree CLI spawned the worktree src, but `restart` (and the
  global install) can put the STALE installed main-checkout dist on :8008
  — pre-M4 code silently serving with verifyUpstream inert. The user's
  global routstrd install is stale; install the worktree build when
  ratified. Detect with `ss -tlnp | grep 8008` + argv inspection.
- `pkill -f` against the worktree pattern misses `bun src/index.ts
  daemon` argv (relative path) — stale daemons survive "restarts" and
  hold the port + wallet lock.
- SDK warm model cache keeps stale advertisements across daemon
  restarts; delete `sdk_storage` rows for the provider when the listing
  schema changes.
- nginx on the VPS already had `Upgrade`/`Connection` +
  `proxy_read_timeout 86400` on `/` — no ws-specific config needed.
- A second stack (`routstr-defaults-*` containers) lives on the VPS —
  untouched.
- VPS core redeploy after a force-push needs `git reset --hard
  origin/feat/tlsn-verified-mode` (ff-only pull fails).

**Verifier leaks:** all 8 orphaned tlsn-verifier processes killed; VPS
proverd healthy (1 process). Daemon restored to the user's pre-test
config (provider unpinned, verifyUpstream absent) and left RUNNING the
worktree build on :8008 (PID 1956583) — user's traffic unaffected.

### Open items for architect (updated)

1. Ratify protocol additions: `tlsn.upstream_models` advertisement +
   comparator `/v1`-path-prefix tolerance + `string|string[]`
   modelMapping.
2. Ratify earlier forks: SSE `pending` header semantics,
   `routeRequestsImpl` test seam, usage-record verdict persistence (P3?).
3. TTFT premium is geography-dominated (7.3s of 10.8s is channel-C TLS
   setup). Mitigations if it matters: verifier↔upstream co-location
   guidance, TLS session resumption across requests (proverd-side),
   or batched verification.
4. routstrd spawn-path footgun (restart → stale installed dist) — worth
   an upstream issue; users on stale builds get silently-unverified
   "verified" mode... which fails LOUD per-request (unavailable), so the
   failure mode is visible, not silent.
5. Push/PR SDK `feat/tlsn-verifier` (5 commits) when ratified. VPS
   proverd image rebuild pulls verifier fixes next deploy (server-side
   proverd unaffected by them; optional).
6. VPS rollback remains: `git checkout main && rm compose.override.yml
   && docker compose up -d --build`.

— channel-B/VPS/live-test builder
