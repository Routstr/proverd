# TLSN Verification for Routstr: Proving Upstream Inference to the SDK

> Design doc: how inference served through `routstr-core` gets cryptographically
> proven to `routstr-sdk` (and therefore to `routstrd`) using TLSNotary.
>
> Builds on `zkTLS-research.md` (ecosystem survey) and `TLSN.md` (generic
> verifier-as-notary design). This document maps those ideas onto the actual
> Routstr codebase.
>
> **Revision 2** — incorporates the verifier-as-notary architecture (no
> third-party notary) and the new Proxy-TLS protocol mode in the vendored
> tlsn `v0.1.0-alpha.16-pre`.

## Table of Contents

1. [Problem Statement](#problem-statement)
2. [What the Code Shows](#what-the-code-shows)
3. [Roles and Trust Model](#roles-and-trust-model)
4. [Protocol Mode: Proxy-TLS vs MPC-TLS](#protocol-mode-proxy-tls-vs-mpc-tls)
5. [Architecture](#architecture)
6. [Request/Response Flow](#requestresponse-flow)
7. [The API Key Question](#the-api-key-question)
8. [Latency Analysis](#latency-analysis)
9. [Design Decisions](#design-decisions)
10. [Component Changes per Repo](#component-changes-per-repo)
11. [Security Considerations](#security-considerations)
12. [Phasing](#phasing)
13. [Open Questions](#open-questions)

---

## Problem Statement

A Routstr client (via `routstrd` + `@routstr/sdk`) pays a routstr-core node for
inference. The node claims it forwarded the request to some upstream provider
(OpenAI, Fireworks, Groq, …) and relayed the response. Today the client has no
way to check that claim:

- the node could have served a cheaper local model,
- could have edited, truncated, or fabricated the response,
- or could have proxied to a different (cheaper) upstream than advertised.

Goal:

> Whatever routstr-core claims as the upstream inference provider, the SDK can
> verify — via a TLSN proof — that the request that went in and the response
> that came out are exactly what the claimed upstream server sent, in a real
> TLS session with that server, unaltered by the node.

Non-goal: proving the upstream ran the claimed model weights. That is a
different problem (TEE/verifiable-inference territory, already covered for
`tinfoil-` models by the EHBP/Tinfoil rail).

---

## What the Code Shows

Findings from reading `routstr-core`, `routstr-sdk`, and `routstrd`.

### 1. The TLS topology decides the roles

The chain is:

```text
client → routstrd (Bun daemon) → @routstr/sdk ── TLS session A ──▶ routstr-core (FastAPI)
                                                                       │
                                                              TLS session B
                                                                       ▼
                                                              upstream AI API
                                                              (api.openai.com, …)
```

`RoutstrClient._makeRequest` (`routstr-sdk/client/RoutstrClient.ts` ~line 882)
does `fetch(${baseUrl}${path})` where `baseUrl` is the **routstr-core node**,
not the upstream. Only routstr-core participates in TLS session B.

Consequences:

- **routstr-core must be the TLSN Prover.** It owns session B and holds the
  upstream API key.
- **The SDK is the TLSN Verifier** — and, per tlsn terminology ("a Notary is
  classified as a Verifier", `crates/tlsn/src/lib.rs`), the verifier is the
  notary. No third party is required anywhere in the design.

### 2. routstr-core currently mutates response bodies

`routstr-core/routstr/upstream/base.py`:

- `_inject_cost_into_usage` writes `usage.cost` into non-streaming response
  JSON and into streaming usage chunks (and deliberately overwrites any
  upstream-provided cost values),
- the streaming path sometimes swallows pure-usage upstream chunks,
- request side: `request_correction.py` rewrites request bodies,
  `model_paths.py` maps model IDs, params get renamed (e.g. `max_tokens` →
  `max_completion_tokens` for reasoning models).

So "bytes the SDK received" ≠ "bytes the upstream sent" in the current mode.
**A verified mode needs pass-through bodies and near-passthrough requests**,
or the SDK's comparison against the disclosed transcript will always fail.

### 3. The SDK already has a verification rail pattern

`routstr-sdk/client/TinfoilSecure.ts` provides EHBP transport encryption +
enclave attestation for `tinfoil-` models: it attests *before* spending tokens
and hooks into `RoutstrClient` (~line 613). This is the exact shape the TLSN
rail should mirror — a second, complementary verification rail.

Also useful: `RoutstrClient` already tees the response Web stream (~line 739)
for usage tracking. The TLSN verifier hangs off the same tee to buffer a copy
for comparison against the proven transcript.

### 4. The vendored tlsn is built for this (alpha.16-pre)

The vendored `tlsn/` workspace (now `v0.1.0-alpha.16-pre`) provides everything
the design needs, none of which existed when research started:

- **`crates/wasm` ships a `Verifier`** (`wasm_bindgen`) that connects
  **outbound to the prover** over a caller-supplied `IoChannel`
  (`JsVerifier.connect(prover_io)` — the JS side wraps its own WebSocket in
  the `JsIo` interface: `read/write/close/unread`). The prover is the
  listener; the verifier dials out — NAT-friendly by construction. Full wasm
  verifier flow: `connect(prover_io)` → `setup()` (returns the server name in
  proxy mode) → `set_server_socket(server_io)` → `run()` → `verify()`.
- **Proxy-TLS mode** (`TlsCommitConfig::Proxy`, #1122): the verifier makes the
  connection to the TLS server itself and relays ciphertext between server and
  prover. Wired into wasm (`set_server_socket()` + `verify()`); native
  reference flow in `tlsn/crates/examples/proxy/proxy.rs`.
- **Config note**: `ProverConfig` is currently empty in this revision —
  transcript sizing lives in `SessionConfig`/`TlsCommitConfig`; a hard
  `max_recv_bytes` cap must be enforced by the prover wrapper (proverd aborts
  the upstream read past the limit).
- **`crates/sdk-core`**: a platform-agnostic SDK built around an `Io` trait
  with platform-injected byte streams (WASM/JS, FFI, native) and
  `NetworkSetting::Latency` tuning — the intended embedding surface for
  `routstr-sdk`.
- **Range-selective reveal** (`ProveConfig.reveal: Option<(RangeSet,
  RangeSet)>`): the prover discloses only chosen transcript byte ranges;
  hidden ranges stay bound by commitments.
- Supporting improvements: `PartialTranscript` now compressed (smaller
  presentations), GHASH preprocessing sized to configured record length
  (#1174), configurable mux stream limits (#1193), session IO reclaim (#1178),
  custom root certs in wasm (#1119), Noir ZK-presentation examples
  (`crates/examples-zk`).
- **TLS 1.2 only for now**: TLS 1.3 exists in `tls-core` but is commented out
  of `ALL_VERSIONS`. Verified sessions pay 2-RTT handshakes until that lands.

---

## Roles and Trust Model

| Role | Who | Notes |
|---|---|---|
| **Prover** | routstr-core node, via a Rust sidecar (`proverd`) built from the vendored `tlsn/` workspace | Owns the upstream TLS session and API key; authors the upstream request |
| **Server** | upstream AI API | Unchanged, unaware of the protocol |
| **Verifier = Notary** | `@routstr/sdk` (inside routstrd, or any app using the SDK), running the tlsn wasm `Verifier` | Dials the prover (mux) **and** the upstream (relay); signs the attestation with the user's ephemeral key |

### Why verifier-as-notary works here (and requires no inbound connectivity)

Earlier drafts assumed a hosted third-party notary because "the prover must
reach the notary interactively." The vendored tlsn inverts that: **the
verifier dials the prover**, over a WebSocket, in the same direction as the
API request itself. routstrd needs zero inbound reachability.

Trust properties:

- **Node+notary collusion is meaningless for self-verification** — the user
  *is* the notary. The node's only remaining cheats are breaking the proof
  system (malicious-secure) or denial of service (fails loud).
- **Endpoint binding is cryptographic in MPC mode and absolute in proxy
  mode**: in proxy mode the verifier itself dials `api.openai.com:443`; the
  node has no network path to any upstream except through the user's relay.
- **Freshness is intrinsic**: the user witnesses the session live and signs
  with their own key on their own clock. No nonce echo, no `Date`-header
  windows, no replay database.
- **Proofs are self-verification artifacts.** An attestation signed by the
  user's ephemeral key convinces that user, and nobody else. Public
  reputation/slashing would need a separate transferable-proof mechanism
  (out of scope for v1; see Open Questions).

---

## Protocol Mode: Proxy-TLS vs MPC-TLS

tlsn alpha.16 offers two commitment protocols (`TlsCommitConfig::{Mpc,
Proxy}`). Both are available in the wasm verifier.

| | **MPC-TLS** | **Proxy-TLS** (new, #1122) |
|---|---|---|
| Who connects to upstream | Prover (node) | **Verifier (routstrd)**, prover relays through it |
| Session keys | Jointly computed (neither party has them) | Prover knows master secret (`MsVisibility::Private`), verifier `Blind` |
| Soundness comes from | Key-splitting: prover can't fabricate a session | Wire-observation: verifier witnessed the actual ciphertext to the server it dialed + post-hoc ZK proof of decryption (`VerifierZk` VM) |
| Pre-request critical path | Garbled-circuit key schedule: **seconds, MBs** — blocks TTFT | **None** — normal TLS handshake (relayed), ZK work is all post-session |
| Upstream sees | Node's IP | **User's IP** |
| Endpoint binding | Server cert verified inside MPC | **Verifier dialed the host itself** |
| Bandwidth through user | Protocol traffic only (MBs of GC) | ~2× payload (relay + mux/API) + smaller protocol traffic |
| Browser entrypoint | Works (ws to prover) | Needs a ws→TCP proxy for the server socket |

**Recommendation: Proxy-TLS is the primary mode.** It eliminates the
critical-path MPC (the main UX risk), makes endpoint binding absolute, and
matches the "user verifies their own inference" trust model. MPC mode remains
as a fallback for deployments where the user cannot or must not connect to
the upstream directly (see the IP-privacy trade-off below).

The rest of this document assumes Proxy-TLS unless noted.

---

## Architecture

Three logical channels:

```text
Channel A — API call (normal HTTPS):   routstrd ⇄ routstr-core
Channel B — TLSN mux (WebSocket):      routstrd ⇄ routstr-core   (verifier⇄prover protocol)
Channel C — raw TCP relay:             routstrd ⇄ upstream       (verifier dialed this)
```

```text
┌────────────────────────────┐                ┌─────────────────────────┐
│ routstrd / @routstr/sdk    │                │ routstr-core            │
│                            │  (A) request   │                         │
│  RoutstrClient ────────────┼───────────────▶│  proxy.py               │
│                            │◀───────────────┼─ (plaintext response)   │
│  TlsnVerifier              │  (A) response  │                         │
│   (tlsn wasm Verifier)     │  (B) mux/ws    │  proverd (Rust, tlsn)   │
│        │  ◀────────────────┼────────────────┼─ (ws listener)          │
│        │ relay (C)         │   MPC/ZK +     │        │                │
│        ▼                   │   proxied TLS  │        │ TLS authored   │
│  api.openai.com:443 ◀──────┼────────────────┼────────┘ here, with     │
│  (dialed by routstrd)      │   ciphertext   │        node's API key   │
└────────────────────────────┘                └─────────────────────────┘
```

### proverd (new component, routstr-core sidecar)

routstr-core is Python; TLSNotary is Rust; there are no maintained Python
bindings. The prover runs as a small Rust sidecar built from the vendored
`tlsn/` workspace:

- exposes a **WebSocket listener** for verifier mux connections (paired to API
  requests by session id);
- **in proxy mode needs no upstream connectivity at all** — every upstream
  byte arrives/leaves through the user's relay;
- authors the TLS session and HTTP request (holds the upstream API key in
  memory only, passed per-request by routstr-core, never persisted);
- streams decrypted upstream response bytes back to routstr-core in real time
  (no added per-token latency beyond relay hops);
- after stream end, generates the ZK proof over the disclosed ranges
  (redacting `authorization`) and sends it to the verifier over the mux.

### TlsnVerifier (new component, SDK side)

`routstr-sdk/client/TlsnVerifier.ts`, mirroring the Tinfoil rail, wrapping the
tlsn wasm `Verifier` / `sdk-core` with Bun-injected `Io` streams:

- dials channel B (its own WebSocket to the node, wrapped in the tlsn
  `IoChannel` interface) and channel C (raw TCP to the upstream via
  `Bun.connect`, or a configured Tor/ws proxy);
- enforces policy: which upstream hosts are acceptable for this model/request;
- runs the verifier protocol, receives the post-session proof, checks it
  against the ciphertext it relayed;
- compares disclosed ranges against the SDK's own request copy and the
  response body it received on channel A;
- signs/stores the attestation locally (ephemeral per-session key).

---

## Request/Response Flow

```text
1. routstrd ──(A: "chat completion, verify=tlsn")──▶ routstr-core
2. routstrd ──(B: verifier connects ws)──▶ routstr-core/proverd
   routstrd ──(C: TCP connect)──▶ api.openai.com:443     ← verifier picks/validates host
3. proverd writes TLS ClientHello + HTTP request (with ITS API key, AEAD-encrypted):
        routstr-core ──(B)──▶ routstrd ──(C)──▶ upstream   ← ciphertext relayed
4. upstream responds (SSE):
        upstream ──(C)──▶ routstrd ──(B)──▶ routstr-core   ← ciphertext relayed
5. proverd decrypts (only it has session keys), routstr-core forwards:
        routstr-core ──(A)──▶ routstrd                     ← tokens the user reads
6. after data: [DONE]:
        routstr-core ──(B: ZK proof + disclosed ranges)──▶ routstrd
   verifier checks proof against the ciphertext IT relayed on (C)
   → UpstreamVerification result
```

Notes on the shape:

- **Why the response can't shortcut through the relay**: the verifier is blind
  to the session keys, so channel-C bytes are unreadable to routstrd. Only the
  node can decrypt; plaintext must come back over channel A. The relay's job
  is to let the user *witness* the bytes, not read them.
- **Every upstream byte crosses the user's link twice** (relay + mux or API).
  Fine for chat-scale payloads (KBs to low MBs); a consideration for
  image/audio/file endpoints.
- **Verification is post-hoc for streaming**: the badge arrives after
  `data: [DONE]`. A mismatch is a fraud signal against the node — surfaced
  loudly, and stored for the user's own records.

### Verification checks (SDK comparator)

1. ZK proof verifies against the witnessed ciphertext and the session the
   verifier itself relayed;
2. server identity = the host the verifier dialed (enforced by construction in
   proxy mode — the verifier chose the destination);
3. disclosed request: method + path match the endpoint; `messages`/`tools`
   **deep-equal** to what the SDK sent on channel A; `model` matches under the
   declared mapping; `authorization` present-but-redacted;
4. disclosed response body **byte-equal** to what the SDK received on channel A
   (streaming: SSE event-sequence equality);
5. redaction check: reject presentations that open the `authorization` value
   (protects the node's key);
6. fail loud: a node that silently serves an unverified response to a
   `verify=tlsn` request is reported as `unavailable`, never passed silently.

Result type:

```ts
type UpstreamVerification =
  | { status: "verified";    upstreamHost: string; upstreamModel: string; attestationId: string }
  | { status: "mismatch";    reason: string }
  | { status: "unavailable"; reason: string };
```

---

## The API Key Question

"Doesn't the routstr node hold the API key?" — yes, and it never becomes
visible to the verifier at any stage:

1. **On the wire**: the key rides inside TLS application records, encrypted
   end-to-end between proverd and the upstream. The verifier relays AEAD
   ciphertext; it sees the ClientHello metadata (SNI — desirable, confirms the
   destination) but not application data.
2. **By construction**: the verifier is `MsVisibility::Blind` to the master
   secret. It has no more ability to read relayed bytes than any ISP on the
   path. TLS ephemeral ECDHE adds forward secrecy.
3. **In the proof**: disclosure is range-selective (`ProveConfig.reveal`). The
   `authorization` value is in a range that is never opened; hidden bytes stay
   bound by transcript commitments. The verifier actively rejects proofs that
   open it.

| | API key value | Request body | Response body | Upstream identity |
|---|---|---|---|---|
| routstr-core (node) | ✅ | ✅ | ✅ | ✅ (chose it) |
| upstream | ✅ | ✅ | ✅ | — |
| routstrd (verifier) | ❌ | ✅ disclosed ranges, proven | ✅ disclosed ranges, proven | ✅ **dialed it** |

Caveat (same as `TLSN.md`'s "important limitation", sharpened): hidden bytes
prove *presence*, not *meaning*. Nothing proves the redacted credential was
the node's own legitimate key rather than a stolen or free-tier one — the
evidence is that the upstream accepted it and returned authenticated data.

---

## Latency Analysis

Proxy-TLS removes the garbled-circuit key schedule from the critical path
(that was MPC mode's seconds-plus-MBs penalty). What remains is relay routing.

Baseline (unverified, node keep-alive to upstream):

$$T_{base} \approx RTT_{u{\leftrightarrow}n} + RTT_{n{\leftrightarrow}o} + T_{model}$$

Verified (proxy-TLS, fresh TLS 1.2 session per request = 2 relayed RTTs):

$$T_{ver} \approx \underbrace{2(RTT_{u{\leftrightarrow}n} + RTT_{u{\leftrightarrow}o})}_{\text{handshake through relay}} + \underbrace{2\,RTT_{u{\leftrightarrow}n} + RTT_{u{\leftrightarrow}o}}_{\text{request + first-token relay legs}} + T_{model}$$

With $RTT_{u↔n}$ = 50ms, $RTT_{u↔o}$ = 60ms, $RTT_{n↔o}$ = 15ms:

| $T_{model}$ | Baseline | Verified | Ratio | Delta |
|---|---|---|---|---|
| 800ms (typical chat) | ~865ms | ~1180ms | **1.36x** | +315ms |
| 2500ms (long prefill/reasoning) | ~2565ms | ~2880ms | **1.12x** | +315ms |
| 100ms (cached/tiny response) | ~165ms | ~480ms | ~2.9x | +315ms |

Conclusions:

1. **The cost is a roughly constant few-hundred-ms premium, not a multiplier.**
   It only looks like ~3x when the model responds near-instantly — where
   nobody feels it. On real 1–3s TTFT calls it's a modest percentage.
2. **Geography is the real cost driver.** The relay replaces the node's
   optimal datacenter path to the upstream with the user's residential path.
   Cross-planet RTTs (user Sydney, node Frankfurt, upstream Oregon) push the
   premium past a second.
3. **Steady-state streaming is unaffected**: SSE is a few KB/s and chunks
   pipeline; inter-token rhythm is unchanged beyond the constant offset.

Mitigations:

- **Pre-warmed relayed sessions** (kills the handshake term, the biggest
  chunk): the TLS handshake doesn't depend on request content. The SDK knows
  the likely upstream host ahead of time (node advertises it in the model
  listing) and can establish relayed sessions speculatively, keeping a small
  ready pool. Removes ~$2(RTT_{u↔n} + RTT_{u↔o})$ from the critical path.
  Needs validation that tlsn tolerates idle gaps between handshake and
  request, plus idle-timeout handling.
- **Parallel setup**: mux + relay dials overlap with cashu auth/billing and
  model mapping — latency that exists in the baseline anyway.
- **Later**: TLS 1.3 (halves the relayed handshake; currently disabled in
  tlsn `ALL_VERSIONS` — watch upstream); session reuse across requests (one
  notarized keep-alive session, per-request presentation slices).

---

## Design Decisions

### 1. Verified mode = no body mutation

Today routstr-core injects `usage.cost` into bodies and rewrites usage chunks.
In verified mode:

- cost data goes **only** in the existing `x-routstr-cost-*` headers;
- `usage.cost` injection is skipped;
- streaming chunks are forwarded untouched (no swallowing of usage chunks).

Request side: `request_correction` becomes a no-op except an allowlisted set
of param renames (e.g. `max_tokens` → `max_completion_tokens`). The SDK
comparator carries a small normalization table for these known equivalences.
Anything else breaks verification loudly, which is the point.

### 2. Verifier-as-notary, no third party

Settled by the tlsn architecture (verifier dials prover) and routstr's
decentralization goals. Rejected alternatives: hosted third-party notary
(centralization + a collusion assumption), node-run notary (worthless —
prover+notary collusion is trivial), inbound notary endpoint on routstrd
(unnecessary — the verifier dials out).

### 3. Proxy-TLS primary, MPC-TLS fallback

Proxy mode wins on TTFT and endpoint binding. Its costs (accepted, with
mitigations):

- **The upstream sees the user's IP**, not the node's — erodes routstr's
  IP-privacy property for verified requests. Mitigation: verifier dials
  channel C via Tor (routstr already has torMode); MPC mode remains for users
  who prioritize upstream-IP privacy over TTFT.
- **User bandwidth carries ~2× payload** plus protocol traffic. Fine for chat;
  revisit for image/audio/file endpoints.
- **Upstream WAF/rate-limits**: the node's API key is used from residential
  IPs. Tolerated by major providers today; monitor per-upstream.
- **Browser entrypoint**: browsers can't open raw TCP, so channel C needs a
  ws→TCP proxy for the SDK's browser build. routstrd (Bun) is unaffected.

### 4. Nested routstr upstreams

A node whose upstream is another routstr node (`upstream/routstr.py`) can only
prove the first hop — in proxy mode the verifier would dial the *next routstr
node*, not the terminal upstream. v1: such models are advertised as
non-verifiable. v2: recursive verification — each hop runs the protocol, and
the SDK verifies the chain (the middle node's proof is over a session the
next hop relayed).

### 5. Economics: opt-in, priced

Verification costs the node proverd compute and post-session ZK work; it costs
the user bandwidth and the latency premium above. Verify-on-demand
(`verify: "tlsn"` per request, or per-model policy), priced into the request
cost — routstr's Cashu billing already does per-request pricing. Sampled
verification (node notarizes a random subset for reputation) is a possible
later addition but requires transferable proofs (see Open Questions).

### 6. Transcript limits

tlsn has configurable max sent/recv sizes (`SessionConfig`, mux limits,
GHASH preprocessing now sized to record length). Verified mode caps
`max_tokens` and returns `unavailable` gracefully when a transcript would
exceed the configured limit, rather than failing mid-stream.

### 7. Streaming is post-hoc

The proof cannot exist before the stream ends. Verification never blocks
delivery: the user sees the stream in real time, the verified badge arrives
after. Mismatch = post-hoc fraud signal. This matches the rollout plan in
`TLSN.md` (capture until `data: [DONE]`).

### 8. Node advertisement

routstr-core advertises per model: `tlsn: "proxy" | "mpc" | false`, the
upstream host(s) it will request relays to, and its proverd websocket
endpoint. The SDK enforces policy (host allowlist per model) and can
pre-warm relays. Models served via nested routstr upstreams advertise
`false`.

---

## Component Changes per Repo

### provable-ai (this repo)

- `proverd/`: Rust binary crate in (or depending on) the vendored `tlsn/`
  workspace: WebSocket listener for verifier mux sessions, Proxy-TLS prover
  flow, per-request API key injection, redaction policy, post-session ZK
  proof generation.
- Keep the vendored `tlsn/` pinned and note the alpha.16 feature
  dependencies (Proxy-TLS, sdk-core, wasm verifier).

### routstr-core

- `upstream/base.py`: verified-mode branch in `BaseUpstreamProvider`
  delegating to proverd; byte-passthrough response path; skip `usage.cost`
  injection when verified mode is active.
- `upstream/request_correction.py`: no-op in verified mode except the
  allowlisted rename table.
- `proxy.py` / `core/main.py`: emit `x-routstr-verified`,
  `x-routstr-upstream-host` (advisory — the verifier enforces the real
  binding), session id for mux pairing.
- Model listing / nostr announcement: `tlsn` capability + upstream host(s) +
  proverd endpoint per model; `false` for nested upstreams.
- Note: **no attestation store/endpoint needed** — proofs go directly to the
  verifier over the mux; routstrd stores its own attestations locally.

### routstr-sdk

- `client/TlsnVerifier.ts`: tlsn wasm `Verifier` integration via `sdk-core`
  `Io` injection (Bun TCP for channel C, WebSocket for channel B); policy
  engine (upstream host allowlist); comparator (messages deep-equality,
  model-mapping normalization, byte/SSE-event equality, redaction check);
  local attestation storage with ephemeral per-session keys.
- `client/RoutstrClient.ts`: `verify: "tlsn"` option; wire the verifier off
  the existing stream tee; attach `UpstreamVerification` to the result;
  pre-warm relay pool.
- `core/types.ts`: `UpstreamVerification` type.
- Keep browser/node/bun entrypoints working (`BROWSER_SAFE_ENTRYPOINTS_REFACTOR.md`);
  browser build documents the channel-C ws→TCP proxy requirement.

### routstrd

- `utils/config.ts`: `verifyUpstream` flag + per-model policy + Tor option
  for channel C.
- `daemon/http/index.ts`: forward the verify option into `routeRequests`;
  surface verification status in responses/logs.
- CLI: show `verified ✓ api.openai.com` / `mismatch ✗` / `unavailable` next
  to the provider URL it already prints; `routstrd attestations <id>` to
  inspect/export a locally stored presentation.

---

## Security Considerations

What this proves:

> The node made a specific HTTPS request to the upstream host **the verifier
> itself dialed**, using hidden credentials, and the response the SDK received
> is byte-identical to what that server returned in the same TLS session —
> whose ciphertext the verifier personally relayed.

What it does **not** prove:

- that the upstream ran the claimed model weights (use the Tinfoil/TEE rail);
- that the upstream's output is truthful;
- that the redacted credential was the node's *own legitimate* key (hidden
  bytes prove presence, not meaning);
- anything to a **third party** — attestations are signed by the user's
  ephemeral key (self-verification only);
- anything for nested-routstr upstreams (v1 marks them non-verifiable).

Additional notes:

- **Node key safety**: TLS carries the key end-to-end; the verifier is blind
  to session keys; the proof never opens the `authorization` range; the SDK
  rejects proofs that do. Forward secrecy protects recorded relay traffic.
- **Tamper/DoS**: relay tampering fails AEAD and aborts; relay dropping
  stalls the request. Worst case is loud failure, never silent corruption.
- **Downgrade**: `verify=tlsn` against a node that silently serves unverified
  responses must surface `unavailable` loudly.
- **Replay**: not applicable in the hosted-notary sense — the verifier
  witnesses its own live session; freshness is intrinsic.
- **Upstream-IP exposure**: proxy mode reveals the user's IP to the upstream
  (see decision 3). Document it; offer Tor.

---

## Phasing

| Phase | Scope | Exit criteria |
|---|---|---|
| **P0 — spike** | proverd + SDK-side wasm verifier, one upstream (OpenAI), non-streaming; **benchmark Proxy-TLS vs MPC-TLS** (TTFT, bandwidth, wasm CPU); validate idle-gap tolerance for pre-warming | End-to-end proof produced and verified by routstrd; mode choice confirmed with numbers |
| **P1 — non-streaming e2e** | verified mode in routstr-core (passthrough, headers, capability advertisement), `TlsnVerifier` in SDK, routstrd flag + log output | `routstrd` request shows `verified ✓ api.openai.com` |
| **P2 — streaming** | SSE relay while capturing to `data: [DONE]`; post-stream proof; transcript caps; relay pre-warming pool | Verified SSE chat completions with ≤ ~1.2x TTFT (pre-warmed) |
| **P3 — hardening** | Tor channel-C option, recursive verification for nested nodes, verifiability in nostr announcements (rank verifiable providers higher), transferable proofs for public reputation, Tinfoil + TLSN combined | Privacy/trust options complete; verifiability first-class in discovery |

---

## Open Questions

1. **Pre-warming validation** — does tlsn's proxy flow tolerate an idle gap
   between the relayed handshake and the request? (Likely — it's just TCP —
   but must be confirmed in P0, along with server idle-timeout behavior.)
2. **TLS 1.3** — present in `tls-core` but disabled (`ALL_VERSIONS`). Halving
   the relayed handshake depends on upstream enabling it; track tlsn releases.
3. **Transferable proofs for public reputation** — self-signed attestations
   convince only their owner. Slashing/public audit would need a separate
   mechanism (e.g. optionally co-signed by a TEE notary for users who opt into
   third-party involvement). Deliberately out of scope for v1.
4. **Upstream WAF behavior** — node's API key used from many residential IPs;
   monitor per-provider rate-limit/abuse heuristics.
5. **Model-mapping normalization table** — per-upstream param-rename
   allowlists need a maintained home (SDK or node-advertised?).
6. **Non-chat payloads** — the ~2× user bandwidth and relay hops matter more
   for image/audio/file endpoints; decide per-endpoint eligibility.

---

*Status: design. No implementation yet.*  
*Related: `TLSN.md` (generic verifier-as-notary design), `zkTLS-research.md`
(ecosystem survey), vendored prover/verifier implementation in `tlsn/`
(`v0.1.0-alpha.16-pre`).*
