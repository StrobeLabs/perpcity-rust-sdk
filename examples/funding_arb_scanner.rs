//! Dry-run funding-arb scanner for Perp City Arbitrum Sepolia markets.
//!
//! Read-only: queries public Goldsky, validates actionable/pathological markets
//! via public RPC `eth_call`, ranks convergence opportunities, and prints JSON.
//! It does not create/load a signer, read private keys, approve allowances, or
//! send transactions.

use std::env;

use alloy::network::Ethereum;
use alloy::primitives::{Address, U256};
use alloy::providers::RootProvider;
use alloy::rpc::client::RpcClient;
use alloy::transports::BoxTransport;
use perpcity_sdk::{HftTransport, IBeacon, Perp, TransportConfig};
use serde::{Deserialize, Serialize};
use serde_json::json;

const DEFAULT_GOLDSKY_URL: &str = "https://api.goldsky.com/api/public/project_cmbawn40q70fj01ws4jmsfj7f/subgraphs/perp-city/df503f4b-20260601_172038/gn";
const DEFAULT_RPC_URL: &str = "https://sepolia-rollup.arbitrum.io/rpc";
const CHAIN_ID: u64 = 421_614;
const Q96_F64: f64 = 79_228_162_514_264_337_593_543_950_336.0;
const MIN_DIVERGENCE_BPS: f64 = 25.0;
// Perp capacity/open-interest fields are amount0/perp units, 18 decimals.
// Do not decode them as USDC 1e6 values; that overstates taker size by 1e12.
const MIN_SIDE_CAPACITY: f64 = 0.000000000000001;
const DEFAULT_MARGIN_USDC: f64 = 5.0;
const DEFAULT_MAX_PERP_DELTA: f64 = 0.000000000001;
const PATHOLOGICAL_INDEX_THRESHOLD: f64 = 1e-9;
const RPC_MISMATCH_BPS: f64 = 5.0;
const HORMUZ_MIRROR_SYMBOLS: [&str; 2] = ["HRMZ-TT", "HRMZ-CT"];
const HORMUZ_MIRROR_PERPS: [&str; 2] = [
    "0xa77de9df6e08beb8f523153dd0110465190526e3",
    "0xd802ff15c9d828390dc155ba3908fcbe0e868e62",
];

fn is_hormuz_mirror_market(symbol: &str, perp_id: &str) -> bool {
    HORMUZ_MIRROR_SYMBOLS
        .iter()
        .any(|blocked| symbol.eq_ignore_ascii_case(blocked))
        || HORMUZ_MIRROR_PERPS
            .iter()
            .any(|blocked| perp_id.eq_ignore_ascii_case(blocked))
}

type ReadOnlyProvider = RootProvider<Ethereum>;

#[derive(Debug, Deserialize)]
struct GraphQlResponse {
    data: Option<GraphQlData>,
    errors: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GraphQlData {
    #[serde(rename = "_meta")]
    meta: Option<SubgraphMeta>,
    perps: Vec<GoldskyPerp>,
}

#[derive(Debug, Deserialize)]
struct SubgraphMeta {
    block: SubgraphBlock,
}

#[derive(Debug, Deserialize)]
struct SubgraphBlock {
    number: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoldskyPerp {
    id: String,
    symbol: Option<String>,
    name: Option<String>,
    liquidity: String,
    capacity_long: String,
    capacity_short: String,
    open_interest_long: String,
    open_interest_short: String,
    sqrt_price_x96: String,
    ema_amm_price_x96: Option<String>,
    ema_index_price_x96: Option<String>,
    funding_per_day: String,
    beacon: Option<GoldskyBeacon>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoldskyBeacon {
    id: String,
    index_x96: String,
}

#[derive(Debug, Serialize)]
struct ScannerOutput {
    dry_run: bool,
    chain_id: u64,
    rpc_url: String,
    goldsky_url: String,
    goldsky_block: Option<u64>,
    market_count: usize,
    actionable_count: usize,
    skipped_count: usize,
    actions: Vec<ActionDecision>,
    no_action: Vec<NoActionDecision>,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ActionDecision {
    rank: usize,
    symbol: String,
    name: Option<String>,
    perp: String,
    beacon: String,
    mark_price: f64,
    index_price: f64,
    divergence_bps: f64,
    funding_rate_daily: f64,
    direction: Direction,
    perp_delta: f64,
    margin_usdc: f64,
    available_side_capacity: f64,
    liquidity: String,
    open_interest_long: f64,
    open_interest_short: f64,
    open_taker_plan: OpenTakerPlan,
    rpc_validation: Option<RpcValidation>,
    reason: String,
    will_send_tx: bool,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NoActionDecision {
    symbol: String,
    name: Option<String>,
    perp: String,
    reason: String,
    liquidity: String,
    mark_price: Option<f64>,
    index_price: Option<f64>,
    rpc_validation: Option<RpcValidation>,
}

#[derive(Debug, Serialize)]
struct OpenTakerPlan {
    holder_required: bool,
    margin_usdc: f64,
    perp_delta: f64,
    amt1_limit: u128,
    ready_for_live_tx: bool,
    next_gate: String,
}

#[derive(Debug, Clone, Serialize)]
struct RpcValidation {
    source: String,
    perp: String,
    beacon: String,
    mark_price: f64,
    index_price: f64,
    divergence_bps: f64,
    funding_rate_daily: f64,
    capacity_long: f64,
    capacity_short: f64,
    open_interest_long: f64,
    open_interest_short: f64,
    direction: Option<Direction>,
    validation_warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum Direction {
    Long,
    Short,
}

impl Direction {
    fn signed_delta(self, magnitude: f64) -> f64 {
        match self {
            Direction::Long => magnitude,
            Direction::Short => -magnitude,
        }
    }
}

fn parse_decimal_f64(raw: &str, field: &str) -> Result<f64, String> {
    raw.parse::<f64>()
        .map_err(|err| format!("failed to parse {field}={raw}: {err}"))
}

fn u256_price_x96_to_f64(value: U256) -> Result<f64, String> {
    decode_price_x96(&value.to_string())
}

fn decode_price_x96(raw: &str) -> Result<f64, String> {
    let value = parse_decimal_f64(raw, "price_x96")?;
    if !value.is_finite() || value <= 0.0 {
        return Err(format!("invalid X96 price value: {raw}"));
    }
    Ok(value / Q96_F64)
}

fn decode_sqrt_price_x96(raw: &str) -> Result<f64, String> {
    let sqrt_price = decode_price_x96(raw)?;
    Ok(sqrt_price * sqrt_price)
}

fn scaled_1e18(raw: &str, field: &str) -> Result<f64, String> {
    Ok(parse_decimal_f64(raw, field)? / 1_000_000_000_000_000_000.0)
}

fn signed_1e18_to_f64(value: alloy::primitives::Signed<88, 2>) -> f64 {
    i128::try_from(value).unwrap_or(0) as f64 / 1_000_000_000_000_000_000.0
}

fn choose_direction(mark: f64, index: f64) -> Option<Direction> {
    if mark > index {
        Some(Direction::Short)
    } else if mark < index {
        Some(Direction::Long)
    } else {
        None
    }
}

fn divergence_bps(mark: f64, index: f64) -> f64 {
    ((mark / index) - 1.0).abs() * 10_000.0
}

fn side_capacity(perp: &GoldskyPerp, direction: Direction) -> Result<f64, String> {
    match direction {
        Direction::Long => scaled_1e18(&perp.capacity_long, "capacityLong"),
        Direction::Short => scaled_1e18(&perp.capacity_short, "capacityShort"),
    }
}

fn proposed_delta_abs(available_side_capacity: f64) -> f64 {
    available_side_capacity.min(DEFAULT_MAX_PERP_DELTA).max(0.0)
}

fn open_taker_plan(perp_delta: f64) -> OpenTakerPlan {
    OpenTakerPlan {
        holder_required: true,
        margin_usdc: DEFAULT_MARGIN_USDC,
        perp_delta,
        amt1_limit: 0,
        ready_for_live_tx: false,
        next_gate:
            "testnet wallet, balances, allowance, slippage limit, and eth_call simulation approval"
                .to_string(),
    }
}

fn decision_for_market(
    perp: GoldskyPerp,
) -> Result<Result<ActionDecision, NoActionDecision>, String> {
    let symbol = perp.symbol.clone().unwrap_or_else(|| perp.id.clone());
    let liquidity_raw = perp.liquidity.clone();
    let liquidity = parse_decimal_f64(&perp.liquidity, "liquidity")?;
    let mark = decode_sqrt_price_x96(&perp.sqrt_price_x96).ok();

    if is_hormuz_mirror_market(&symbol, &perp.id) {
        return Ok(Err(NoActionDecision {
            symbol,
            name: perp.name,
            perp: perp.id,
            reason: "hormuz_mirror_excluded_from_generic_convergence".to_string(),
            liquidity: liquidity_raw,
            mark_price: mark,
            index_price: perp
                .beacon
                .as_ref()
                .and_then(|beacon| decode_price_x96(&beacon.index_x96).ok()),
            rpc_validation: None,
        }));
    }

    let beacon = match perp.beacon.as_ref() {
        Some(beacon) => beacon,
        None => {
            return Ok(Err(NoActionDecision {
                symbol,
                name: perp.name,
                perp: perp.id,
                reason: "missing_beacon".to_string(),
                liquidity: liquidity_raw,
                mark_price: mark,
                index_price: None,
                rpc_validation: None,
            }));
        }
    };
    let index = decode_price_x96(&beacon.index_x96).ok();

    if liquidity <= 0.0 {
        return Ok(Err(NoActionDecision {
            symbol,
            name: perp.name,
            perp: perp.id,
            reason: "zero_liquidity".to_string(),
            liquidity: liquidity_raw,
            mark_price: mark,
            index_price: index,
            rpc_validation: None,
        }));
    }

    let (Some(mark), Some(index)) = (mark, index) else {
        return Ok(Err(NoActionDecision {
            symbol,
            name: perp.name,
            perp: perp.id,
            reason: "decode_error".to_string(),
            liquidity: liquidity_raw,
            mark_price: mark,
            index_price: index,
            rpc_validation: None,
        }));
    };

    if index <= PATHOLOGICAL_INDEX_THRESHOLD || !index.is_finite() {
        return Ok(Err(NoActionDecision {
            symbol,
            name: perp.name,
            perp: perp.id,
            reason: "pathological_index".to_string(),
            liquidity: liquidity_raw,
            mark_price: Some(mark),
            index_price: Some(index),
            rpc_validation: None,
        }));
    }

    let Some(direction) = choose_direction(mark, index) else {
        return Ok(Err(NoActionDecision {
            symbol,
            name: perp.name,
            perp: perp.id,
            reason: "mark_equals_index".to_string(),
            liquidity: liquidity_raw,
            mark_price: Some(mark),
            index_price: Some(index),
            rpc_validation: None,
        }));
    };

    let divergence = divergence_bps(mark, index);
    if divergence < MIN_DIVERGENCE_BPS {
        return Ok(Err(NoActionDecision {
            symbol,
            name: perp.name,
            perp: perp.id,
            reason: "divergence_below_threshold".to_string(),
            liquidity: liquidity_raw,
            mark_price: Some(mark),
            index_price: Some(index),
            rpc_validation: None,
        }));
    }

    let capacity = side_capacity(&perp, direction)?;
    if capacity < MIN_SIDE_CAPACITY {
        return Ok(Err(NoActionDecision {
            symbol,
            name: perp.name,
            perp: perp.id,
            reason: "insufficient_side_capacity".to_string(),
            liquidity: liquidity_raw,
            mark_price: Some(mark),
            index_price: Some(index),
            rpc_validation: None,
        }));
    }

    let funding_rate_daily = scaled_1e18(&perp.funding_per_day, "fundingPerDay")?;
    let open_interest_long = scaled_1e18(&perp.open_interest_long, "openInterestLong")?;
    let open_interest_short = scaled_1e18(&perp.open_interest_short, "openInterestShort")?;
    let delta_abs = proposed_delta_abs(capacity);
    let perp_delta = direction.signed_delta(delta_abs);
    let mut warnings = Vec::new();

    if matches!(direction, Direction::Long) && funding_rate_daily > 0.0 {
        warnings.push("direction_converges_price_but_pays_positive_funding".to_string());
    } else if matches!(direction, Direction::Short) && funding_rate_daily < 0.0 {
        warnings.push("direction_converges_price_but_pays_negative_funding".to_string());
    }

    if perp.ema_amm_price_x96.is_none() || perp.ema_index_price_x96.is_none() {
        warnings.push("missing_ema_fields".to_string());
    }

    Ok(Ok(ActionDecision {
        rank: 0,
        symbol,
        name: perp.name,
        perp: perp.id,
        beacon: beacon.id.clone(),
        mark_price: mark,
        index_price: index,
        divergence_bps: divergence,
        funding_rate_daily,
        direction,
        perp_delta,
        margin_usdc: DEFAULT_MARGIN_USDC,
        available_side_capacity: capacity,
        liquidity: liquidity_raw,
        open_interest_long,
        open_interest_short,
        open_taker_plan: open_taker_plan(perp_delta),
        rpc_validation: None,
        reason: "mark_index_convergence_with_side_capacity".to_string(),
        will_send_tx: false,
        warnings,
    }))
}

fn read_only_provider(rpc_url: &str) -> Result<ReadOnlyProvider, Box<dyn std::error::Error>> {
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(rpc_url)
            .build()?,
    )?;
    let rpc_client = RpcClient::new(BoxTransport::new(transport), false);
    Ok(RootProvider::<Ethereum>::new(rpc_client))
}

async fn validate_onchain(
    provider: &ReadOnlyProvider,
    perp_addr: &str,
) -> Result<RpcValidation, String> {
    let perp_address: Address = perp_addr
        .parse()
        .map_err(|err| format!("invalid perp address {perp_addr}: {err}"))?;
    let perp = Perp::new(perp_address, provider);
    let modules = perp
        .modules()
        .call()
        .await
        .map_err(|err| format!("modules eth_call failed: {err}"))?;
    let pool_state = perp
        .poolState()
        .call()
        .await
        .map_err(|err| format!("poolState eth_call failed: {err}"))?;
    let rates = perp
        .rates()
        .call()
        .await
        .map_err(|err| format!("rates eth_call failed: {err}"))?;
    let capacity = perp
        .capacity()
        .call()
        .await
        .map_err(|err| format!("capacity eth_call failed: {err}"))?;
    let open_interest = perp
        .openInterest()
        .call()
        .await
        .map_err(|err| format!("openInterest eth_call failed: {err}"))?;
    let beacon = IBeacon::new(modules.beacon, provider);
    let index_x96: U256 = beacon
        .index()
        .call()
        .await
        .map_err(|err| format!("beacon.index eth_call failed: {err}"))?;

    let mark_price = u256_price_x96_to_f64(pool_state.ammPrice)?;
    let index_price = u256_price_x96_to_f64(index_x96)?;
    let direction = if index_price > PATHOLOGICAL_INDEX_THRESHOLD {
        choose_direction(mark_price, index_price)
    } else {
        None
    };

    let mut validation_warnings = Vec::new();
    if index_price <= PATHOLOGICAL_INDEX_THRESHOLD {
        validation_warnings.push("rpc_pathological_index".to_string());
    }
    if direction.is_none() {
        validation_warnings.push("rpc_no_direction".to_string());
    }

    Ok(RpcValidation {
        source: "public_rpc_eth_call".to_string(),
        perp: perp_addr.to_string(),
        beacon: modules.beacon.to_string(),
        mark_price,
        index_price,
        divergence_bps: if index_price > PATHOLOGICAL_INDEX_THRESHOLD {
            divergence_bps(mark_price, index_price)
        } else {
            f64::INFINITY
        },
        funding_rate_daily: signed_1e18_to_f64(rates.fundingPerDay),
        capacity_long: capacity.long as f64 / 1_000_000_000_000_000_000.0,
        capacity_short: capacity.short as f64 / 1_000_000_000_000_000_000.0,
        open_interest_long: open_interest.long as f64 / 1_000_000_000_000_000_000.0,
        open_interest_short: open_interest.short as f64 / 1_000_000_000_000_000_000.0,
        direction,
        validation_warnings,
    })
}

fn compare_rpc_action(action: &mut ActionDecision) {
    let Some(rpc) = action.rpc_validation.as_ref() else {
        return;
    };
    if let Some(rpc_direction) = rpc.direction {
        if rpc_direction != action.direction {
            action.warnings.push("rpc_direction_mismatch".to_string());
        }
    }
    if (rpc.divergence_bps - action.divergence_bps).abs() > RPC_MISMATCH_BPS {
        action.warnings.push("rpc_divergence_mismatch".to_string());
    }
    let rpc_capacity = match action.direction {
        Direction::Long => rpc.capacity_long,
        Direction::Short => rpc.capacity_short,
    };
    if rpc_capacity < action.perp_delta.abs() {
        action.warnings.push("rpc_capacity_below_plan".to_string());
        action.open_taker_plan.ready_for_live_tx = false;
    }
}

async fn fetch_markets(goldsky_url: &str) -> Result<GraphQlData, Box<dyn std::error::Error>> {
    let query = r#"
        query FundingArbScannerMarkets {
          _meta { block { number } }
          perps(first: 1000) {
            id
            symbol
            name
            liquidity
            capacityLong
            capacityShort
            openInterestLong
            openInterestShort
            sqrtPriceX96
            emaAmmPriceX96
            emaIndexPriceX96
            fundingPerDay
            beacon { id indexX96 }
          }
        }
    "#;

    let client = reqwest::Client::builder()
        .user_agent("perpcity-funding-arb-scanner/0.2 dry-run")
        .build()?;
    let response = client
        .post(goldsky_url)
        .json(&json!({ "query": query }))
        .send()
        .await?
        .error_for_status()?;
    let body: GraphQlResponse = response.json().await?;
    if let Some(errors) = body.errors {
        return Err(format!("Goldsky GraphQL errors: {errors}").into());
    }
    body.data
        .ok_or_else(|| "Goldsky response missing data".into())
}

async fn build_output(
    rpc_url: String,
    goldsky_url: String,
    goldsky_block: Option<u64>,
    perps: Vec<GoldskyPerp>,
) -> ScannerOutput {
    let market_count = perps.len();
    let mut actions = Vec::new();
    let mut no_action = Vec::new();
    let mut warnings = Vec::new();

    for perp in perps {
        match decision_for_market(perp) {
            Ok(Ok(action)) => actions.push(action),
            Ok(Err(skip)) => no_action.push(skip),
            Err(err) => warnings.push(err),
        }
    }

    actions.sort_by(|a, b| {
        b.divergence_bps
            .partial_cmp(&a.divergence_bps)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for (idx, action) in actions.iter_mut().enumerate() {
        action.rank = idx + 1;
    }
    no_action.sort_by(|a, b| a.symbol.cmp(&b.symbol));

    match read_only_provider(&rpc_url) {
        Ok(provider) => {
            for action in &mut actions {
                match validate_onchain(&provider, &action.perp).await {
                    Ok(validation) => {
                        action.rpc_validation = Some(validation);
                        compare_rpc_action(action);
                    }
                    Err(err) => {
                        warnings.push(format!("rpc_validation_failed:{}:{err}", action.symbol))
                    }
                }
            }
            for skip in &mut no_action {
                if skip.reason == "pathological_index" {
                    match validate_onchain(&provider, &skip.perp).await {
                        Ok(validation) => skip.rpc_validation = Some(validation),
                        Err(err) => {
                            warnings.push(format!("rpc_validation_failed:{}:{err}", skip.symbol))
                        }
                    }
                }
            }
        }
        Err(err) => warnings.push(format!("rpc_provider_init_failed:{err}")),
    }

    ScannerOutput {
        dry_run: true,
        chain_id: CHAIN_ID,
        rpc_url,
        goldsky_url,
        goldsky_block,
        market_count,
        actionable_count: actions.len(),
        skipped_count: no_action.len(),
        actions,
        no_action,
        warnings,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc_url = env::var("PERPCITY_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());
    let goldsky_url = env::var("GOLDSKY_URL").unwrap_or_else(|_| DEFAULT_GOLDSKY_URL.to_string());

    let data = fetch_markets(&goldsky_url).await?;
    let goldsky_block = data.meta.map(|meta| meta.block.number);
    let output = build_output(rpc_url, goldsky_url, goldsky_block, data.perps).await;

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_x96_decodes_unit_price() {
        assert!((decode_price_x96("79228162514264337593543950336").unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn sqrt_price_x96_decodes_squared_price() {
        let q96_times_two = "158456325028528675187087900672";
        assert!((decode_sqrt_price_x96(q96_times_two).unwrap() - 4.0).abs() < 1e-12);
    }

    #[test]
    fn direction_converges_mark_to_index() {
        assert_eq!(choose_direction(120.0, 100.0), Some(Direction::Short));
        assert_eq!(choose_direction(80.0, 100.0), Some(Direction::Long));
        assert_eq!(choose_direction(100.0, 100.0), None);
    }

    #[test]
    fn proposed_delta_is_capped_by_capacity_and_default_size() {
        assert_eq!(proposed_delta_abs(0.0004), 0.0004);
        assert_eq!(proposed_delta_abs(5.0), DEFAULT_MAX_PERP_DELTA);
    }
}
