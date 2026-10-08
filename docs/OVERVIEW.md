# TLSN verified mode for Routstr — start here

This folder is the complete documentation trail for Routstr's **TLSN verified
mode**: cryptographic proof that an AI inference response really came from the
claimed upstream provider, with the node's upstream API key kept hidden.

If you are new to this work, read this file first, then follow the reading
order at the bottom.

## What this is

A Routstr node normally proxies your request to an upstream API and hands you
back the response — you have to trust the node. Verified mode replaces that
trust with a **TLSNotary proof**: the client runs a verifier that participates
in (but cannot decrypt) the node's TLS session with the upstream, and
afterwards verifies that the response bytes it received are byte-identical to
what the upstream actually sent, over a real TLS connection to the committed
host, with the node's credential headers redacted.

**What it proves**

- The response came from a real TLS session with the allowlisted upstream host.
- The request bytes on the wire match the request the client made (method,
  path, messages, tools, model) — modulo a documented request path / forward
  model id mapping.
- Response bytes received by the client equal the proven response bytes.
- The node's API key is redacted from the disclosure (its presence is bound by
  the transcript commitment; its value is hidden).

**What it does not prove / is out of scope**

- **Confidentiality from the node.** The node is the prover; it sees plaintext.
  The guarantee is "the node cannot lie about what the upstream returned".
- **Freshness against byte-identical replays.** The proof binds request
  *content*, not wall-clock time: a byte-identical repeated request could in
  principle be answered from a cached, previously verified session.
- **TLS 1.3.** TLS 1.2 only at the pinned tlsn revision (1.3 is disabled
  upstream at that pin).
- **Per-request session setup cost.** One TLS session per request is used
  today; keep-alive/connection reuse is future work (see Limitations).

## The stack

```
channel A — API call (HTTPS):      client (routstrd + @routstr/sdk) ⇄ routstr-core
channel B — TLSN mux (WebSocket):  verifier ⇄ proverd        (via the node's /v1/tlsn/ws proxy)
channel C — raw TCP relay:         verifier ⇄ upstream       (the verifier dials the upstream)
```

- **`proverd`** (this repo) — the node-side Proxy-TLS **prover** sidecar. It
  makes the upstream request, holds the TLS session keys, and proves the
  transcript.
- **`tlsn-verifier`** (this repo) — the **verifier** binary the client runs
  (spawned by the SDK under Bun/Node). Dials channel B and channel C, compares
  the disclosed transcript against what it saw on channel A.
- **`routstr-core`** — the node. Opt-in per request via
  `x-routstr-verify: tlsn-proxy`, forwards the request through proverd, and
  advertises TLSN support per model in `/v1/models`. Also exposes the
  channel-B proxy (`GET /v1/tlsn/ws`) because proverd binds loopback on the
  node and remote verifiers cannot reach it directly.
- **`@routstr/sdk` + `routstrd`** — the client side. `routstrd` is the daemon
  users run; it enables `verifyUpstream`, attaches verification to requests,
  and surfaces the verdict (`x-routstr-tlsn-status` header + logs).

## Where the code lives

| repo | branch | contents | state |
|---|---|---|---|
| **Routstr/proverd** (this repo) | `main` | prover sidecar, native verifier, mock upstream fixture, Dockerfile | pushed (`36d4d40`) |
| **Routstr/routstr-core** | `feat/tlsn-verified-mode` | verified-mode forwarding, `/v1/tlsn/ws` channel-B proxy, `/v1/models` tlsn advertisement | pushed (`93a08f4c`), **no PR yet** |
| **Routstr/routstr-sdk** | `feat/tlsn-verifier` | verifier rail (native + wasm backends), comparator, `routeRequests` passthrough, advertisement plumbing | origin branch exists at an older point; **5 local commits not yet pushed** |
| **Routstr/routstrd** | `feat/tlsn-verify` | daemon wiring (headers/logs/billing untouched), e2e harness, verifier build script | **local only, not pushed** |

Branch pushes / PRs for the last two are pending the maintainer's call. No PRs
have been opened for any repo.

Notable files:

| file | repo |
|---|---|
| `routstr/tlsn_ws.py` — channel-B ws proxy | routstr-core |
| `routstr/upstream/tlsn_verified.py` — verified-mode forwarding | routstr-core |
| `routstr/payment/models.py` — `/v1/models` tlsn advertisement | routstr-core |
| `client/tlsn/` — verifier backends, comparator, `TlsnVerifier` | routstr-sdk |
| `client/RoutstrClient.ts`, `routeRequests.ts` — verify plumbing | routstr-sdk |
| `src/utils/tlsn.ts`, `src/daemon/http/` — daemon verdict handling | routstrd |
| `scripts/e2e-tlsn.ts` — one-command local end-to-end harness | routstrd |
| `scripts/build-tlsn-verifier.sh` — build/install the verifier | routstrd |
| `src/testverifier.rs`, `src/bin/tlsn-verifier.rs` — verifier implementation | proverd (this repo) |

## Status (as of 2026-10-08)

Milestones M1–M5 are complete and verified: proverd + native verifier (M1),
core verified mode (M2), SDK verifier rail (M3), routstrd wiring (M4), and a
one-command local e2e harness (M5).

**Live end-to-end verified inference is working on a production node**
(`ppq.redsh1ft.com`, public TLSN node): all 387 models advertise TLSN
capability and verified requests return `x-routstr-tlsn-status:
verified:<upstream-host>`.

Measured on that node (glm-5.3-flash, `max_tokens: 16`, streaming):

| metric | value |
|---|---|
| TTFT unverified | ~1.1 s |
| TTFT verified | ~12 s (+~10.8 s) |
| channel-C TLS setup (dominant cost, geography-bound) | 7.3–15.4 s |
| proof time (post-hoc for streaming; does not affect TTFT) | 3.3–5.9 s small requests; 66 s for a 230 KB transcript |
| cost | identical to unverified (14 msats for the test) |

The verified-mode premium is dominated by the interactive MPC handshake across
verifier↔prover↔upstream distance, not by compute — expect much less with the
verifier closer to the upstream. **Full breakdown, the security analysis of
connection reuse, and the ranked list of security-neutral optimizations:
[`docs/LATENCY.md`](LATENCY.md).**

Open items (see `docs/STATUS.md` → "Open items for architect"): ratify protocol
additions (advertisement fields, comparator tolerances, SSE status semantics),
push/PR the SDK and routstrd branches, decision on connection reuse vs
latency ([`docs/LATENCY.md`](LATENCY.md) §3–4), and usage-record verdict
persistence.

## Running it

**Build everything** (proverd, verifier, fixture upstream):

```sh
cargo build --release --bins --examples
cargo test --release          # includes a proverd + mock-upstream e2e
```

**Docker** (used by nodes; see `Dockerfile`):

```sh
docker build -t proverd . && docker run -p 7047:7047 proverd
```

**Full local e2e harness** (the fastest way to see the whole thing work): in a
`routstrd` checkout on `feat/tlsn-verify`,

```sh
bun scripts/e2e-tlsn.ts        # add --keep to inspect the run dir
```

Brings up a Cashu mint (podman), mock TLS upstream, proverd, routstr-core in
verified mode, and routstrd — then asserts verified non-streaming, verified
streaming, and a tamper→mismatch adversarial case. Requires podman and the
proverd release binaries.

**Deploy on a node** — proverd as a sidecar via an untracked compose override:

```yaml
# compose.override.yml next to the node's compose.yml
services:
  proverd:
    build: /path/to/proverd
    environment:
      - PROVERD_BIND=0.0.0.0:7047
    restart: unless-stopped
    stop_grace_period: 3s
  routstr:
    depends_on: [proverd]
    environment:
      - TLSN_PROVERD_URL=http://proverd:7047
```

Node env that matters: `HTTP_URL` (the node's public URL — required so
`/v1/models` advertises the node's own `/v1/tlsn/ws` proxy instead of
proverd's internal address) and `TLSN_PROVERD_URL` (enables verified mode at
all). Any reverse proxy in front must pass WebSocket upgrades and have long
timeouts for that route:

```nginx
location /v1/tlsn/ws {
    proxy_pass http://127.0.0.1:8000;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_read_timeout 86400;
}
```

**Install the verifier on a client** (routstrd):

```sh
bun run scripts/build-tlsn-verifier.sh   # clones proverd at the pinned rev → ~/.routstrd/bin/
```

then set `verifyUpstream: true` in the daemon config (`tlsn.verifierBin` /
`tlsn.upstreamHostAllowlist` optional). The daemon fails loud at startup if
the binary cannot be resolved rather than silently serving unverified.

## Limitations and ops gotchas

- **TLS 1.2 only** at the pinned tlsn revision.
- **One TLS session per request.** Connection reuse (which would remove the
  ~10 s setup for subsequent requests) is future work in the protocol/this
  sidecar and has security-design implications — see the connection-reuse
  discussion in `docs/STATUS.md`. `Connection: close` is used upstream today.
- **Verification is post-hoc**: delivery is never blocked. Streaming responses
  get `x-routstr-tlsn-status: pending` and the verdict is logged after
  `[DONE]`; non-streaming responses carry the final verdict in the header.
- **The node sees plaintext** (it is the prover) — see "What it does not prove".
- **Mux endpoints are unauthenticated**; unknown session ids are dropped by
  proverd's 30 s pairing timeout. Assumes a localhost/host-network sidecar
  behind the node's own proxy.
- **Verifier lifetime**: `tlsn-verifier` has a watchdog (default 600 s,
  `TLSN_VERIFIER_WATCHDOG_SECS`). Callers must also cancel handles on failure
  paths — a leak here previously produced multi-GB orphaned processes.
- **The Nostr review sync fails closed**: a node without a positive kind-38425
  review is disabled in `routstrd`, including local/private nodes. Re-enable
  with `routstrd providers enable <idx>`.
- **Harness mint**: use `cashubtc/nutshell:0.20.3` or newer (older images lack
  the `active` field the cashu wallet requires), `MINT_RATE_LIMIT=False`,
  `MINT_INPUT_FEE_PPK=0`.
- **`routstrd` spawn paths**: the CLI can spawn a *stale globally-installed*
  daemon instead of your checkout — verify with
  `ss -tlnp | grep 8008` and check the process argv.
- **Cached model advertisements**: the SDK caches `/v1/models` in its sqlite
  store; clear `sdk_storage` rows for a provider when the listing schema
  changes.

## Reading order

1. **This file** — orientation.
2. `docs/BRIEF.md` — original goals and framing.
3. `docs/TLSN-routstr.md` — **the design doc**: trust model, proxy-TLS vs
   MPC-TLS, architecture, request flow, the API-key question, latency
   analysis, per-repo changes, security considerations.
4. `docs/STATUS.md` — **the full history**: every milestone report, architect
   decisions, fixes, measurements, ops gotchas, and current open items. Long,
   append-only, newest at the bottom.
5. `docs/LATENCY.md` — measurements and optimization paths: where the ~11 s
   premium comes from, why reuse across requests does not happen today, what
   connection reuse would cost in security terms, and what to try instead.
6. `docs/HANDOFF-M4M5.md` — what M1–M5 built and where.
7. `docs/TASK-CHANNEL-B-VPS.md` — the channel-B proxy + VPS deployment task
   spec (historical).
8. The code, starting from this repo's `README.md` (proverd protocol surface)
   and `src/testverifier.rs`.
