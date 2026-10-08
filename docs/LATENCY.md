# Latency: measurements and optimization paths

Everything we know about the verified-mode latency premium, measured on a real
node, plus the analysis of how to reduce it. Written after the first live
verified inference (2026-10-08, `ppq.redsh1ft.com`).

If you only read one thing: **the premium is ~11 s per request, it is paid
per request (nothing is reused between requests today), and the dominant term
is the interactive MPC-TLS handshake across verifier↔prover↔upstream
distance — not compute.** Roughly 7–15 s of a ~12 s TTFT is channel-C setup.

---

## 1. Measurements

### 1.1 Headline A/B (streaming, same model, same prompt, ~2 min apart)

Model `glm-5.3-flash`, `max_tokens: 16`, stream, through a local routstrd
(`verifyUpstream` on/off), cost 14 msats either way — **verification does not
change the price**.

| metric | unverified | verified (TLSN) | delta |
|---|---|---|---|
| **client TTFT** (`time_starttransfer`) | **1.140 s** | **11.967 s** | **+10.83 s** |
| node-reported `x-routstr-duration-ms` | 0.700 s | 11.520 s | +10.82 s |
| verdict header | — | `pending` → post-stream `verified:<host>` | |

Non-streaming verified, for contrast: node-side 14.083 s, **client wall clock
18.67 s** — the extra ~4.6 s is the client waiting for the proof and billing
settlement. This is why **streaming is the mode to prefer**: for SSE the proof
is post-hoc and does not touch TTFT; for non-streaming it is additive.

### 1.2 All verified sessions observed, in chronological order

proverd's own `SessionTimings` (ms). This is the raw material for the rest of
the document.

| time (UTC) | ws_wait | tls_setup | ttfb | transfer | proof | total | note |
|---|---|---|---|---|---|---|---|
| 09:31:43 | 659 | 7 548 | 2 886 | 0 | 4 009 | 15 803 | |
| 09:34:33 | 0 | 15 371 | 5 218 | 3 061 | 66 289 | 91 027 | 230 KB conversation |
| 09:47:22 | 2 | 10 268 | 2 630 | 0 | 3 656 | 17 153 | |
| 09:58:44 | 0 | 8 725 | 3 110 | 0 | 5 924 | 18 432 | |
| 10:02:31 | 0 | 12 488 | 4 537 | 0 | 3 734 | 21 356 | |
| 10:12:41 | 900 | 9 015 | 3 190 | 0 | 3 903 | 17 617 | verified, non-streaming |
| 10:13:11 | 636 | 7 262 | 2 927 | 99 | 3 295 | 14 835 | verified, streaming |

Phase meanings (proverd's instrumented phases):

- **`ws_wait`** — pairing: the verifier's channel-B websocket joins the
  session proverd created for the request. Sub-second.
- **`tls_setup`** — **the dominant term**: the interactive MPC-TLS handshake
  with the upstream (channel C), relayed through the prover.
- **`ttfb`** — upstream first response byte, through the relay.
- **`transfer`** — remaining body bytes.
- **`proof`** — the MPC proof. Post-hoc for streaming (does not affect TTFT);
  additive for non-streaming.
- **`total`** — whole session including proof.

### 1.3 Proof time scales with transcript size

| request size | proof time |
|---|---|
| 486 B (short chat) | 3.3–5.9 s |
| 230 KB (long conversation) | **66.3 s** |

`transfer` also grows (3.1 s on the 230 KB session vs ~0). For streaming this
costs CPU/latency *after* the client is done; for non-streaming it is on the
critical path.

### 1.4 Method and caveats

- TTFT measured with `curl -w '%{time_starttransfer}'` against the local
  routstrd; `x-routstr-duration-ms` is the node's own server-side measurement;
  `SessionTimings` come from proverd logs on the node.
- **n = 1 per arm** for the headline A/B. It is a same-minute, same-model,
  same-prompt pair, but not a repeated measurement.
- Upstream TTFB varied 2.6–5.2 s across sessions, while the unverified run
  showed only 0.70 s of node time — so a clean
  `verified = unverified + setup` decomposition has slack, and
  `x-routstr-duration-ms` for a stream may not cover the same window as the
  client's TTFT.
- `tls_setup` is noisy (7.3–15.4 s, ±3 s run to run) and includes the
  upstream's own handshake through the relay, not only MPC work.
- To firm it up: 5 interleaved verified + 5 unverified streaming requests,
  same prompt, report mean/median/min/max.

### 1.5 Why request #2 is *not* faster

Three sessions 11 minutes apart, two of them 30 s apart:
`tls_setup` = 7.5, 10.3, 8.7, 12.5, 9.0, 7.3 s (excluding the 230 KB
outlier). No downward trend; the 30-second pair differed by 1.8 s, inside the
±3 s noise. Proof times likewise show no warm-up.

**Mechanism.** Every verified request is a fresh session on every layer: a
fresh `tlsn-verifier` process, a fresh channel-B websocket with a new
`session_id`, a **fresh TLS session to the upstream**, and a fresh MPC key
agreement. A proof is bound to one TLS session — the prover commits to that
session's key material — so nothing carries over by construction.

What *is* reused: the daemon's channel-A HTTP keep-alive to the node, model
cache, wallet tokens, provider selection. That is the cheap part, which is why
the unverified baseline is ~0.7–1.1 s.

---

## 2. Where the time actually goes

tlsn's own documentation (`crates/tlsn/src/lib.rs` in the pinned revision) is
explicit about the shape of this cost:

> The MPC-TLS protocol is highly interactive: prover and verifier exchange many
> small messages, and each one is on the critical path.

With our topology, every one of those messages travels:

```
verifier (client machine)
  → TLS/nginx (node, :443)
  → routstr-core  GET /v1/tlsn/ws  (WebSocket proxy, Python)
  → proverd       GET /ws          (axum WebSocket)
  → and back
```

So `tls_setup` is plausibly **message count × effective per-message latency
across those hops**, not cryptographic computation. The same doc advises
disabling Nagle (`TCP_NODELAY`) on the three TCP sockets handed to the crate,
and then adds the key caveat for us:

> This setting has no effect on non-TCP transports (e.g. **WebSocket relays**
> or in-memory duplex streams).

Channel B *is* a WebSocket relay, and it is multi-hop. **We do not currently
know the message count or round-trip breakdown** — proverd logs phase totals
only. That gap is why the first recommended step below is instrumentation, not
an optimization.

---

## 3. Connection reuse: the big lever, and its security cost

The obvious idea: keep one TLS connection to the upstream open and serve many
requests over it, paying the handshake once. It is the one change that could
take verified TTFT from ~12 s to near the unverified baseline. It is **not** a
free optimization.

### 3.1 What does NOT change

The cryptographic commitment is what secures verified mode, and it does not
care how many bytes it covers:

- the TLS session really was with the allowlisted host (server name is
  committed);
- request/response bytes are exactly what was exchanged (byte-equality with
  channel A);
- credential headers stay redacted — the verifier never learns the API key;
- only the prover holds keys; the verifier is blind to them.

Committing to more bytes over a longer-lived connection is **not weaker**.

### 3.2 What DOES change: the unit of trust

Today the unit is **one request = one fresh session**, which gives two
properties *for free*:

1. **Completeness** — the whole transcript belongs to this request; there is
   nothing to slice, so nothing can be hidden outside the proven window.
2. **Attribution/freshness** — the proof is bound to the request just sent, at
   that moment.

With reuse the unit becomes **one connection = one committed byte stream, many
disclosures**, and the verifier must then enforce:

| must hold | if it does not |
|---|---|
| disclosed range contains *my* request bytes | prover shows a window from a different exchange |
| ranges are contiguous; no undisclosed bytes between request and response | prover smuggles or modifies headers outside the window |
| response range starts *after* the request range, in order | request/response pairing is hidden |
| per-request verifier randomness still inside the exchange | a valid **old window** is passed off as the answer to a **new** request — replay hole, the classic failure mode of amortized proofs |

None of these are impossible — proving over long-lived connections is a known
direction in the tlsn space — but they are new trust-critical logic, whereas
today they are structural. **Same cryptographic strength, new attack surface
that must be designed and adversarially reviewed.**

### 3.3 Hard blocker in the pinned tlsn revision

We build against `tlsnotary/tlsn` rev
`4415391333bd67bd2b3f62082f0b6c3d0261aa05`. In `crates/tlsn/src/prover.rs`
the prover's connection future completes **only when the server closes the
connection** (`if *state.server_closed && state.output.is_some() { … Ready }`),
i.e. **the commitment is terminal — one commitment per connection.**

The API *does* support multiple disclosures over one committed transcript
(`Prover::prove(&mut self)` is repeatable; on the verifier side `accept()`
returns `(VerifierOutput, Verifier<state::Committed>)`), so "one commitment,
many proofs" is expressible. But "one connection, one proof **per request**,
with a fresh commitment each time" is not.

Consequence for reuse today: hold the connection across N requests, commit
once at the end, disclose per request — which means **deferred/batched
verdicts**, and our per-request `pending → verified` semantics would have to
change. That is a semantics change to shipped behaviour, not an optimization.

### 3.4 Verdict

Connection reuse is a **protocol change**: it needs a design for window
containment, contiguity, ordering and per-request freshness, plus explicit
adversarial review of the slicing logic. Deferred until either (a) the
security-neutral work below is exhausted, or (b) deferred/batched verdicts
become acceptable for a use case.

---

## 4. Security-neutral levers (do these first)

Ranked by expected payoff per unit of risk. None of them changes the trust
model.

### 4.1 Instrument the session — *start here*

We are optimizing blind. Add to proverd/verifier:

- count of mux messages exchanged during setup and during transfer;
- time spent in-flight (waiting on the channel) vs. local compute;
- round-trip timing histogram.

This tells us whether `tls_setup` is 200 round trips or 20 000, and therefore
whether the levers below are worth anything. **No API, protocol or security
change.**

### 4.2 `TCP_NODELAY` on the sockets *under* every hop

We set `set_nodelay(true)` on the channel-C socket only
(`src/testverifier.rs`, the connection to the TLS server). The tlsn warning's
caveat is about the WebSocket *transport*, not the TCP sockets underneath it:

- nginx → routstr-core upstream (keepalive/`proxy_http_version 1.1`);
- routstr-core's WebSocket client → proverd;
- proverd's listener socket.

Cheap, security-neutral. Unclear magnitude until §4.1 tells us the message
count.

### 4.3 Coalesce / batch MPC messages in the WebSocket bridge

The channel-B bridge is currently a naive frame pump in both directions
(`routstr/tlsn_ws.py` in routstr-core; proverd's `ws_io`). One MPC message =
one WebSocket frame = one (or more) TCP writes. Small-message chatter across a
multi-hop relay is exactly the pattern the tlsn docs warn about. Coalescing
within a short window (or using a batched framing mode) may collapse much of
the per-message cost — security-neutral, since the bytes are unchanged.

### 4.4 Fewer hops

Every hop is on the critical path. A direct verifier↔proverd path (no nginx,
no Python proxy) removes two of them. Trade-offs to settle first: proverd must
then be reachable publicly, and the accepted DoS note in
`routstr/tlsn_ws.py` (unauthenticated connects with unknown session ids occupy
a pairing slot until proverd's timeout) needs a better answer than "the
pairing timeout reaps them".

### 4.5 Not ours to fix: verifier↔upstream distance

The trust model puts the verifier **at the client**, so the verifier↔upstream
RTT is a user-facing property, not a deployment choice of the node. What we can
do is *document* it: users geographically near the upstream they verify will
see a far smaller premium. Expect the constant to shrink substantially for a
co-located verifier; how much exactly is worth measuring.

---

## 5. Suggested order of work

1. **Instrument** (§4.1) and re-measure — establish the baseline breakdown.
2. **NODELAY + coalescing** (§4.2, §4.3) — measure again; these may recover a
   large fraction with zero security impact.
3. Re-run a proper A/B benchmark (5+5 interleaved requests) to quantify.
4. Only then evaluate **connection reuse** (§3) against its design and review
   cost — and consider whether a protocol change upstream (incremental
   commitment over a live connection) is the right place for it.
5. Optionally revisit **TLS session resumption** (PSK/tickets): saves ~1 RTT,
   but the MPC handshake still has to run, so the payoff looks small relative
   to §4 — worth re-checking after §4.1.

---

## 6. References

- Raw numbers and surrounding context: `docs/STATUS.md` ("VPS live test").
- tlsn API and the interactivity/NODELAY warning: pinned revision
  `4415391333bd67bd2b3f62082f0b6c3d0261aa05`, `crates/tlsn/src/lib.rs`,
  `crates/tlsn/src/prover.rs`, `crates/tlsn/src/verifier.rs`.
- Session phase instrumentation: `proverd` (session timings logged on
  completion) and `src/testverifier.rs`.
- Channel-B topology and the WebSocket proxy:
  `routstr-core:routstr/tlsn_ws.py`.
