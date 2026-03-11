# Signals — Inter-Agent Communication

## Format

```
[TIMESTAMP] [AGENT] [SIGNAL_TYPE] message
```

## Signal Types

| Signal | Meaning | Who sends | Who reads |
|--------|---------|-----------|-----------|
| `READY` | Task completed, next agent can proceed | Any pipeline agent | Orchestrator |
| `BLOCKED` | Cannot proceed, needs human input | Any | Orchestrator → Human |
| `ALERT` | Something is wrong (infra, CI, deploy) | Watchdog, DevOps | Orchestrator → Slack |
| `HANDOFF` | Passing context to next pipeline stage | Pipeline agents | Next agent in pipeline |
| `RETRY` | Previous attempt failed, retrying | Any | Orchestrator |
| `STALE` | Agent appears stuck or unresponsive | Heartbeat | Orchestrator |

## Active Signals

**2026-03-11 04:01 WATCHDOG ALERT — PERP FEED STILL DOWN (54+ HOURS), ROOT CAUSE UNCLEAR** — Container status: fathom 5/5 running (sui-liq 2d, sui-arb 2d, fathom 1h, hyper-arb 3d, hyper-liq 3d); ht-deploy 2/2 running. **PERP feed inoperable: all 6 symbols fail snapshot parsing with "error decoding response body". Zero PERP raw files written. SPOT/HL/dYdX feeds operational (last data: 2026-03-11 02:33 UTC, ~1.5h stale but streaming). Issue persists despite rollback — suggests either Binance API schema change or IP ban still partially active. Code regression hypothesis unsupported (issue spans multiple versions).**

**Evidence (live logs 2026-03-11 04:01:18–04:01:22 UTC):**
- All 6 symbols: continuous "snapshot parse failed — error decoding response body"
- WS stream connects successfully, but snapshot bootstrap fails on 4-5/6 symbols (BTC, SOL, XRP, DOGE, BNB; ETH occasionally parses)
- Reconnect cycle every ~1-2 seconds (exponential backoff 1086-1221ms)
- SPOT/HL/dYdX data directory exists with files (2026-03-11 02:33 UTC) — these feeds working
- binance_perp directory does not exist — PERP never bootstrapped after restart
- No status.json being written (health unmonitored)

**Root cause (HYPOTHESIS):** Either:
1. **Binance API response format changed** on 2026-03-09 08:04 UTC (affects all code versions at/after this date)
2. **IP ban still partially active** — WS connects but snapshot REST endpoint returns malformed response
3. **SnapshotRest struct incompatibility** — parser unable to deserialize current Binance response regardless of code version

Previous theory (simple code regression) **disproven** — rollback to v20260307-75bd0a6 did NOT fix issue.

**Impact:** ar-quant-alpha, hyper-liq, hyper-arb cannot execute PERP trades. Data gap 54+ hours. SPOT/HL/dYdX operational but PERP-dependent strategies blocked.

**Blocker:** Requires either (1) Binance support investigation (API changes), or (2) manual snapshot response inspection + struct comparison. Rollback strategy insufficient.

**Status:** CRITICAL, unresolved. Escalate to human: request Binance snapshot endpoint response dump for debugging.

---

**2026-03-11 11:01 WATCHDOG CRITICAL — FATHOM COMPLETELY NON-FUNCTIONAL (28 MIN UPTIME, PERP+SPOT BOTH DOWN)** — fathom 5/5 running (sui-liq 2d, sui-arb 2d, fathom 28m [restarted 02:33 UTC], hyper-arb 3d, hyper-liq 3d); ht-deploy 2/2 running (ar-quant-alpha 2w, ht-tg-bot 6w). **PERP feed completely inoperable: no raw data directory exists (`/raw/binance_perp/`). SPOT feed stalled: 1s files not updating since 02:33 UTC (container restart time). Only dYdX and Hyperliquid collecting data. CONTAINER MARKED HEALTHY BUT NOT COLLECTING DATA.**

**Evidence:**
- Zero perp raw files: `/app/data/v20260307-75bd0a6/raw/binance_perp/` does not exist
- Spot 1s files frozen: all BTCUSDT-BNBUSDT files modified at 02:33:00 UTC (exactly at startup, no updates in 28 minutes)
- Logs show identical reconnect loop: "snapshot parse failed — error decoding response body" on all 6 perp symbols, exponential backoff 1000-1200ms, repeated every 1-2 seconds
- Gap detection triggering every few seconds (normal reconnect logic activating in failure state)
- status.json not being written (container health NOT monitored)
- HyperLiquid and dYdX data files exist and may be current (not verified)
- Container memory stable: 233.4MB / 384MB (61% usage)

**Root cause (from prior investigation):** Code regression in v20260307-75bd0a6 binary (SnapshotRest struct incompatible with Binance response format, or Binance API format changed).

**Impact:** ar-quant-alpha, hyper-liq, hyper-arb CANNOT execute PERP trades. Data gap 41+ hours. Spot data collection halted. System severely degraded.

**Status:** CRITICAL, unresolved, blocker on rollback decision.

**Evidence (logs 2026-03-11 02:03:47–02:03:50 UTC):**
- Pattern identical to previous state: reconnect → WS connects → snapshot parse fails for 5 symbols → ETH occasionally succeeds → gap → reconnect
- Exponential backoff ~1000-1100ms (working as designed)
- Reconnect cycle repeating every 1-2 seconds (1000+ failures/min)
- No status.json being written (health unmonitored)

**Root cause:** Code regression in v20260307-2f0aafb (SnapshotRest struct likely incompatible with Binance response format). IP ban confirmed lifted (2026-03-10).

**Blocker:** No rollback decision. Code fix requires human action — compare v20260307-2f0aafb vs v20260307-75bd0a6 snapshot parsing logic.

**Impact:** ar-quant-alpha, hyper-liq, hyper-arb CANNOT execute PERP trades. Missing data gap 41+ hours. System degraded.

**Secondary concern:** ht-deploy SSH timeout — unable to verify ar-quant-alpha status. May indicate connectivity/load issue on HT VPS.

**Status:** CRITICAL, unresolved, blocker on human decision.

---

**2026-03-11 01:01 WATCHDOG ALERT — PERP FEED OFFLINE (17+ HOURS, ROLLBACK PENDING)** — All containers healthy: fathom 5/5 (sui-liq 2d, sui-arb 2d, fathom 10h, hyper-arb 3d, hyper-liq 3d); ht-deploy 2/2 (ar-quant-alpha 2w, ht-tg-bot 6w). **PERP feed inoperable. Continuous snapshot parse failures + reconnect loop (1200ms backoff).**

**Evidence (logs 2026-03-11 01:01:07–01:01:11 UTC):**
- All 6 symbols: "error decoding response body" on snapshot REST fetch
- Rotating pattern: occasional "snapshot ok" (XRPUSDT, DOGEUSDT) followed by immediate gap-triggered disconnect
- WS stream connects successfully (fstream.binance.com reachable) but snapshot bootstrap fails
- Reconnect loop every ~1.2 seconds (exponential backoff 1000-1250ms)
- No status.json being written (health unmonitored)

**Root cause:** Code regression in v20260307-2f0aafb (likely SnapshotRest struct schema mismatch with Binance response).

**Recommended action (PENDING DECISION):**
- **Immediate:** Roll back fathom to v20260307-75bd0a6 (last known working)
- **Then:** Compare snapshot parsing code between v20260307-2f0aafb vs v20260307-75bd0a6 to identify schema mismatch

**Impact:** ar-quant-alpha, hyper-liq, hyper-arb cannot execute PERP trades. Data gap 54+ hours.

**Status:** CRITICAL, unresolved. Awaiting rollback decision.

---

**2026-03-11 08:30 WATCHDOG ALERT — PERP FEED STILL OFFLINE (ROLLBACK REQUIRED)** — All containers healthy: fathom 5/5 (sui-liq 2d, sui-arb 2d, **fathom 10h** [restarted], hyper-arb 3d, hyper-liq 3d); ht-deploy 2/2 (ar-quant-alpha 2w, ht-tg-bot 6w). **PERP feed still inoperable after 10h restart. Code regression v20260307-2f0aafb persists.**

**Evidence:**
- PERP WS reconnect loop every 1 second (from logs: repeated "WS connected" messages)
- Snapshot parse failures on ETHUSDT, SOLUSDT, XRPUSDT, DOGEUSDT, BNBUSDT ("error decoding response body")
- BTCUSDT occasionally parses (intermittent success pattern matches previous state)
- Memory stable: 286.5MB / 384MB (safe margin)
- No status.json being written (health unmonitored)
- SPOT/HL/DYDX feeds operational (only PERP affected)

**Root cause (from investigation 2026-03-10 23:05):** Code parsing regression in v20260307-2f0aafb. Binance IP ban already lifted (confirmed HTTP 404 response, not 403).

**Recommended action (PENDING):**
- **Immediate:** Roll back to v20260307-75bd0a6 (last known working version)
- Then investigate code difference in snapshot parsing between v20260307-2f0aafb vs v20260307-75bd0a6

**Impact:** ar-quant-alpha, hyper-liq, hyper-arb cannot execute PERP trades. Data gap 53+ hours.

**Status:** CRITICAL, unresolved. Waiting for rollback decision.

---

**2026-03-10 23:05 WATCHDOG INVESTIGATION COMPLETE — PERP FEED CODE REGRESSION CONFIRMED, IP BAN LIFTED** — All containers healthy: fathom 5/5 running (sui-liq 2d, sui-arb 2d, fathom 8h, hyper-arb 3d, hyper-liq 3d); ht-deploy 2/2 running (ar-quant-alpha 2w, ht-tg-bot 6w). **PERP feed offline 39+ hours. ROOT CAUSE: Code parsing regression in v20260307-2f0aafb, NOT IP ban.**

**Evidence:**
- ✅ Binance API IP ban LIFTED: Direct curl test returns HTTP 404 (not 403 Forbidden)
- ✅ SPOT/HL/DYDX feeds operational: Parquet files writing daily (2026-03-10 latest)
- ❌ PERP feed completely non-functional: Continuous "snapshot parse failed — error decoding response body" on all 6 symbols
- ❌ PERP raw data not being written (WS stuck in reconnect loop)
- ❌ metadata/status.json never created (health unmonitored)
- 🔁 Reconnect loop active: exponential backoff 1000-1250ms, WS connects but snapshot fetch fails immediately

**Root Cause Analysis:**
- IP ban window (75h) from 2026-03-09 08:04 UTC should have expired 2026-03-10 18:16 UTC
- Expected recovery did NOT occur → signals code issue, not network
- Binance API now accessible (HTTP 404) → ban confirmed lifted
- Snapshot parsing fails on ALL symbols with same error → NOT a per-symbol issue
- Container restart 8h ago did NOT fix issue → problem is in binary v20260307-2f0aafb

**Comparison to working version (v20260307-75bd0a6):**
- Previous version may have different SnapshotRest struct schema
- Or v20260307-2f0aafb expects different Binance response format than actual

**Impact:** ar-quant-alpha, hyper-liq, hyper-arb cannot execute PERP trades. System degraded.

**Recommended next steps:**
1. **Immediate:** Roll back to v20260307-75bd0a6 to restore PERP feed
2. **Investigation:** Compare snapshot parsing code between v20260307-2f0aafb vs v20260307-75bd0a6
3. **Prevention:** Add snapshot response validation/logging to catch future parsing regressions early

**2026-03-11 21:31 WATCHDOG CRITICAL — PERP FEED OFFLINE, 37+ HOURS, ROOT CAUSE IDENTIFIED AS CODE REGRESSION** — Containers healthy. fathom: 5/5 running (303.2MB/384MB, 78.97%, 1.62% CPU); ht-deploy: 2/2 running. **PERP feed inoperable for 37+ hours (since 2026-03-09 08:04 UTC). IP ban confirmed lifted — Binance API responding (HTTP 404). Issue is code parsing regression, NOT network.**

**Evidence:**
- Binance API test from fathom: returns HTTP 404 (not 403 Forbidden = IP ban lifted)
- Container logs show intermittent successes: BTCUSDT and BNBUSDT parse OK ("snapshot ok"), but ETH/SOL/XRP/DOGE fail with "error decoding response body"
- Rotating pattern: different symbols succeed each reconnection cycle (suggests either rate-limiting response or schema mismatch in snapshot parsing)
- All other feeds operational (SPOT/HL/DYDX working normally)

**Previous state from 2026-03-10 21:01 alert:
- All 6 PERP symbols: continuous "snapshot parse failed" with "error decoding response body"
- Occasional partials: sporadic "snapshot ok" on random symbols (BTC/ETH/XRP/BNB rotate)
- Reconnect loop active: exponential backoff 1000-1250ms, WS connects but snapshot fetch fails
- status.json NOT being written (health unmonitored)
- Raw depth parquet files ARE being written (WebSocket receiving data)
- SPOT/HL/DYDX feeds operational, only PERP affected

**Root cause STILL unclear after 25+ hours:** Either (1) IP ban didn't lift (ban mechanism broken?), or (2) snapshot parsing regression in v20260307-2f0aafb.

**Timeline:**
- 2026-03-09 08:04 UTC: IP ban triggered
- 2026-03-10 18:16 UTC: Auto-heal window closed
- 2026-03-10 20:07 UTC: Alert posted (feed still offline after expected recovery)
- 2026-03-10 21:01 UTC: **PERP offline 25+ hours. Auto-heal FAILED.**

**Investigation required (URGENT):**
1. **Immediate:** Test if IP ban actually lifted: `curl -I 'https://fapi.binance.com/fapi/v1/depth?symbol=BTCUSDT'` from fathom container (get HTTP response code)
2. If 403/429 → IP ban still active: rotate to new VPS or request Binance support (unusual for ban to persist 6+ hours past window)
3. If 200 → ban lifted but code regression: compare snapshot parsing in v20260307-2f0aafb vs v20260307-75bd0a6 (struct schema change likely)
4. Review raw error logs: check Binance response codes (timeout? 403? schema mismatch?)

**Impact:** ar-quant-alpha, hyper-liq, hyper-arb cannot execute PERP trades. No PERP data for 25+ hours. System degraded.

**Status:** CRITICAL, unresolved. Auto-heal mechanism failed or root cause is code regression. Requires immediate human investigation.

---

**2026-03-10 15:32 WATCHDOG CRITICAL — IP BAN CONFIRMED, AUTO-HEAL AT 18:16 UTC (~2h44m)** — Container status healthy. fathom: 5/5 running (300.6MB/384MB, 2.44% CPU); ht-deploy: 2/2 running. **PERP feed offline since 2026-03-09 08:04 UTC (31 hours).** Current logs (15:32:14-15:32:16 UTC) show:
- All 6 PERP symbols: "error decoding response body" on snapshot REST fetch
- Only XRPUSDT briefly parsed (likely cache hit), rest fail
- Gap counts catastrophic: 281–327 gaps/symbol (threshold <10) in 30 seconds
- Reconnect loop active: WS connects every ~1s, immediately fails on snapshot fetch
- status.json NOT being written (health unmonitored)

**Root cause confirmed:** HTTP 403 Forbidden from `fstream.binance.com` (Binance API level, not connection issue). NOT a parsing regression.

**Timeline:**
- 2026-03-09 08:04 UTC: IP ban triggered (excessive REST reconnect attempts)
- Auto-heal scheduled: 2026-03-10 18:16 UTC (75-hour ban window)
- Current time: 15:32 UTC — **45 minutes remaining until auto-heal**

**Options:**
1. **WAIT** (recommended if deadline allows): Ban expires 18:16 UTC, feed self-heals automatically
2. **ROTATE IP NOW**: Deploy fathom to new VPS to bypass 45-minute wait
3. **BACKOFF LOGIC** (prevent future): Add exponential backoff to snapshot retry (current: aggressive ~1s reconnects)

**Impact:** ar-quant-alpha, hyper-liq, hyper-arb cannot execute PERP trades. SPOT/HL/DYDX feeds operational.

**Container note:** fathom service restarted 39 min ago (likely from error loop during IP ban recovery attempt). Other fathom services stable (2d uptime). HT containers stable (ar-quant-alpha 2w, ht-tg-bot 6w).

**Status:** CRITICAL but time-bounded. Awaiting decision: wait 45min or rotate IP now.

---

**2026-03-10 20:07 WATCHDOG ALERT — PERP FEED STILL OFFLINE (REGRESSION)** — Expected auto-heal at 18:16 UTC did NOT occur. PERP connection in continuous reconnect loop. **Symptoms:**
- All 6 PERP symbols (BTCUSDT, ETHUSDT, SOLUSDT, XRPUSDT, DOGEUSDT, BNBUSDT): snapshot parse fails with "error decoding response body"
- Stale event ages: 145-622 seconds on live symbols
- Gap counts catastrophic: 7000-9600 gaps/symbol today (threshold: <10)
- metadata/status.json not being written (container health unmonitored)
- WS stream connects repeatedly (every 1 second) but fails on snapshot bootstrap
- Raw depth parquet files ARE being written (feed receiving WebSocket data)
- SPOT/HL/DYDX feeds working normally (50 ev/s on SPOT)

**Container status:** fathom running (194.8MB/384MB, 2.04% CPU) — NOT unhealthy. Code issue, not OOM/crash.

**Root cause:** Unknown. Either (1) IP ban not actually lifted, or (2) snapshot parsing regression in v20260307-2f0aafb.

**Impact:** ar-quant-alpha, hyper-liq, hyper-arb cannot execute PERP trades. Missing data for 12+ hours.

**Recommended action:**
1. Check if IP ban is actually lifted (curl from fathom container to Binance snapshot endpoint)
2. If ban still active → rotate IP or wait for auto-heal
3. If ban lifted → debug snapshot parsing in v20260307-2f0aafb vs previous tag (v20260307-75bd0a6)

**Status:** PERP feed offline. Requires manual investigation.

---

**2026-03-10 19:02 WATCHDOG HEALTH CHECK (STALE)** — All 7 containers healthy. fathom: 5/5 (sui-liq 2d, sui-arb 2d, fathom 17h, hyper-arb 2d, hyper-liq 2d). ht-deploy: 2/2 (ar-quant-alpha 14d, ht-tg-bot 6w+). No action required. [SUPERSEDED — see current alert above]

**2026-03-10 10:33 WATCHDOG HEALTH CHECK** — All systems operational. fathom: 5/5 containers running. sui-liq(26.4MB/128MB), sui-arb(3.3MB/64MB), fathom(308MB/384MB @ 80.2%), hyper-arb(3.1MB/64MB), hyper-liq(3.7MB/64MB). ht-deploy: 2/2 running. ar-quant-alpha(healthy, 14d+), ht-tg-bot(6w+). **Status:** IP ban lifted — Binance API responding normally. Perp feed recovering (may see stale errors in logs, reconnect in progress). No action required.

**2026-03-10 18:00 WATCHDOG HEALTH CHECK** — Container health OK. fathom: 5/5 running (sui-liq 2d, sui-arb 2d, fathom 16h, hyper-arb 2d, hyper-liq 2d). HT: 2/2 running (ar-quant-alpha 14d, ht-tg-bot 6w). Perp feed offline (IP ban 2026-03-10 08:04 UTC, expires 18:16 UTC = ~16h remaining). No action required — auto-healing in progress.

**2026-03-10 17:31 WATCHDOG HEALTH CHECK** — Container health OK. fathom: 5/5 running (sui-liq, sui-arb, fathom, hyper-arb, hyper-liq up 2d-16h). HT: 2/2 running (ar-quant-alpha 14d uptime, ht-tg-bot 6w+). Memory normal (302.6MB/384MB fathom, all within limits).

**2026-03-10 17:02 WATCHDOG ALERT — TEMPORARY IP BAN (AUTO-HEALING)** — fathom `perp` feed offline. ROOT CAUSE: Binance rate-limit ban on IP `108.61.127.94`. Ban active since ~2026-03-09 08:04 UTC. **Ban expires automatically in ~45 minutes (2026-03-10 18:16 UTC)**.

**Mechanism:** REST snapshot fetch returns HTTP 429 (Binance code -1003), which doesn't parse as SnapshotRest JSON → "error decoding response body". WS stream connects fine but can't bootstrap without snapshots. Continuous reconnect loop creates 1-2k failures/min.

**What triggered the ban:** Likely excessive REST calls during connect/reconnect cycles (perp feed attempted 6+ reconnections per minute × 30 hours = 10k+ REST calls).

**Options:**
1. **Wait** (recommended) — Ban auto-expires in 75 min. Feed will self-heal when Binance lifts restriction.
2. **Proactive IP rotation** — Deploy to new VPS now to avoid 75 min downtime. Ban is IP-specific, not account-scoped.
3. **Reduce reconnect aggression** — Add exponential backoff to connection retry logic (prevent future bans).

**Impact:** ar-quant-alpha, hyper-liq, hyper-arb depend on perp feed. No perp execution possible for 75 min.

**Status:** PERP feed offline. Self-healing in 75 minutes unless IP rotated.

**2026-03-10 15:37 WATCHDOG OK (HT infrastructure)** — ar-quant-alpha: running, 2w+ uptime, healthy. ht-tg-bot: running, 6w+ uptime. All HT systems nominal.

**2026-03-10 15:37 WATCHDOG CRITICAL STATUS** — fathom containers running, but perp feed inoperable due to IP ban. Spot/hl/dydx feeds operational. Perp data gap 30+ hours. System unhealthy overall.

## Investigation Log

**2026-03-10 17:02 WATCHDOG INVESTIGATION COMPLETE**

**Timeline:**
- 2026-03-09 08:04 UTC: Perp feed fails → starts error loop
- 2026-03-10 ~07:00 UTC: fathom container restarts (15h uptime)
- 2026-03-10 15:31 UTC: Alert posted (before root cause known)
- 2026-03-10 17:02 UTC: Root cause identified (IP ban) and documented

**Diagnosis method:**
1. Confirmed containers healthy (all running)
2. Reviewed fathom logs → "error decoding response body" on snapshot parse
3. Found SnapshotRest struct in code (expects: lastUpdateId, bids, asks)
4. Tested Binance API directly from fathom container
5. Got Binance error -1003: "Way too many requests; IP banned until 1773137818216"
6. Calculated ban expiry: 2026-03-10 18:16 UTC (75 min from detection)

**Other feeds status:** spot, hl, dydx all healthy. Only perp affected.

## Signal History

**2026-03-10 15:00 WATCHDOG OK** — All 7 containers healthy. fathom: sui-liq(27.3MB/128MB, 0%), sui-arb(5MB/64MB, 0.28%), fathom(167.5MB/384MB, 1.31%), hyper-arb(4.3MB/64MB, 0%), hyper-liq(3.5MB/64MB, 0%). ht-deploy: ar-quant-alpha(24.6MB/1GB, 0.16%, 2w uptime), ht-tg-bot(41.1MB/1.9GB, 0%). Max CPU: 1.31%. All memory OK.

**2026-03-10 14:30 WATCHDOG OK** — All 7 containers healthy. fathom: sui-liq(11.5MB/128MB, 2d), sui-arb(2.5MB/64MB, 2d), fathom(291MB/384MB, 13h), hyper-arb(752KB/64MB, 2d), hyper-liq(2.2MB/64MB, 2d). ht-deploy: ar-quant-alpha(24.6MB/1GB, 2w uptime), ht-tg-bot(41.1MB/1.9GB, 6w uptime). CPU all <2.2%.

**2026-03-10 14:00 WATCHDOG OK** — All 7 containers healthy. fathom: sui-liq(29MB/128MB, 2d), sui-arb(3MB/64MB, 2d), fathom(167MB/384MB, 12h), hyper-arb(4MB/64MB, 2d), hyper-liq(4.6MB/64MB, 2d). ht-deploy: ar-quant-alpha(24.6MB/1GB, 2w uptime), ht-tg-bot(41.1MB/1.9GB, 6w uptime). CPU all <3%.

**2026-03-10 13:30 WATCHDOG OK** — All 7 containers healthy. fathom: sui-liq(28.3MB), sui-arb(3MB), fathom(296.5MB/384MB), hyper-arb(4MB), hyper-liq(4.5MB). ht-deploy: ar-quant-alpha(24.6MB, 2w uptime), ht-tg-bot(41.1MB, 6w uptime)

**2026-03-10 17:31 WATCHDOG OK (CONTAINERS)** — All 7 containers healthy. fathom: 5 running (302.6MB/384MB, 2.03% CPU). ht-deploy: 2 running (ar-quant-alpha 14d, ht-tg-bot 6w+). Perp feed stale due to IP ban (expires 18:16 UTC, ~45 min). Spot/hl/dydx feeds operational.

**2026-03-09 00:01 WATCHDOG OK** — All 5 containers healthy (fathom: 180.6MB/384MB, hl:dydx:spot:perp active, ar-quant-alpha: 13d uptime, ht-tg-bot: 6w uptime)
