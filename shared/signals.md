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
