# Production-readiness roadmap

> Scoped 2026-06-29; **status updated 2026-09-30.** Companion to the "Production
> readiness" section in `README.md` (which describes the *current* state); this describes
> what would *raise* it.
>
> **Where it stands:** Tier 1 (security) is **done**, including adversarial hardening
> (1.5). Tier 2 (reliability) is **done** — the 48–72 h soak target was met **twice**
> (48.7 h on v0.9.3, 51.6 h on v0.9.6), with 0 panics across every soak. Tier 3 is partly
> done (a metrics surface exists; upstreaming is well along); multi-monitor is still blocked.

## Framing — the ceiling, and the realistic target

macOS puts a **hard ceiling** on "production": there is no multi-user concurrent
interactive GUI session model like Windows Terminal Services — one interactive
desktop per user, period. So the realistic target is **not** "enterprise RDP server."
It is:

> A **reliable, secure, unattended single-session server** you can deploy for yourself
> or a small team over a **LAN or VPN** and trust to stay up.

That is reachable. Everything below moves toward it; the [NO-GOs](#the-honest-no-gos)
are scope limits, not gaps to close.

## Tier 1 — Security (lifts the "trusted-LAN only" caveat)

1. **Real TLS certificates — DONE (2026-06-30).** The operator can now supply a real
   CA / ACME / Let's Encrypt cert/key via `--cert`/`--key` (or `TLS_CERT`/`TLS_KEY` in
   config.env), so clients can verify the server's identity instead of relying on
   trust-on-first-use. When set, macrdp uses exactly those files and **never** silently
   falls back to self-signed (a missing/bad file is a hard error), and it warns at
   startup if the cert is expired / within 14 days. Self-signed in `~/Library/Application
   Support/macrdp` remains the zero-config default. (Dropping `cert.pem`/`key.pem` into
   the cert dir also still works.) Not done here: ACME auto-renewal (operator tooling's
   job — replace the file + restart) and hot reload (a cert change needs a restart).
2. **Auth hardening — DONE (2026-06-30).** In front of the NLA/CredSSP pre-auth gate,
   macrdp now does per-source-IP connection **rate-limiting** + escalating auto-expiring
   **failed-attempt lockout** + a greppable **auth audit log** (`macrdp::audit` lines:
   who connected, from where, accept/reject/disconnect + outcome). On by default with
   conservative tunable thresholds (env / `config.env`); **loopback is exempt** so you
   can't self-lock. Lives in `src/auth_guard.rs` (a pure, unit-tested decision core) wired
   through the existing `ConnectionHandler` seam — **zero vendored divergence**. The lockout is deliberately **heuristic**
   (errored/very-short ⇒ failure; clean long session resets), so a benign disconnect never
   locks anyone out. Not done here: precise CredSSP-failure classification (would need a
   vendored signal — intentionally avoided).
3. **Document the posture honestly — DONE.** Even hardened, internet-facing RDP is a bad
   idea for *any* server — the production answer is "behind a VPN or an RD Gateway." This is
   stated in the README **§Production readiness** (the "Short version" line: don't put it on a
   public IP, and the trusted-LAN-scope limitation: put internet-facing RDP behind a VPN / RD
   Gateway), and reinforced by a **Network exposure** note next to the LAN-bind examples in
   **§Examples**. See also `@docs/macos-gotchas.md` (port 3389 privileged → 3390 default).

4. **Repository + CI supply-chain hardening — DONE (2026-09-20).** The workflows in
   `.github/` are the highest-value tampering target in the repo: once on `main` they run
   with repository secrets and a write-scoped token. The posture, in the order that actually
   matters:
   - **Only the owner has write access.** Every contributor PR arrives from a fork, so nobody
     else can *change* a workflow — only propose one. The realistic risk is therefore not an
     outside contributor but a workflow edit riding along unnoticed inside a large diff and
     being merged.
   - **`.github/CODEOWNERS` + "require review from Code Owners"** on `main` turns exactly that
     into an explicit step: a PR touching `.github/` auto-requests the owner's review and
     cannot be merged without it.
   - **Fork-PR workflow runs require approval for *all* outside collaborators** (was
     first-time contributors only, so a repeat contributor's PR ran CI automatically).
   - **SHA pinning is enforced** (`sha_pinning_required`). Every action was already pinned to a
     full commit SHA with a version comment; the setting stops a future PR quietly swapping one
     for a mutable tag.
   Already true, and worth not re-deriving: **`GITHUB_TOKEN` cannot modify
   `.github/workflows/`** at all (GitHub withholds the `workflow` scope from it, so no
   compromised action can rewrite CI); **fork PRs get no secrets and a read-only token** on
   `pull_request`; all three workflows declare `permissions: contents: read`; and none uses
   **`pull_request_target`**, the trigger that *does* expose secrets to fork code. The
   *dependency* half is the daily `cargo-deny` scan in `security.yml`.
   Not done, deliberately: narrowing `allowed_actions` from `all` to an allowlist (upkeep vs.
   benefit), and required status checks — a PR can still be merged red, which is tolerable
   while the owner is the only merger.

5. **Adversarial hardening — DONE (2026-07-09 → 2026-08-16).** Beyond the auth gate:
   - **Fuzzing** the network-facing decoders with in-tree `cargo-fuzz` harnesses (2026-07-09):
     `ironrdp-rdpeudp` came through clean; `ironrdp-rdpeusb` surfaced **3 real panics**, fixed
     (#147/#149). A URBDRC fuzz target also went upstream (IronRDP #1690).
   - **Resource bounds:** `--max-client-size` caps the resolution a client can request (#153),
     and the smart-card bridge's allocation is bounded.
   - **Four abuse harnesses** (`scripts/soak_abuse{,2,3,4}.sh`, 2026-08-05): pre-TLS floods and
     malformed payloads, slowloris, malformed framing, and UDP multitransport abuse (including a
     400-source-port flood). All pass — see `docs/pin-bump-soak-results.md` §2.
   - **Two unauthenticated remote denial-of-service bugs found and fixed:** a pre-TLS CPU spin
     on a 2-byte frame (found by `soak_abuse3`; v0.9.4, 2026-08-05) and a silent connection
     wedging the accept loop (#180, v0.9.6, 2026-08-16). Both were invisible to the health
     watchdog.
   - **Dependency scanning:** a daily `cargo-deny` scan (#146, 2026-07-09); advisories it
     surfaced were patched in v0.9.7.
   - **SIEM audit stream:** opt-in structured JSON audit events for a log collector
     (`--audit-file`, v0.8.33, 2026-07-10), independent of `RUST_LOG`.

## Tier 2 — Reliability / unattended operation

4. **A real multi-day soak — DONE (2026-08-22).** *(highest confidence per hour.)* The biggest unknown for
   "leave it running" is leaks/drift over time. Known suspects: the *audio long-session
   drift* item, and documented SCStream / NFS-mount leaks on hard kill (`SIGKILL` skips
   `Drop`). Run a 48–72 h soak (idle + active, with reconnect cycles) and fix what it
   surfaces.
   - **Status — DONE (2026-08-22). The 48–72 h target was met twice**, on the Mac mini (M1,
     macOS 26.5) under real use. Figures below are re-derived from the sampler's raw log
     (`~/macrdp-soak-samples.log` on the mini, 60 s samples) on 2026-09-30: **0 panics in all
     7,988 samples** across the August runs.
   - **v0.9.6 — 51.6 h continuous (2026-08-20 01:52Z → 08-22 05:28Z), one process.** RSS
     21–147 MB, idle floor steady ~31–37 MB (the 147 MB peak was a real 3024×1898 session,
     released within minutes). The daily-driver config, used for real over ZeroTier and LAN
     (mstsc and the Windows App for macOS) — including a Windows App reconnect that came up
     blank and **self-healed through blank recovery**. **Two overnight 4 h abuse windows**
     (all four harnesses) ran on a schedule during it, with memory flat throughout. Lesson from
     this run: don't run the abuse harnesses while someone is using the machine — the load
     broke an interactive session's resize, and it's why they moved to an overnight window.
   - **v0.9.3 — 48.7 h continuous (2026-08-03 02:16Z → 08-05 02:56Z), one process.** RSS
     20–197 MB (the 197 MB peak was a deliberate second-client takeover storm, released back to
     ~35 MB); file descriptors 17–42 and identical before and after every session. Plain-TCP
     config (UDP multitransport off, to isolate the core). Blank recovery was observed working
     during it. Connection-level abuse passes (~740 hostile connections) ran on top.
   - **v0.9.5 — 27.4 h, 2026-08-05 → 08-06** (Mac mini M1, the entitled daily-
     driver build: H.264 + AAC + drive redirection + adaptive bitrate). **0 restarts, 0 panics**;
     RSS avg 54 MB (20–116 MB, returning to baseline — **no leak**); up to 2 concurrent real
     clients. It ran alongside the four abuse harnesses (Tier 1.5) and ended only because the
     process was deliberately restarted for the UDP test. Full record:
     `docs/pin-bump-soak-results.md`. (The gate for landing the IronRDP pin bump, not a
     48 h attempt.)
   - **v0.8.21-era — 31 h, 2026-07-01 (the first soak).** The soak run (started 2026-07-01 18:39, **pre-v0.8.22 / pre-ARC** build, 31 h /
     1861 one-minute samples; data recovered on a clean re-copy after a first transfer came back
     zero-filled) shows the **foundation core is clean over time, not just alive:**
     - **No memory leak** — RSS bounded 18–88 MB, tracking activity (88 active at start, down to
       18 idle, back to ~60–71 active), ending *lower* than it started. No threads/fds/SCStreams/
       NFS-mounts/log growth either; **single process the whole run** (no crash/restart/hang).
       Corroborated independently by `pmset` (the `caffeinate` assertion is `-w`-tied to macrdp's
       pid and held unbroken 30 h 45 m) + ~36-day machine uptime.
     - **0 panics.** The 55 `Connection error`s are all per-connection (write-all / accept_begin /
       CredSSP) — the normal client-drop / half-open-probe signatures, non-fatal.
     - **v0.8.21 auth-guard fix FIELD-VALIDATED.** The run captured the before/after: the 17
       lockout rejects of a legitimate LAN client (escalating to ~239 s) are all **pre-fix**
       (06-30 + 07-01 02:xx, before the 18:39 build swap) — this *is* the false-lockout that
       surfaced the v0.8.21 fix. The **post-fix soak window had ZERO lockouts** and 14 perfectly
       balanced accept/disconnect pairs. (The overnight escalation cluster is the "took a few
       tries while I was out" incident — pre-fix, now fixed.)
   - **Scope — what the soaks do NOT cover:** one host, one network, and the configs above; the
     headless blanking modes weren't soaked; and everything after v0.9.6 is unsoaked —
     lock-on-disconnect / auto-unlock, rich clipboard, the microphone, and the v0.9.7
     dependency updates. A re-soak is worth doing before calling those production-grade.
   - **Soak tooling — both earlier notes DONE:** the monitor (`scripts/soak-monitor.sh`) syncs
     to disk after every sample, so an interrupted transfer can't zero-fill the record; and the
     `audio_dvc` "GREEN" status line now logs at DEBUG, not WARN.
5. **Robust teardown + log rotation.** *(log rotation + startup reaper SHIPPED 2026-06-30; health-check watchdog SHIPPED 2026-07-03 — Tier 2.5 complete.)*
   - **Log rotation — DONE.** `~/Library/Logs/macrdp.log` is now a self-owned, size-bounded
     rotating file (`src/logging.rs`: `macrdp.log` + N logrotate-style archives, default
     10 MiB × 5; tunable via `MACRDP_LOG_MAX_BYTES`/`MACRDP_LOG_MAX_FILES`). The plist no
     longer redirects stdout there (panics → a small `macrdp.err.log`; a panic hook also
     routes panics into `macrdp.log` so the GUI still detects crashes).
   - **Startup reaper — DONE.** The graceful `SIGTERM`/`SIGINT` path already unmounts + cleans;
     the remaining leak was `SIGKILL`/panic (uncatchable in-process). `src/reaper.rs` now sweeps
     a *dead* prior process's leftovers on the next start (stale NFS mounts +
     `$TMPDIR/macrdp-{rdpdr,paste,lazy-paste}-<pid>` dirs), dead-pid-gated so it's safe with
     another instance live. (SCStreams / virtual display / blanking were already process-scoped
     and auto-restore.)
   - **Health-check watchdog — DONE (2026-07-03).** `src/health.rs`: a dedicated OS thread
     (not a tokio task, so it survives a wedged runtime) periodically submits a trivial probe
     onto the tokio runtime and waits a bounded time for it to run. A deadlocked runtime never
     runs it; after N consecutive misses the watchdog `process::exit`s with a distinct code
     (70/EX_SOFTWARE) so `KeepAlive` restarts a fresh process —
     closing the "alive but wedged" gap `KeepAlive` alone can't. Conservative by default (15 s
     interval, 30 s timeout, 2 misses ⇒ a wedge must persist ~90 s before a bounce, so load
     spikes never trip it). Armed on the long-lived launchd-watched serve process; skipped, by default, interactively
     (stdout a TTY). Env: `MACRDP_HEALTHCHECK=0/1` + `MACRDP_HEALTHCHECK_{INTERVAL_SECS,TIMEOUT_SECS,FAILURES}`
     (config.env keys `HEALTH_CHECK` / `HEALTHCHECK_*`). Verified: arms headless, no false bounce
     on an idle runtime. **Scope:** targets runtime-level hangs (deadlock / all workers blocked);
     a listener-level heartbeat for "accept loop silently stopped while the runtime is healthy"
     is a possible follow-up. Two instances of exactly that class have since been bounded
     directly instead: the preemption probe (#180, v0.9.6) and — pending — `accept_finalize`
     (#182, held for the next IronRDP pin bump, which carries the upstream bound).
   - **`--detach-primary` restart stopgap — DONE (2026-07-23, #169).** On macOS 26 the panel
     can't be re-enabled in-process after a detach; under launchd macrdp now exits so
     `KeepAlive` restarts it, restoring the panel in ~2–3 s instead of leaving it dark (#168).
6. **Per-connection worker processes (`--fork-workers`) — REMOVED 2026-07-17,
   superseded.** A `--fork-workers` model (a supervisor that fork+exec'd a fresh worker
   process per connection, xrdp-style) was added as one answer to the mstsc
   reconnect-blank, then kept a documented opt-in (DECIDED 2026-07-04: never the default —
   single-process + blank-recovery + ARC was field-proven, composed with everything, and
   had the 31 h clean Tier 2.4 soak, none of which fork-workers matched). Once the server
   learned to self-heal the reconnect-blank **in place** via a bare core
   Deactivation–Reactivation (v0.8.27, now the default blank-recovery action), fork-workers
   no longer bought anything single-process didn't, so it was removed entirely along with
   its process-lifecycle surface (fd passing, worker serialization, the SCK-exit trap) and
   its mutual exclusion with `--enable-udp-multitransport`.

## Tier 3 — Polish / nice-to-have

7. **Single-session multi-monitor** (client-side multi-display). Achievable but **blocked**
   on the git-pinned `ironrdp-acceptor`'s single-monitor `MonitorLayoutPdu`; scoped/paused
   (see the multi-virtual-monitor TODO + memory).
8. **Auto-update** (e.g. Sparkle) so deployed instances stay current.
9. **A status / metrics surface — PARTLY DONE (2026-08-04).** `--stats-endpoint` serves live
   bitrate, RTT, standing queue, fps, frames sent and session size as JSON on loopback, and the
   menu-bar controller's Status tab shows it with server CPU/RAM/uptime and the connected client.
   Security events go to the SIEM audit stream (Tier 1.5). Not done: error counters, history, or
   a scrape format (e.g. Prometheus) for external monitoring.
10. **Upstream the vendored IronRDP forks — WELL ALONG.** 23 macrdp PRs merged upstream
    (2026-05-21 → 09-30). The v0.9.5 pin bump retired two forks outright
    (`ironrdp-async`, `ironrdp-rdpeusb`); five remain (`ironrdp-acceptor`, `-dvc`, `-rdpdr`,
    `-rdpeudp`, `-server`), and the next bump can drop more divergences that have since landed
    upstream (e.g. the `accept_finalize` bound, and the microphone via `ironrdp-rdpeai`).

## The honest NO-GOs

Don't chase these — they're scope limits, not bugs:

- **Multi-user concurrent sessions** — macOS architectural limit (one interactive GUI
  session per user).
- **Capturing DRM video / secure-input fields** — OS-enforced; can't and shouldn't be
  overridden.
- **An enterprise SLA / commercial support** — it's a one-person project.

## Recommendation — the "most production per unit of effort" trio

If picking a starting batch, do these three:

1. **Real TLS certs** (Tier 1.1) — **DONE (2026-06-30).**
2. **Auth rate-limit + lockout + audit log** (Tier 1.2) — **DONE (2026-06-30).**
3. **A 48–72 h soak to shake out leaks/drift** (Tier 2.4) — **DONE (2026-08-22): met twice**,
   48.7 h on v0.9.3 and 51.6 h on v0.9.6, each a single process with no leak and 0 panics,
   under real use plus the abuse harnesses. (Earlier 31 h and 27.4 h runs were clean too.)

That trio takes it from "daily-driver I babysit" to "I can deploy this and walk away on a
network I control." **All three are done**, and Tier 1.5's hardening goes past what the trio
asked for. The remaining work is incremental: re-soak the features added since v0.9.6, and the
Tier 3 items.
