# Handoff: M4 (routstrd wiring) + M5 (e2e harness) + live VPS testing

You are the M4/M5 builder. M1–M3 are done, verified, and pushed. The architect
(design doc author) steers via `provable-ai/STATUS.md`.

## Read first (in order)

1. `/home/user/projects/routstr_main/provable-ai/TLSN-routstr.md` — the design
   (roles, three channels A/B/C, verified-mode rules). § "Component Changes per
   Repo" → routstrd is your M4 target.
2. `/home/user/projects/routstr_main/provable-ai/STATUS.md` — full history,
   architect decisions, known gotchas (read the tail especially).
3. This file.

## Where the work lives NOW (important — it moved)

The lab copies in `provable-ai/` are **frozen snapshots**. Live branches:

| Component | Location | Branch / state |
|---|---|---|
| proverd + tlsn-verifier + mock_upstream | `/home/user/projects/routstr_main/proverd` | `main` @ 5ab1b37, pushed to `Routstr/proverd` |
| SDK verifier rail | `/home/user/projects/routstr_main/routstr-sdk/.worktrees/tlsn-verifier` | `feat/tlsn-verifier` @ d5543a5, pushed |
| routstr-core verified mode | `/home/user/projects/routstr_main/routstr-core/.worktrees/tlsn-verified-mode` | `feat/tlsn-verified-mode` @ 7eb7e43c, pushed |
| **routstrd (your target)** | `/home/user/projects/routstr_main/routstrd` | **untouched, dirty main tree — do NOT work here** |

### M4 worktree setup (first action)

routstrd's main working tree is dirty (`package.json`, recovery-probe files).
Create your own worktree (`.worktrees/` is already in routstrd's `.gitignore`):

```bash
cd /home/user/projects/routstr_main/routstrd
git fetch origin
git worktree add .worktrees/tlsn-verify -b feat/tlsn-verify origin/main
cd .worktrees/tlsn-verify && bun install
```

M4 needs the SDK's tlsn rail: point the worktree's `@routstr/sdk` dependency
at the SDK worktree (`/home/user/projects/routstr_main/routstr-sdk/.worktrees/tlsn-verifier`,
e.g. `link:` / `file:` or `bun link`) — do NOT wait for the npm release.

## M4 — routstrd wiring

Scope (from the design doc + M3 handoff notes):

1. `src/utils/config.ts`: `verifyUpstream` flag + tlsn settings:
   - `TLSN_VERIFIER_BIN` resolution (env → `~/.routstrd/bin/tlsn-verifier` →
     error with install instructions),
   - host policy (allow/deny lists for upstream hosts the verifier will dial),
   - optional Tor toggle for channel C (defer if it's a rabbit hole — note it).
2. `src/daemon/http/index.ts` (~line 2079, the `routeRequests({...})` call):
   - when enabled, pass `verify: "tlsn"` + tlsn options through,
   - surface `upstreamVerification` from the response: log it, add an
     `x-routstr-tlsn-status` response header
     (`verified:<host>` / `mismatch:<reason>` / `unavailable:<reason>`).
3. CLI output: `verified ✓ <host>` / `mismatch ✗` / `unavailable` next to the
   provider URL that routstrd already prints per request.
4. `src/daemon/types.ts`: carry verification into usage/log records (optional
   but cheap).
5. Bun tests with a mock node — follow the pattern in the SDK worktree's
   `tests/bun/routstrClientTlsn.test.ts`.

## Binary packaging (part of M4)

- `routstrd/scripts/build-tlsn-verifier.sh`: clone/build `tlsn-verifier` from
  `Routstr/proverd` at a pinned rev into `~/.routstrd/bin/`. Daemon-first
  (no npm per-platform binary shipping for now).
- Document in routstrd README: verified mode requires `tlsn-verifier` binary +
  a node advertising `tlsn` capability (`proverd_ws` in the model listing).

## M5 — e2e harness

- One script (in `provable-ai/` or the routstrd worktree `scripts/`) that:
  1. starts `mock_upstream` (from the proverd repo),
  2. starts `proverd`,
  3. starts routstr-core from the core worktree (`uv run`, `TLSN_PROVERD_URL`
     set, fixture CA configured),
  4. starts routstrd from your worktree pointed at the LOCAL node,
  5. runs a non-streaming chat completion, then a streaming one,
  6. asserts `verified ✓` on both, and asserts tamper → mismatch on at least
     one adversarial case.
- `.env.example` hooks for a REAL upstream API key later. Never invent keys.

## Known gotchas (found during architect verification)

1. SDK bun tests: `TLSN_LAB_DIR` fallback path is wrong from worktrees —
   always export `PROVERD_BIN`, `TLSN_VERIFIER_BIN`, `MOCK_UPSTREAM_BIN`
   (= `…/proverd/target/debug/examples/mock_upstream`), `TLSN_FIXTURE_CA_DER`,
   `TLSN_FIXTURE_CA_PEM`. Fix the fallback or document it.
2. proverd repo lacks a PEM fixture (`fixtures/` has only `.der`) but
   `PROVERD_EXTRA_ROOT_CERT_PEM` needs PEM — add `fixtures/root_ca.crt`.
3. proverd fails silently on unreadable extra-root PEM — make it a loud
   startup error while you're in there (small proverd PR).
4. proverd test binaries: `cargo build --release` in the proverd repo gives
   ~2.4× faster proofs than debug — use release in the harness.

## Phase after M5: live VPS testing (with the user's VPS node)

The user has a VPS running a live routstr node with a real upstream. After M5
is green locally:

1. **Deployment gap to close first**: proverd currently binds
   `127.0.0.1:7047` and the model listing advertises proverd's own ws URL —
   on a real node the client can't reach that. routstr-core should proxy
   channel B (e.g. `GET /v1/tlsn/ws?session_id=…` → proverd) and advertise
   THAT URL in `proverd_ws`. Small routstr-core addition (core worktree).
2. Deploy proverd + updated node on the VPS (real upstream key in node env).
3. Point routstrd at the VPS node, run verified non-streaming + streaming
   against the real upstream. Measure TTFT premium vs unverified (compare
   against the design doc's latency table; expect roughly the constant
   few-hundred-ms premium, more if the VPS is far away).
4. Record results in STATUS.md.

## Rules

- Work only in your routstrd worktree (+ small PRs to the proverd/core/sdk
  branches if you hit the gotchas above). Never the dirty routstrd main tree,
  never the frozen lab copies.
- Commit per milestone; keep `provable-ai/STATUS.md` updated (append your own
  M4/M5 sections).
- Design forks the doc doesn't answer → write in STATUS.md and stop that
  thread; the architect steers.
- No real API keys anywhere except the node's own env on the VPS.
