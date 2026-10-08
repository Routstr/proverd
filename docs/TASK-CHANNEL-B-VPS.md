# Task: Channel-B proxy + VPS live deployment (TLSN first live test)

Self-contained task spec. Context files if you need them:
`TLSN-routstr.md` (design), `STATUS.md` (full history, read the tail),
`HANDOFF-M4M5.md` (what M1–M5 built and where things live).

User sanction: push branches to origin, deploy to the VPS over SSH
(`ssh <your-node-host>`, non-interactive works), restart the live node,
and run real (paid) verification tests from a local routstrd. Spend small:
`max_tokens` ≤ 32, a handful of requests total.

## Phase 1 — channel-B proxy in routstr-core

Work in `/home/user/projects/routstr_main/routstr-core/.worktrees/tlsn-verified-mode`
(branch `feat/tlsn-verified-mode`).

Problem: proverd binds 127.0.0.1 on the node; a remote verifier (the SDK in
routstrd) cannot reach it. routstr-core must proxy channel B.

1. `GET /v1/tlsn/ws?session_id=<uuid>` — WebSocket endpoint in routstr-core
   that bidirectionally proxies bytes to proverd's `GET /ws?session_id=<uuid>`
   (proverd URL from `TLSN_PROVERD_URL`). Binary frames, passthrough, close
   semantics propagate both ways. Only registered when `TLSN_PROVERD_URL` is
   set.
2. Model listing: advertise `proverd_ws` as the NODE's own public
   `/v1/tlsn/ws` URL (derived from the node's public URL setting), not
   proverd's direct address. Keep the absolute URL form the SDK already
   expects (check the SDK's `client/tlsn` for how it parses `proverd_ws`).
3. Tests: unit test the proxy against a mock proverd ws; keep the existing
   TLSN live tests green.
4. DoS note (document, don't fix): unauthenticated ws connects with unknown
   session ids die at proverd's 30s pairing timeout. Fine for now.

Push the branch to origin when green (`feat/tlsn-verified-mode`).

## Phase 2 — VPS deployment

Remote: `ssh <your-node-host>` (BatchMode works). Facts I gathered:

- Node: `/home/debian/projects/routstr-core`, git clean on `main`, origin =
  `routstr/routstr-core`. Runs via **docker compose** (`routstr-core-routstr-1`
  on :8000, tor container, ui). `.env` present (ppq.ai key with balance —
  NEVER print its contents). Node identity: "PPQ Mirror", v0.4.7.
- Reverse proxy on :80/:443 fronts `ppq.redsh1ft.com` → :8000.
- No cargo/rustc/uv on the host. Same arch as us (x86_64).

Steps:

1. **proverd image**: add a multi-stage `Dockerfile` to the proverd repo
   (rust:1.95 builder → debian-slim runtime, `cargo build --release`,
   expose `PROVERD_BIND=0.0.0.0:7047`), push to `Routstr/proverd`.
2. On the VPS: `git fetch` + checkout `feat/tlsn-verified-mode` in
   `/home/debian/projects/routstr-core`. Do NOT clobber anything — the tree
   is clean, so this should be a plain branch checkout.
3. Add proverd to the stack via **`compose.override.yml`** (untracked, survives
   pulls — do NOT edit tracked `compose.yml`): service `proverd` built from a
   local clone of `Routstr/proverd` (`/home/debian/projects/proverd`),
   `PROVERD_BIND=0.0.0.0:7047`, on the same compose network, restart policy.
4. Core env: `TLSN_PROVERD_URL=http://proverd:7047` (compose network). Check
   how the M2 code reads it (settings.py) and whether .env or compose env is
   the right place — prefer compose.override env, don't touch tracked files.
5. **Reverse proxy**: find what serves 443 (nginx/caddy) and make sure the
   `/v1/tlsn/ws` location passes WebSocket upgrades (`Upgrade`/`Connection`
   headers) AND has long read/write timeouts (a ws lives for the whole
   inference + proof — kill any 60s default for that route). Reload the proxy
   carefully.
6. Rebuild + restart the stack (`docker compose up -d --build`). Brief
   downtime is sanctioned. Verify `/v1/info` answers and the model listing
   now shows `tlsn` capability with `proverd_ws` pointing at
   `https://ppq.redsh1ft.com/v1/tlsn/ws`.
7. Rollback note: if anything breaks, checkout `main` + `up -d --build`
   restores the old node. Leave the override file documented.

## Phase 3 — live verification test (local routstrd → VPS node)

From the routstrd worktree
(`/home/user/projects/routstr_main/routstrd/.worktrees/tlsn-verify`):

1. Daemon with `verifyUpstream` enabled, provider pinned to
   `https://ppq.redsh1ft.com`, host policy allowing the advertised
   `upstream_hosts` (the ppq.ai API host — channel C dials it from HERE).
2. One **non-streaming** chat completion, `max_tokens` ≤ 32. Assert:
   HTTP 200, `x-routstr-tlsn-status: verified:<real upstream host>`,
   daemon log `verified ✓`.
3. One **streaming** request. Assert: `pending` header, SSE `[DONE]`,
   post-stream log `verified ✓`.
4. One **unverified** request (verifyUpstream off) for TTFT comparison.
   Record both TTFTs and the premium in STATUS.md (compare with the design
   doc's latency table — expect a larger constant because the VPS and the
   upstream are remote from us; note geography).
5. Watch the node's proverd logs over ssh during one request to confirm the
   session pairs (`session_id` match POST↔ws).
6. If the ppq.ai upstream turns out to be TLS-1.3-only or the proof fails:
   capture the exact error, do NOT retry-storm (each attempt costs money).
   Report and stop.

## Rules

- Everything gets recorded in `provable-ai/STATUS.md` (new section: "VPS live
  test"). Include TTFT numbers and the final verified headers.
- Never print secrets: remote `.env`, wallet tokens, ppq key. Redact in logs.
- Push branches when green: `feat/tlsn-verified-mode` (core), proverd
  Dockerfile commit. Do not open PRs.
- If the remote node ends up broken and you can't restore it, STOP and report
  — don't leave it down.
