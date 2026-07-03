# Perp City mainnet live-wallet + low-cost bot runtime plan

Status: planning/runbook only. No wallet creation, no secrets, no infrastructure changes, no transactions, and no spending are authorized by this document.

## Executive recommendation

Do **not** run mainnet live bots on Fly for the first canary. Use a single low-cost Linux VPS with `systemd`, explicit wallet funding caps, read-only shadow mode first, and a human-approved one-shot path before any daemonized trading.

Recommended non-Fly runtime:

1. **Primary recommendation: Hetzner Cloud ARM VPS (`CAX11`) or equivalent 1-2 vCPU / 2-4 GB VPS.**
   - Rust/Alloy bots should run fine as static release binaries on arm64.
   - `systemd` gives simple restart policy, watchdogs, timers, logs, and credential isolation without a platform-specific deploy layer.
   - For this bot class, RPC/provider latency and protocol safety dominate; Fly-style edge placement is not needed.
2. **Fallback if arm64 build friction appears: small x86 VPS (`CX22`, OVH/Vultr/Linode/DO equivalent).**
   - Slightly less efficient, easier if existing CI only builds x86_64.
3. **Not recommended for mainnet canary:** free-tier/ephemeral hosts, home Raspberry Pi, laptop, cron on a personal machine, or any host that sleeps/restarts unpredictably.

## Current workspace posture

The SDK already has useful low-bandwidth primitives:

- Chain constants distinguish **Arbitrum One `42161`** from **Arbitrum Sepolia `421614`**.
- Transport supports read/write/shared RPC pools, letting reads use cheap/free endpoints while writes use a more reliable endpoint only when needed.
- `get_perp_snapshot` batches market reads via Multicall3.
- `open_*`/`adjust_*` methods parse events from receipts, avoiding follow-up state reads.
- Receipt polling is manual and bounded, avoiding Alloy background block polling.
- Pre-flight simulation is now mandatory in the transaction builder path.

Testnet runbooks remain testnet-only. Mainnet must be treated as a separate critical-risk rollout.

## Hard mainnet invariants

The bot must refuse to start unless all are true:

1. `CHAIN_ID == 42161`; fail closed on any mismatch or missing value.
2. RPC `eth_chainId` returns `0xa4b1` before any signing path is initialized.
3. Deployments/perp allowlist is loaded from a reviewed mainnet config file; no arbitrary env-provided perp address in live mode.
4. `LIVE_TRADING=1` and `MAINNET_ACK=perp-city-mainnet-42161` are both set for live mode.
5. `GLOBAL_KILL_SWITCH` is unset/false locally and in the remote kill-switch source.
6. Wallet address equals the configured `EXPECTED_BOT_ADDRESS`.
7. Wallet USDC and ETH balances are **at or below** the configured caps; overfunding blocks startup.
8. Per-tx, per-market, per-day, and global exposure caps are loaded and nonzero but tiny.
9. Current code git SHA/build ID matches the approved rollout record.
10. The bot starts in `read_only` or `simulate_only`; `send` requires a separate gate.

## Wallet and funding caps

Use a **dedicated mainnet bot wallet** created/funded by the owner outside this runbook. Do not reuse personal, treasury, deployer, or testnet keys.

Initial canary caps should be intentionally small:

| Scope | Initial cap |
|---|---:|
| Wallet USDC balance | 25-50 USDC |
| Wallet ETH gas balance | small Arbitrum gas-only amount, e.g. <= 0.005 ETH |
| Per transaction margin | protocol minimum or near-minimum, normally 5-10 USDC |
| Per market open margin | <= 10 USDC |
| Total open margin | <= 25 USDC |
| Daily new margin | <= 25 USDC |
| Max tx per hour | 1 during canary |
| Max unexpected loss before halt | 5 USDC or first unexplained loss, whichever is lower |

Rules:

- Overfunding is a safety failure, not a convenience.
- The bot never pulls funds from another wallet.
- No approvals beyond the exact token/protocol need. Prefer bounded allowance; if unlimited allowance is unavoidable, the wallet funding cap is the primary blast-radius limit.
- A separate human-controlled wallet should be able to close/withdraw if the bot host is lost.

## Key management

Minimum acceptable low-cost setup:

- No secrets in chat, git, logs, `.env.example`, shell history, or deployment output.
- Store the private key as a host credential, not in the repository.
- Preferred on a systemd VPS: `systemd-creds` encrypted credential or `LoadCredentialEncrypted=`; bot reads from `/run/credentials/...` at runtime.
- Acceptable interim: root-owned `0600` env/credential file under `/etc/perpcity/`, read by a locked-down service user, with no logging of env values.
- Better if available: hardware-backed or remote signer, but do not add that complexity before the tiny canary unless already operational.

Operational key rules:

- One key per bot/runtime/environment.
- Rotate immediately if exposed in chat, logs, CI, shell history, or crash dumps.
- Disable core dumps for the service.
- Service user has no SSH login and no write access to the repo or config except its state directory.

## Low-bandwidth runtime design

Use a single process with three modes: `read_only`, `simulate_only`, `send_once`. Do not start with an infinite autonomous trading daemon.

RPC approach:

- One WebSocket subscription for `newHeads` if stable; use it to update base fee without polling.
- HTTP reads through a cheap/free read pool with low polling frequency.
- Dedicated write RPC only for `eth_estimateGas`, `eth_call` preflight, and `eth_sendRawTransaction` in approved send windows.
- Use Multicall snapshots rather than per-field calls.
- Avoid broad Goldsky scans in the live loop; precompute allowlists offline and refresh slowly.
- Add jitter/backoff; no tight loops. Default read cadence should be tens of seconds to minutes, not subsecond.

Suggested canary cadence:

- Health/chain check: every 60s.
- Allowlisted market snapshots: every 60-300s depending on count.
- Balance/exposure check: every 60s and before every transaction.
- Kill-switch check: every 15-30s and immediately before signing.

## Monitoring and alerts

Minimum metrics/events to log and alert on:

- Process start/stop/restart and build SHA.
- Current mode: `read_only`, `simulate_only`, `send_once`, `halted`.
- Chain ID and RPC endpoint health.
- Wallet ETH/USDC balances and cap status.
- Open positions, total margin, per-market margin, and unrealized/realized PnL.
- Every proposed tx with market, direction, margin, max gas, simulation result, and approval ID.
- Every sent tx hash, receipt status, gas used, decoded events, and post-trade exposure.
- Any simulation revert, receipt timeout, RPC split-brain, nonce error, unexpected position, cap breach, or kill-switch activation.

Alert channels can be simple at first: local journald + one external notification sink. Alerts must never include private keys or full environment dumps.

## Kill switch

Implement layered halt controls:

1. **Local file:** `/etc/perpcity/KILL_SWITCH` or configured path. If present, block all signing/sending.
2. **Remote read-only flag:** small HTTPS object, GitHub raw file, or other static source. Cache last good value; fail closed if unreachable during live mode.
3. **Wallet cap trip:** overfunding, unexpected positions, unexpected allowance, or PnL/loss breach automatically sets local halted state.
4. **Process control:** `systemctl stop perpcity-bot` must stop the loop cleanly.
5. **On-chain/manual fallback:** human-controlled close/withdraw procedure documented separately.

The kill switch must be checked at startup, before simulation, immediately before signing, and immediately before broadcast.

## Gated rollout

### Gate 0 — repo/readiness review

Allowed: local docs, code review, dry-run config validation.

Exit criteria:

- Mainnet allowlist file exists and includes only reviewed markets/perps.
- Chain guard tests exist for `42161` vs `421614`.
- Bot mode defaults to `read_only`.
- No code path can send from default config.

### Gate 1 — testnet live one-shots only

Allowed: Arbitrum Sepolia `421614`, bounded one-shot transactions after explicit approval.

Exit criteria:

- Testnet one-shots prove simulation, send, receipt parsing, exposure accounting, cap enforcement, and kill switch.
- At least one intentional kill-switch test blocks a send.
- Logs are clean and contain no secrets.

### Gate 2 — mainnet read-only shadow

Allowed: Arbitrum One `42161`, no private key required, no signing.

Exit criteria:

- Runs 24-72h on the chosen VPS.
- RPC call volume is measured and within budget.
- Market snapshots, balances for a public address, and alerts work.
- Process survives restart and resumes without duplicate actions.

### Gate 3 — mainnet key loaded, send disabled

Allowed: dedicated capped bot wallet loaded on VPS, `simulate_only`, no broadcasts.

Exit criteria:

- Wallet address matches config.
- Wallet balances are under caps.
- Simulated candidate txs are produced but blocked from broadcast.
- Kill switch blocks simulation/signing path as designed.

### Gate 4 — human-approved dust one-shot

Allowed: exactly one mainnet transaction with explicit approval containing market, perp, direction, margin, limits, max gas, stop condition, and approval ID.

Exit criteria:

- Tx mined successfully or failed safely before broadcast.
- Receipt decoded and post-trade exposure reconciled.
- No unexpected allowance/balance/position changes.
- Bot returns to halted or read-only mode after the one-shot.

### Gate 5 — supervised canary

Allowed: tiny capped `send_once` windows, at most one tx/hour, human monitoring active.

Exit criteria:

- 3-7 days without unexplained reverts, nonce issues, RPC split-brain, cap breaches, missed alerts, or unexpected PnL/loss.
- Actual RPC/bandwidth/cost data reviewed.
- Wallet remains capped; no top-up without renewed approval.

### Gate 6 — limited automation

Allowed only after separate approval. Still cap-driven, allowlist-only, and kill-switchable.

Initial automation should remain conservative: low market count, no self-escalating size, no auto-top-up, no unlimited trading loop, and no strategy changes without review.

## Recommended `systemd` shape

- `perpcity-bot.service`: locked-down long-running read-only/simulate process.
- `perpcity-send-once@.service`: templated one-shot unit for an approved tx payload/approval ID.
- `perpcity-health.timer`: optional health/report job if not built into the process.

Hardening targets:

- Run as non-root service user.
- `NoNewPrivileges=true`.
- `PrivateTmp=true`.
- `ProtectSystem=strict` with explicit writable state directory.
- `ProtectHome=true`.
- `MemoryDenyWriteExecute=true` if compatible.
- `LimitCORE=0`.
- Credentials loaded through systemd credential APIs or a root-owned credential file.

## Final go/no-go

Go only when:

- Testnet mechanics have passed.
- Mainnet has completed read-only shadow.
- Dedicated wallet is capped and expected address is locked in config.
- Kill switch and monitoring have been tested.
- The exact one-shot or canary action has explicit human approval.

No-go if any of these are false, if the wallet is overfunded, if the chain ID cannot be proven as `42161`, if monitoring is down, if the kill-switch source is unreachable in live mode, or if the code/config SHA differs from the approved rollout.
