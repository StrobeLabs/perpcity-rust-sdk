"""
Multi-agent orchestrator for building the PerpCity Rust SDK.

Three specialized agents collaborate sequentially per module group:
  1. DeFi/Quant Agent  — writes core implementation + tests
  2. Quality Agent     — reviews and refactors for idiomatic Rust
  3. Performance Agent — optimizes hot paths + writes benchmarks

Usage:
    python orchestrate.py [--stage STAGE_NAME] [--skip-gate]
"""

import asyncio
import json
import sys
import time
from claude_agent_sdk import query, ClaudeAgentOptions, ResultMessage
from claude_agent_sdk.types import StreamEvent

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------
BASE = "/Users/prashanth/Desktop/Strobe_Collective/strobe"
SDK_DIR = f"{BASE}/perpcity-rust-sdk"

SHARED_DIRS = [
    SDK_DIR,
    f"{BASE}/perpcity-contracts",
    f"{BASE}/perpcity-zig-sdk",
    f"{BASE}/rust_context/shared",  # axiomtrade-rs, hyperliquid-rust-sdk, sol-trade-sdk
]
PERF_EXTRA = [f"{BASE}/rust_context/performance"]  # brrr, evmap
QUALITY_EXTRA = [f"{BASE}/rust_context/architecture"]  # binius, fantoccini

# ---------------------------------------------------------------------------
# Tool sets
# ---------------------------------------------------------------------------
DEFI_TOOLS = ["Read", "Write", "Edit", "Bash", "Glob", "Grep"]
QUALITY_TOOLS = ["Read", "Edit", "Bash", "Glob", "Grep"]
PERF_TOOLS = ["Read", "Edit", "Bash", "Glob", "Grep"]
GATE_TOOLS = ["Bash", "Read"]

# ---------------------------------------------------------------------------
# System prompts
# ---------------------------------------------------------------------------
DEFI_SYSTEM_PROMPT = """\
You are a DeFi/quantitative finance expert building a Rust SDK for the PerpCity \
perpetual futures protocol on Base L2.

REFERENCE CODEBASES (study before writing code):
- `rust_context/shared/axiomtrade-rs/` — Production DeFi SDK. Learn: thiserror \
error handling, async client structure, example-driven docs, module organization.
- `rust_context/shared/hyperliquid-rust-sdk/` — Best DeFi team's SDK. Learn: \
two-layer types (client-facing f64 vs wire U256), flat module structure, serde patterns.
- `rust_context/shared/sol-trade-sdk/` — Trading SDK with HFT infrastructure. Learn: \
nonce caching (`common/nonce_cache.rs`), gas fee strategies (`common/gas_fee_strategy.rs`), \
trading factory pattern (`trading/factory.rs`), performance techniques (`perf/`).
- `perpcity-zig-sdk/src/` — Shows WHAT modules to build, but NOT how. Improve \
every pattern. Do not translate Zig to Rust — build something superior.

SOURCE OF TRUTH:
- `perpcity-contracts/src/interfaces/IPerpManager.sol` — all structs, events, errors
- `perpcity-contracts/src/PerpManager.sol` — all functions and their signatures
- `perpcity-contracts/src/libraries/Constants.sol` — protocol constants

KEY PROTOCOL DETAILS:
- Uniswap V4 AMM underneath; positions are ERC721 NFTs
- USDC is 6-decimal (1 USDC = 1_000_000 on-chain)
- Prices stored as sqrtPriceX96 = sqrt(price) * 2^96
- EIP-1559 gas on Base L2; 1-second block times
- Use Alloy's `sol!` macro for contract bindings — this eliminates all manual ABI work

RULES:
- Write unit tests that test YOUR logic — conversions, math, edge cases, error paths
- Do NOT write tests that just construct a struct and assert a field equals what you set it to. \
Those test Rust, not your code. If there's no logic being exercised, there's no test to write.
- Keep code minimal — no unnecessary abstractions or wrapper functions
- Use thiserror for errors, tokio for async
- Every public function needs a doc comment

NOTES FILE:
- When you finish your work, append a section to `NOTES.md` in the SDK root.
- Format: `## DeFi Agent — <Stage Name>\n` followed by bullet points.
- Write: what you built, key design decisions and WHY, anything the next agent should know \
(tricky areas, known limitations, things you weren't sure about).
- Read NOTES.md at the start if it exists — previous agents may have left context for you.
"""

QUALITY_SYSTEM_PROMPT = """\
You are a Rust architecture and code quality expert. Your job is to review and \
refactor code that another agent has written, making it more idiomatic and well-designed.

REFERENCE CODEBASES (study before reviewing):
- `rust_context/architecture/binius/` — 14-crate workspace with advanced Rust patterns. \
Learn: modular trait hierarchies, type-level generics, careful separation of abstract \
interfaces (hal crate) from concrete implementations, proc macros for reducing boilerplate.
- `rust_context/architecture/fantoccini/` — WebDriver client with clean API design. \
Learn: actor-model concurrency (session/command via mpsc channels), builder pattern for \
ergonomic construction, wrapping complex protocols in high-level safe APIs, feature-gated \
TLS backends.

REVIEW CHECKLIST:
- Idiomatic ownership/borrowing (no unnecessary clones, proper lifetimes)
- Type system leverage (newtype pattern, zero-cost abstractions, exhaustive enums)
- Error handling with thiserror (no unwrap in library code, ? propagation)
- Public API ergonomics (builder pattern where appropriate, sensible defaults)
- Minimal code — delete anything unnecessary, collapse verbose patterns
- Run `cargo clippy -- -D warnings` and fix all warnings
- Run `cargo build` and `cargo test` to verify nothing breaks

RULES:
- Do NOT add features or new modules — only improve what exists
- Do NOT add comments to explain obvious code
- Prefer refactoring to commenting
- If a type or function can be eliminated without loss, eliminate it
- DELETE useless tests. A test that constructs a struct and asserts a field equals what was \
just set is testing Rust, not the SDK. Tests must exercise real logic: conversions, math, \
edge cases, error paths, invariants. If a test has no logic under test, remove it.
- FIX correctness bugs even if they require API changes. An API that produces wrong results \
in a supported use case is not a "feature addition" — it's a bug. For example: a function \
that applies a single price to all positions regardless of their market ID is incorrect, \
not a missing feature. Fix the API signature and update tests/callers.

NOTES FILE:
- Read `NOTES.md` in the SDK root FIRST — previous agents left context about what they built and why.
- When you finish, append a section: `## Quality Agent — <Stage Name>\n` followed by bullet points.
- Write: what you changed, what you deleted and why, remaining concerns, suggestions for the \
performance agent (if applicable).
"""

PERF_SYSTEM_PROMPT = """\
You are a low-level Rust performance engineer specializing in HFT (high-frequency \
trading) systems where nanoseconds matter. You follow a strict measurement-first \
methodology — never guess where time is spent.

REFERENCE CODEBASES (study before optimizing):
- `rust_context/performance/brrr/` — Aggressive optimization demo. Learn: LTO fat \
in Cargo.toml profile, panic=abort, direct system FFI via libc for bypassing std overhead.
- `rust_context/performance/evmap/` — Lock-free concurrent map. Learn: read-write split \
pattern, zero-copy reads, explicit publish() for tunable consistency/performance tradeoff.
- `rust_context/shared/sol-trade-sdk/src/perf/` — HFT performance techniques. Learn: \
SIMD optimizations (simd.rs), zero-copy I/O (zero_copy_io.rs), compiler optimization \
settings (compiler_optimization.rs), ultra-low-latency patterns (ultra_low_latency.rs).

PROFILING TOOLS AVAILABLE (installed on this system):
- `samply record cargo bench -- --bench <name>` — sampling profiler, generates flamegraphs. \
Open the output to find which functions consume the most CPU time.
- `cargo flamegraph --bench <name>` — generates flamegraph SVG from dtrace. \
Read the SVG to identify hotspots.
- `cargo asm --lib "perpcity::<function_path>"` — shows generated assembly for a specific \
function. Use this to verify inlining, check for unexpected bounds checks, see if the \
compiler is optimizing as expected.
- `criterion` (already in dev-deps) — statistical benchmarking with `cargo bench`.

MANDATORY WORKFLOW — follow this order for every optimization:
1. BASELINE: Write Criterion benchmarks first. Run `cargo bench` and record the numbers. \
   This is your baseline — paste the output.
2. PROFILE: Run `samply record cargo bench -- --bench <name>` or `cargo flamegraph` to \
   generate a profile. Identify the actual hotspots — which functions take the most time?
3. INSPECT: For each hotspot, run `cargo asm --lib "perpcity::<path>"` to see the generated \
   assembly. Check: did #[inline] actually inline? Are there unexpected allocations, bounds \
   checks, or branch mispredictions?
4. CHANGE: Make exactly ONE optimization based on what you measured.
5. VERIFY: Run `cargo bench` again. Compare to baseline. If no measurable improvement, REVERT.
6. REPEAT: Go back to step 2 with the new baseline.

Also verify struct sizes with `std::mem::size_of::<T>()` and `std::mem::align_of::<T>()` \
in tests rather than assuming layout.

PERFORMANCE PRIORITY ORDER (what actually matters in on-chain HFT):
1. ARCHITECTURAL — minimize RPC round-trips. A single RPC call is 5-50ms. The entire math \
stack is ~100ns. One avoided RPC call = 50,000x the savings of optimizing tick math. \
Count RPC calls in a full trade cycle (prepare → send → confirm) and eliminate unnecessary ones. \
Find sequential awaits that could be parallelized.
2. TRANSPORT — connection quality, failover speed, hedged request efficiency. Measure endpoint \
selection overhead, circuit breaker transition latency, WS reconnection time.
3. CONCURRENCY — lock contention, atomic ordering correctness, read-path lock-freedom. \
These matter under load when multiple tasks hit shared state simultaneously.
4. CPU — cache alignment, inlining, branch prediction, zero-allocation. Only optimize this \
AFTER the above are solid. This is where profiling tools (samply, cargo asm) shine.

OPTIMIZATION TECHNIQUES (apply only when measurement justifies):
- Cache-line alignment: `#[repr(C)]`, `#[repr(align(64))]` for hot structs
- Lock-free atomics: AtomicU64 with appropriate Ordering
- Zero-allocation hot paths: no Vec/HashMap growth during trading
- #[cold] on error paths, #[inline] on measured hot functions
- Memory layout: group hot fields together, separate cold metadata
- Profile settings in Cargo.toml: LTO, codegen-units=1, panic=abort for release

DELIVERABLES:
- Criterion benchmarks in `benches/math_bench.rs`, `benches/hft_bench.rs`, and `benches/transport_bench.rs`
- Baseline numbers, profile analysis, and optimized numbers (show the diff)
- `[profile.release]` optimizations in Cargo.toml
- Optimized code in `perpcity-rust-sdk/src/`

RULES:
- NEVER optimize without a baseline measurement
- NEVER apply an optimization that doesn't show measurable improvement
- Never sacrifice correctness for speed — run `cargo test` after every change
- Comment non-obvious optimizations with measured improvement (e.g., "// 3.2ns → 1.8ns")

NOTES FILE:
- Read `NOTES.md` in the SDK root FIRST — previous agents left context about what they built, \
design decisions, and suggestions for you specifically.
- When you finish, append a section: `## Perf Agent — <Stage Name>\n` followed by bullet points.
- Write: baseline numbers, what you optimized, final numbers, techniques that didn't help \
(so future runs don't retry them).
"""

# ---------------------------------------------------------------------------
# Stage definitions
# ---------------------------------------------------------------------------
STAGES = [
    {
        "name": "Foundation",
        "defi": (
            "STUDY PHASE (do this first before writing any code):\n"
            "1. Read rust_context/shared/axiomtrade-rs/src/lib.rs and rust_context/shared/axiomtrade-rs/src/errors.rs "
            "— understand how they structure their module tree and error types.\n"
            "2. Read rust_context/shared/hyperliquid-rust-sdk/src/lib.rs and rust_context/shared/hyperliquid-rust-sdk/src/errors.rs "
            "— understand their flat module layout and re-export pattern.\n"
            "3. Read perpcity-contracts/src/interfaces/IPerpManager.sol and perpcity-contracts/src/libraries/Constants.sol "
            "— these are the source of truth.\n"
            "4. Read perpcity-zig-sdk/src/ files to understand what modules exist.\n\n"
            "BUILD PHASE:\n"
            "1) Initialize with `cargo init --lib` if not already done. "
            "2) Write Cargo.toml with dependencies: alloy (version 1, features: full, sol-types), "
            "tokio (version 1, features: full), thiserror 2, serde (version 1, features: derive), serde_json 1. "
            "Dev-deps: criterion 0.5 (features: html_reports), tokio (features: full, test-util). "
            "3) Write src/constants.rs with protocol constants from perpcity-contracts/src/libraries/Constants.sol. "
            "4) Write src/errors.rs with a thiserror Error enum — model it after axiomtrade-rs/src/errors.rs style. "
            "Cover: InvalidPrice, InvalidMargin, InvalidLeverage, InvalidTickRange, Overflow, TxReverted, "
            "EventNotFound, GasPriceUnavailable, TooManyInFlight, plus transparent variants for Alloy errors. "
            "5) Write src/contracts.rs using the sol! macro — copy the exact struct definitions and function "
            "signatures from perpcity-contracts/src/interfaces/IPerpManager.sol. Include PerpManager, IERC20, "
            "IFees, and IMarginRatios contracts. Include all events. "
            "6) Write src/lib.rs that declares all modules — follow hyperliquid-rust-sdk's flat re-export pattern. "
            "Verify with `cargo build`."
        ),
        "quality": (
            "STUDY PHASE (do this first):\n"
            "1. Read rust_context/architecture/fantoccini/src/lib.rs and rust_context/architecture/fantoccini/src/error.rs "
            "— understand their public API surface and error design.\n"
            "2. Read rust_context/architecture/binius/Cargo.toml and browse rust_context/architecture/binius/crates/ "
            "— understand how they structure a complex workspace.\n\n"
            "REVIEW PHASE:\n"
            "Review the foundation files in perpcity-rust-sdk/src/ (lib.rs, constants.rs, errors.rs, "
            "contracts.rs, Cargo.toml). Check: proper re-exports in lib.rs, correct error type design "
            "(compare to fantoccini's error.rs), sol! macro usage matches the Solidity interfaces exactly. "
            "Run cargo clippy and fix warnings."
        ),
        "perf": None,
    },
    {
        "name": "Types",
        "defi": (
            "STUDY PHASE (do this first before writing any code):\n"
            "1. Read rust_context/shared/hyperliquid-rust-sdk/src/exchange/ and rust_context/shared/hyperliquid-rust-sdk/src/info/ "
            "— understand their two-layer type pattern (client-facing f64 vs wire U256).\n"
            "2. Read rust_context/shared/axiomtrade-rs/src/models/ — understand how they structure client-facing types.\n"
            "3. Read perpcity-zig-sdk/src/conversions.zig — understand what conversions are needed.\n\n"
            "BUILD PHASE:\n"
            "1) Write src/types.rs with client-facing types: PerpData, Bounds, Fees, LiveDetails, "
            "OpenInterest, OpenTakerParams, OpenMakerParams, CloseParams, Deployments, CloseResult. "
            "These use f64 for human-readable values, Alloy Address/B256 for identifiers. "
            "No trivial accessor functions — just pub fields. "
            "2) Write src/convert.rs with conversion functions between client types and contract types: "
            "scale_to_6dec, scale_from_6dec, leverage_to_margin_ratio, margin_ratio_to_leverage, "
            "price_to_sqrt_price_x96, sqrt_price_x96_to_price. "
            "Write thorough tests for every conversion (especially edge cases: zero, negative, overflow). "
            "Update lib.rs. Verify with `cargo build && cargo test`."
        ),
        "quality": (
            "STUDY PHASE (do this first):\n"
            "1. Read rust_context/architecture/fantoccini/src/client.rs — understand their builder pattern "
            "and how they expose ergonomic types to users.\n"
            "2. Read rust_context/architecture/binius/crates/ — look for examples of newtype patterns "
            "and type-level abstractions.\n\n"
            "REVIEW PHASE:\n"
            "Review src/types.rs and src/convert.rs. Check: types derive the right traits "
            "(Clone, Debug, etc.), conversion functions handle edge cases, public API is ergonomic. "
            "Consider if any types should use the newtype pattern for type safety. "
            "Run cargo clippy and fix warnings."
        ),
        "perf": None,
    },
    {
        "name": "Math",
        "defi": (
            "STUDY PHASE (do this first before writing any code):\n"
            "1. Read perpcity-zig-sdk/src/liquidity.zig — understand the getSqrtRatioAtTick algorithm "
            "and the liquidity estimation functions. Note the test cases.\n"
            "2. Read perpcity-zig-sdk/src/position.zig — understand entry_price, position_size, "
            "liquidation_price calculations. Note the test cases.\n"
            "3. Read rust_context/shared/hyperliquid-rust-sdk/src/helpers.rs — understand how they "
            "handle fixed-point math in Rust.\n\n"
            "BUILD PHASE:\n"
            "1) Create src/math/mod.rs. "
            "2) Write src/math/tick.rs: tick_to_price, price_to_tick, get_sqrt_ratio_at_tick "
            "(bit-shift lookup table matching Uniswap V4), align_tick_down, align_tick_up. "
            "3) Write src/math/liquidity.rs: estimate_liquidity, liquidity_for_target_ratio. "
            "4) Write src/math/position.rs: entry_price, position_size, position_value, leverage, "
            "liquidation_price. "
            "These are pure functions on Alloy primitives (I256, U256). No structs needed. "
            "Write comprehensive tests — port the test cases from the Zig SDK. "
            "Update lib.rs. Verify with `cargo build && cargo test`."
        ),
        "quality": (
            "STUDY PHASE (do this first):\n"
            "1. Read rust_context/architecture/binius/crates/ — find examples of pure math modules "
            "and how they handle trait bounds on numeric types.\n"
            "2. Read the existing src/math/ files — understand the current implementation.\n\n"
            "REVIEW PHASE:\n"
            "Review src/math/ (tick.rs, liquidity.rs, position.rs). Check: pure functions with no "
            "side effects, proper use of Alloy primitives, edge case handling (zero division, overflow), "
            "consistent naming. Run cargo clippy and fix warnings."
        ),
        "perf": (
            "STUDY PHASE (do this first before optimizing anything):\n"
            "1. Read rust_context/performance/brrr/Cargo.toml and rust_context/performance/brrr/.cargo/config.toml "
            "— understand their aggressive compiler optimization settings.\n"
            "2. Read rust_context/performance/brrr/src/main.rs — study their performance techniques "
            "(direct system calls, memory-mapped I/O, branch-free parsing).\n"
            "3. Read rust_context/performance/evmap/src/lib.rs — understand the read-write split architecture.\n\n"
            "OPTIMIZE PHASE (measurement-first):\n"
            "The tick and position math functions are called on every price update. "
            "Write Criterion benchmarks in benches/math_bench.rs "
            "for: tick_to_price, price_to_tick, get_sqrt_ratio_at_tick, entry_price, liquidation_price. "
            "Follow the mandatory workflow: baseline → profile → inspect asm → change → verify. "
            "Add [[bench]] section to Cargo.toml."
        ),
    },
    {
        "name": "HFT",
        "defi": (
            "STUDY PHASE (do this first before writing any code):\n"
            "1. Read rust_context/shared/axiomtrade-rs/src/client/ — understand how they structure "
            "async client infrastructure with multiple subsystems.\n"
            "2. Read rust_context/shared/hyperliquid-rust-sdk/src/exchange/ — understand their "
            "transaction submission and nonce handling patterns.\n"
            "3. Read perpcity-zig-sdk/src/ — identify all HFT-related modules (nonce, gas, pipeline, etc.).\n\n"
            "BUILD PHASE:\n"
            "1) Create src/hft/mod.rs. "
            "2) Write src/hft/nonce.rs: NonceManager with AtomicU64 for lock-free nonce acquisition, "
            "Mutex<HashMap> for pending tracking. Methods: acquire, track, confirm, release, resync. "
            "3) Write src/hft/gas.rs: Urgency enum, GasFees struct, GasCache with RwLock, "
            "gas limit constants. Methods: update, fees_for. "
            "4) Write src/hft/pipeline.rs: TxPipeline combining NonceManager + GasCache. "
            "Methods: prepare (zero RPC), record_submission, confirm, fail, stuck_txs, prepare_bump. "
            "5) Write src/hft/state_cache.rs: StateCache with multi-layer TTL (slow: fees/bounds 60s, "
            "fast: prices/funding 2s). All methods take explicit now_ts for deterministic testing. "
            "6) Write src/hft/latency.rs: LatencyTracker with 1024-sample rolling window, "
            "O(1) record, O(n log n) stats (p50/p95/p99). "
            "7) Write src/hft/position_manager.rs: ManagedPosition with stop-loss/take-profit/"
            "trailing-stop triggers. PositionManager with track/untrack/check_triggers. "
            "All modules must be fully testable without network. Write tests for each. "
            "Update lib.rs. Verify with `cargo build && cargo test`."
        ),
        "quality": (
            "STUDY PHASE (do this first):\n"
            "1. Read rust_context/architecture/fantoccini/src/session.rs and rust_context/architecture/fantoccini/src/client.rs "
            "— understand their actor-model concurrency pattern (mpsc channels, command dispatch).\n"
            "2. Read rust_context/architecture/binius/crates/ — look for examples of trait hierarchies "
            "used to abstract over different backends.\n\n"
            "REVIEW PHASE:\n"
            "Review src/hft/ (all 6 files). Check: proper use of Arc/Mutex/RwLock/AtomicU64, "
            "no deadlock risks (lock ordering), clean separation of concerns, idiomatic error handling. "
            "The nonce manager should use AtomicU64 with appropriate Ordering. "
            "Run cargo clippy and fix warnings."
        ),
        "perf": (
            "STUDY PHASE (do this first before optimizing anything):\n"
            "1. Read rust_context/performance/evmap/src/lib.rs, rust_context/performance/evmap/src/read.rs, "
            "and rust_context/performance/evmap/src/write.rs — understand the lock-free read-write split "
            "pattern, zero-copy reads, and explicit publish() semantics.\n"
            "2. Read rust_context/performance/brrr/src/main.rs — study their zero-allocation techniques "
            "and branch-free hot paths.\n"
            "3. Read the existing src/hft/ files — understand the current implementation before changing anything.\n\n"
            "OPTIMIZE PHASE (measurement-first):\n"
            "Focus areas: "
            "1) nonce.rs — verify lock-free acquire path, check atomic orderings "
            "2) gas.rs — RwLock read path should be uncontended "
            "3) state_cache.rs — consider evmap-style read-write split for zero-lock reads "
            "4) latency.rs — O(1) record must have no allocations "
            "5) pipeline.rs — prepare() must be zero-allocation, zero-RPC "
            "Write Criterion benchmarks in benches/hft_bench.rs for: nonce acquire/release cycle, "
            "gas cache lookup, state cache read/write, pipeline prepare. "
            "Follow the mandatory workflow: baseline → profile → inspect asm → change → verify. "
            "Add #[repr(C)] or #[repr(align(64))] only where measurement justifies it."
        ),
    },
    {
        "name": "Transport",
        "defi": (
            "STUDY PHASE (do this first before writing any code):\n"
            "1. Read perpcity-zig-sdk/src/connection.zig and perpcity-zig-sdk/src/multi_rpc.zig "
            "— understand what the Zig SDK built: multi-endpoint selection, health tracking, failover. "
            "We will build something far more powerful using tower middleware.\n"
            "2. Read rust_context/shared/sol-trade-sdk/src/common/nonce_cache.rs and "
            "rust_context/shared/sol-trade-sdk/src/common/gas_fee_strategy.rs — study their connection patterns.\n"
            "3. Read rust_context/shared/axiomtrade-rs/src/client/ — study how they construct providers.\n"
            "4. Read the existing perpcity-rust-sdk/src/hft/ files — understand what local infrastructure "
            "already exists that the transport layer needs to integrate with.\n"
            "5. Read NOTES.md for context from previous agents.\n\n"
            "BUILD PHASE:\n"
            "Build a transport layer that crushes the Zig SDK's hand-rolled connection management "
            "by leveraging Alloy + tower middleware composition.\n\n"
            "1) Add tower dependencies to Cargo.toml: tower (features: retry, timeout, limit, hedge, buffer, "
            "load, discover), tower-http, pin-project-lite.\n"
            "2) Create src/transport/mod.rs.\n"
            "3) Write src/transport/config.rs: TransportConfig builder with:\n"
            "   - Multiple endpoint URLs (HTTP and WebSocket)\n"
            "   - Per-endpoint timeouts, retry policy, circuit breaker thresholds\n"
            "   - Strategy enum: RoundRobin, LatencyBased, Hedged { fan_out: usize }\n"
            "4) Write src/transport/health.rs: EndpointHealth tracker:\n"
            "   - Rolling latency window (reuse LatencyTracker pattern from hft/)\n"
            "   - Error rate tracking with exponential decay\n"
            "   - Circuit breaker states: Closed (healthy) → Open (dead) → HalfOpen (probing)\n"
            "   - Automatic recovery probing after cooldown\n"
            "5) Write src/transport/provider.rs: HftTransport that composes tower layers:\n"
            "   - tower::timeout for per-request timeouts\n"
            "   - tower::retry with custom policy (retry reads, never retry writes/sends)\n"
            "   - Hedged requests: fan out reads to N endpoints, take fastest response\n"
            "   - Circuit breaker per endpoint\n"
            "   - Must implement or wrap Alloy's Transport/Provider traits so PerpClient can use it\n"
            "6) Write src/transport/ws.rs: WebSocket subscription manager:\n"
            "   - Subscribe to new blocks, pending transactions, contract events\n"
            "   - Auto-reconnect on disconnect with exponential backoff\n"
            "   - Multiplexed: one WS connection serves multiple subscription types\n"
            "7) Write tests: mock endpoints that simulate latency/failures/timeouts, "
            "verify failover behavior, verify hedged requests take fastest, "
            "verify circuit breaker opens after N failures and recovers after cooldown.\n"
            "Update lib.rs. Verify with `cargo build && cargo test`."
        ),
        "quality": (
            "STUDY PHASE (do this first):\n"
            "1. Read rust_context/architecture/fantoccini/src/session.rs — study their actor-model "
            "pattern for managing long-lived connections with reconnection logic.\n"
            "2. Read rust_context/architecture/fantoccini/src/client.rs — study their builder pattern.\n"
            "3. Read rust_context/architecture/binius/crates/ — look for trait abstraction patterns "
            "that separate interface from implementation.\n\n"
            "REVIEW PHASE:\n"
            "Review src/transport/ (all files). Check:\n"
            "- Builder pattern is ergonomic with sensible defaults\n"
            "- tower layer composition is correct (ordering matters)\n"
            "- Circuit breaker state machine is sound (no impossible transitions)\n"
            "- WebSocket reconnection doesn't leak tasks or connections\n"
            "- The transport implements the right Alloy traits for PerpClient to consume\n"
            "- Error types integrate cleanly with PerpCityError\n"
            "Run cargo clippy and fix warnings."
        ),
        "perf": (
            "STUDY PHASE (do this first before optimizing anything):\n"
            "1. Read rust_context/performance/evmap/src/lib.rs — the read-write split pattern "
            "may apply to the health tracker (many concurrent reads, rare writes).\n"
            "2. Read rust_context/shared/sol-trade-sdk/src/perf/ultra_low_latency.rs and "
            "rust_context/shared/sol-trade-sdk/src/perf/zero_copy_io.rs — study their transport optimizations.\n"
            "3. Read the existing src/transport/ files and NOTES.md.\n\n"
            "OPTIMIZE PHASE (measurement-first):\n"
            "This is the most performance-critical module in the entire SDK. Focus on what "
            "actually matters — minimizing RPC round-trip count and latency, NOT CPU nanoseconds.\n"
            "1) Write benchmarks in benches/transport_bench.rs: endpoint selection latency, "
            "health check overhead, hedged request fan-out overhead.\n"
            "2) Measure: how many RPC calls does a full trade cycle take? (prepare → send → confirm). "
            "Identify any sequential awaits that could be parallelized.\n"
            "3) Verify the health tracker's read path is lock-free for the hot path (endpoint selection).\n"
            "4) Verify hedged request cancellation is clean — losing responses must be dropped, not leaked.\n"
            "Follow the mandatory workflow: baseline → profile → change → verify."
        ),
    },
    {
        "name": "Client",
        "defi": (
            "STUDY PHASE (do this first before writing any code):\n"
            "1. Read rust_context/shared/axiomtrade-rs/src/client/ — study their async client structure, "
            "how they wrap multiple protocol calls, and their constructor pattern.\n"
            "2. Read rust_context/shared/hyperliquid-rust-sdk/src/exchange/ and rust_context/shared/hyperliquid-rust-sdk/src/info/ "
            "— study how they separate read (info) from write (exchange) operations.\n"
            "3. Read rust_context/shared/hyperliquid-rust-sdk/src/lib.rs — study their top-level re-exports.\n"
            "4. Read the existing perpcity-rust-sdk/src/ files — understand the types, contracts, math, hft, "
            "and transport modules that the client will wire together.\n"
            "5. Read NOTES.md for context from all previous agents.\n\n"
            "BUILD PHASE:\n"
            "1) Write src/client.rs: PerpClient struct that wires together the transport layer, "
            "HFT infrastructure, and contract bindings into one ergonomic API.\n"
            "   - Constructor takes HftTransport (from src/transport/) + wallet + deployments\n"
            "   - Internally holds: TxPipeline, StateCache, sol!-generated contract instances\n"
            "   - The transport layer handles all RPC failover/hedging transparently\n"
            "Public API: new, open_taker, open_maker, close_position, adjust_notional, adjust_margin, "
            "ensure_approval, get_perp_config (cached via StateCache), get_perp_data, get_position, "
            "get_mark_price, get_live_details, get_open_interest, get_funding_rate, get_usdc_balance. "
            "Use sol!-generated typed calls — no manual ABI encoding. "
            "Parse events from receipts using Alloy's log decoding. "
            "2) Update src/lib.rs with final public API re-exports. "
            "Verify with `cargo build`."
        ),
        "quality": (
            "STUDY PHASE (do this first):\n"
            "1. Read rust_context/architecture/fantoccini/src/client.rs — study their builder pattern, "
            "how they construct the client ergonomically, and their clean public API.\n"
            "2. Read rust_context/architecture/fantoccini/src/lib.rs — study what they expose publicly.\n\n"
            "REVIEW PHASE:\n"
            "Review src/client.rs and src/lib.rs. Check: clean public API (compare to fantoccini's approach), "
            "proper integration with HftTransport (the client should accept it, not construct its own provider), "
            "ergonomic builder or constructor pattern, no leaking internal types. "
            "The lib.rs re-exports should give users a clean `use perpcity::*` experience. "
            "Run cargo clippy and fix warnings."
        ),
        "perf": None,
    },
    {
        "name": "Polish",
        "defi": (
            "STUDY PHASE (do this first):\n"
            "1. Read rust_context/shared/hyperliquid-rust-sdk/src/bin/ — study how they write example binaries.\n"
            "2. Read rust_context/shared/axiomtrade-rs/src/ — look for any example patterns.\n"
            "3. Read the full perpcity-rust-sdk/src/ codebase — understand all public APIs available.\n\n"
            "BUILD PHASE:\n"
            "1) Write examples/open_position.rs — basic long/short taker position example. "
            "2) Write examples/market_maker.rs — maker position with tick range. "
            "3) Write examples/hft_bot.rs — full HFT pipeline (nonce + gas + state cache + pipeline). "
            "4) Verify all examples compile with `cargo build --examples`. "
            "5) Run full test suite: `cargo test`."
        ),
        "quality": (
            "STUDY PHASE (do this first):\n"
            "1. Read rust_context/architecture/fantoccini/src/lib.rs — study their final public API surface "
            "as a model for clean exports.\n"
            "2. Read the full perpcity-rust-sdk/src/lib.rs — understand current export state.\n\n"
            "REVIEW PHASE:\n"
            "Final review pass over the entire codebase. Run `cargo clippy -- -D warnings`. "
            "Check that lib.rs exports are clean, examples compile, and there is no dead code. "
            "Run `cargo doc --no-deps` and verify documentation renders correctly."
        ),
        "perf": (
            "STUDY PHASE (do this first):\n"
            "1. Read rust_context/performance/brrr/Cargo.toml and rust_context/performance/brrr/.cargo/config.toml "
            "— study their release profile settings (LTO, codegen-units, panic strategy).\n"
            "2. Read the existing benches/ directory — review what benchmarks exist.\n\n"
            "OPTIMIZE PHASE:\n"
            "1) Add [profile.release] to Cargo.toml — model after brrr's settings: "
            "lto = 'fat', codegen-units = 1, panic = 'abort'. "
            "2) Run all benchmarks: `cargo bench`. "
            "3) Report final performance numbers with comparison to any earlier baselines. "
            "4) Ensure all tests still pass: `cargo test`."
        ),
    },
]


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------
def _format_tool_summary(tool_name: str, tool_input: dict) -> str:
    """Format a one-line summary of what a tool call is doing."""
    if tool_name == "Read":
        path = tool_input.get("file_path", tool_input.get("path", "?"))
        parts = path.rsplit("/", 2)
        short = "/".join(parts[-2:]) if len(parts) >= 2 else path
        return f"Reading {short}"
    elif tool_name == "Write":
        path = tool_input.get("file_path", tool_input.get("path", "?"))
        parts = path.rsplit("/", 2)
        short = "/".join(parts[-2:]) if len(parts) >= 2 else path
        return f"Writing {short}"
    elif tool_name == "Edit":
        path = tool_input.get("file_path", tool_input.get("path", "?"))
        parts = path.rsplit("/", 2)
        short = "/".join(parts[-2:]) if len(parts) >= 2 else path
        return f"Editing {short}"
    elif tool_name == "Bash":
        cmd = tool_input.get("command", "?")
        if len(cmd) > 80:
            cmd = cmd[:77] + "..."
        return f"$ {cmd}"
    elif tool_name == "Glob":
        pattern = tool_input.get("pattern", "?")
        return f"Glob {pattern}"
    elif tool_name == "Grep":
        pattern = tool_input.get("pattern", "?")
        path = tool_input.get("path", ".")
        parts = path.rsplit("/", 2)
        short = "/".join(parts[-2:]) if len(parts) >= 2 else path
        return f"Grep '{pattern}' in {short}"
    elif tool_name == "Task":
        desc = tool_input.get("description", "")
        prompt = tool_input.get("prompt", "")
        if desc:
            return f"Subagent: {desc}"
        elif prompt:
            short = prompt[:80] + "..." if len(prompt) > 80 else prompt
            return f"Subagent: {short}"
        return "Subagent: (spawning)"
    elif tool_name == "LSP":
        op = tool_input.get("operation", "?")
        path = tool_input.get("filePath", "?")
        parts = path.rsplit("/", 2)
        short = "/".join(parts[-2:]) if len(parts) >= 2 else path
        return f"LSP {op} in {short}"
    return f"{tool_name}({json.dumps(tool_input)[:60]})"


async def run_agent(
    prompt: str,
    system_prompt: str,
    tools: list[str],
    extra_dirs: list[str] | None = None,
    label: str = "Agent",
    max_turns: int = 80,
) -> str | None:
    """Run a single agent to completion with real-time logging."""
    dirs = SHARED_DIRS + (extra_dirs or [])
    result = None
    turn = 0
    current_tool = None
    tool_input_chunks = ""
    in_tool = False

    async for message in query(
        prompt=prompt,
        options=ClaudeAgentOptions(
            system_prompt=system_prompt,
            allowed_tools=tools,
            model="opus",
            cwd=SDK_DIR,
            add_dirs=dirs,
            permission_mode="acceptEdits",
            max_turns=max_turns,
            include_partial_messages=True,
        ),
    ):
        if isinstance(message, StreamEvent):
            event = message.event
            event_type = event.get("type")

            if event_type == "content_block_start":
                content_block = event.get("content_block", {})
                if content_block.get("type") == "tool_use":
                    current_tool = content_block.get("name")
                    tool_input_chunks = ""
                    in_tool = True

            elif event_type == "content_block_delta":
                delta = event.get("delta", {})
                if delta.get("type") == "input_json_delta":
                    tool_input_chunks += delta.get("partial_json", "")
                elif delta.get("type") == "text_delta" and not in_tool:
                    pass  # Suppress streamed text — too noisy

            elif event_type == "content_block_stop":
                if in_tool and current_tool:
                    try:
                        tool_input = json.loads(tool_input_chunks)
                    except json.JSONDecodeError:
                        tool_input = {}
                    summary = _format_tool_summary(current_tool, tool_input)
                    print(f"    [{label}] {summary}")
                    in_tool = False
                    current_tool = None
                    tool_input_chunks = ""

            elif event_type == "message_start":
                turn += 1
                if turn % 5 == 0:
                    print(f"    [{label}] ... turn {turn}")

        elif isinstance(message, ResultMessage):
            result = message.result if hasattr(message, "result") else None

        # Fallback for non-streaming message types
        elif isinstance(message, dict) and "result" in message:
            result = message["result"]
        elif hasattr(message, "result") and getattr(message, "result", None):
            result = message.result

    print(f"    [{label}] Done ({turn} turns)")
    return result


async def cargo_gate() -> bool:
    """Run cargo build + test + clippy. Returns True if all pass."""
    result = await run_agent(
        prompt=(
            "Run these commands in order and report pass/fail for each:\n"
            "1. cargo build 2>&1\n"
            "2. cargo test 2>&1\n"
            "3. cargo clippy -- -D warnings 2>&1\n"
            "If any fail, show the error output."
        ),
        system_prompt="You are a CI gate. Run the commands and report results concisely.",
        tools=GATE_TOOLS,
        label="Gate",
    )
    if result:
        print(f"    Gate result: {result[:200]}")
    # Check for actual failure indicators.
    # Must not match passing output like "0 failed", "test result: ok. 260 passed; 0 failed"
    lower = (result or "").lower()
    # Strip known passing patterns before checking for failure words
    sanitized = lower
    for safe in ["0 failed", "0 failures", "test result: ok"]:
        sanitized = sanitized.replace(safe, "")
    has_failure = any(phrase in sanitized for phrase in [
        "❌", "failed", "failure", "error[", "not pass", "could not compile",
    ])
    return result is not None and not has_failure


async def build_stage(stage: dict) -> None:
    """Run the 3-agent pipeline for one stage."""
    name = stage["name"]
    print(f"\n{'='*60}")
    print(f"  STAGE: {name}")
    print(f"{'='*60}")

    t0 = time.time()

    # Agent 1: DeFi
    print(f"\n  [1/3] DeFi Agent — writing code...")
    defi_result = await run_agent(
        stage["defi"], DEFI_SYSTEM_PROMPT, DEFI_TOOLS, label="DeFi",
    )
    print(f"    Elapsed: {time.time() - t0:.0f}s")

    # Agent 2: Quality
    t1 = time.time()
    print(f"\n  [2/3] Quality Agent — reviewing...")
    quality_result = await run_agent(
        stage["quality"], QUALITY_SYSTEM_PROMPT, QUALITY_TOOLS,
        extra_dirs=QUALITY_EXTRA, label="Quality",
    )
    print(f"    Elapsed: {time.time() - t1:.0f}s")

    # Agent 3: Performance (optional, gets more turns for profiling cycles)
    if stage.get("perf"):
        t2 = time.time()
        print(f"\n  [3/3] Performance Agent — optimizing...")
        perf_result = await run_agent(
            stage["perf"], PERF_SYSTEM_PROMPT, PERF_TOOLS,
            extra_dirs=PERF_EXTRA, label="Perf", max_turns=120,
        )
        print(f"    Elapsed: {time.time() - t2:.0f}s")

    # Gate
    print(f"\n  Gate: cargo build + test + clippy...")
    passed = await cargo_gate()
    elapsed = time.time() - t0
    status = "PASSED" if passed else "FAILED"
    print(f"\n  Stage {name}: {status} ({elapsed:.0f}s total)")

    if not passed:
        print(f"\n  WARNING: Stage {name} gate failed. Continuing anyway...")


async def main():
    # Parse optional --stage flag to run a single stage
    target_stage = None
    if "--stage" in sys.argv:
        idx = sys.argv.index("--stage")
        if idx + 1 < len(sys.argv):
            target_stage = sys.argv[idx + 1]

    print("PerpCity Rust SDK — Multi-Agent Builder")
    print(f"  SDK dir: {SDK_DIR}")
    print(f"  Stages: {[s['name'] for s in STAGES]}")
    if target_stage:
        print(f"  Running single stage: {target_stage}")

    t_start = time.time()

    for stage in STAGES:
        if target_stage and stage["name"].lower() != target_stage.lower():
            continue
        await build_stage(stage)

    elapsed = time.time() - t_start
    print(f"\n{'='*60}")
    print(f"  ALL STAGES COMPLETE ({elapsed:.0f}s total)")
    print(f"{'='*60}")


if __name__ == "__main__":
    asyncio.run(main())
