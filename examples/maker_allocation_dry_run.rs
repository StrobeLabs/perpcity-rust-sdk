//! Dry-run maker liquidity allocation planner for Perp City Arbitrum Sepolia.
//!
//! Read-only: queries Goldsky, uses indexed beacon/perp snapshots to pick
//! conservative ranges, and prints JSON. It does not load a key or send txs.

use std::env;

use alloy::primitives::U256;
use perpcity_sdk::math::liquidity::liquidity_for_target_ratio;
use perpcity_sdk::{align_tick_down, align_tick_up, price_to_tick};
use serde::{Deserialize, Serialize};
use serde_json::json;

const DEFAULT_GOLDSKY_URL: &str = "https://api.goldsky.com/api/public/project_cmbawn40q70fj01ws4jmsfj7f/subgraphs/perp-city/df503f4b-20260601_172038/gn";
const CHAIN_ID: u64 = 421_614;
const Q96_F64: f64 = 79_228_162_514_264_337_593_543_950_336.0;
const TICK_SPACING: i32 = 60;
const DEFAULT_MARGIN_USDC: f64 = 100.0;
const DEFAULT_MAX_MARKETS: usize = 10;
const TARGET_MARGIN_RATIO: f64 = 0.20;
const PATHOLOGICAL_INDEX_THRESHOLD: f64 = 1e-9;
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

#[allow(dead_code)]
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
    funding_per_day: String,
    last_touched_timestamp: String,
    beacon: Option<GoldskyBeacon>,
    snapshots: Vec<PerpSnapshot>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoldskyBeacon {
    id: String,
    index_x96: String,
    snapshots: Vec<BeaconSnapshot>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BeaconSnapshot {
    timestamp: String,
    index_x96: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PerpSnapshot {
    timestamp: String,
    amm_price_x96: String,
}

#[derive(Debug, Serialize)]
struct Output {
    dry_run: bool,
    chain_id: u64,
    goldsky_block: Option<u64>,
    market_count: usize,
    selected_count: usize,
    default_margin_usdc: f64,
    selected_markets: Vec<MakerAllocation>,
    market_status: Vec<MarketStatus>,
    limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct MarketStatus {
    symbol: String,
    name: Option<String>,
    perp: String,
    beacon: Option<String>,
    liquidity_raw: String,
    liquidity: f64,
    capacity_long: f64,
    capacity_short: f64,
    open_interest_long: f64,
    open_interest_short: f64,
    funding_rate_daily: f64,
    mark_price: Option<f64>,
    index_price: Option<f64>,
    divergence_bps: Option<f64>,
    history_points: usize,
    history_volatility_pct: Option<f64>,
    reason_codes: Vec<String>,
    maker_priority_score: f64,
}

#[derive(Debug, Serialize)]
struct MakerAllocation {
    rank: usize,
    symbol: String,
    perp: String,
    margin_usdc: f64,
    center_price: f64,
    width_pct: f64,
    price_lower: f64,
    price_upper: f64,
    tick_lower: i32,
    tick_upper: i32,
    liquidity: String,
    max_amt0_in: u128,
    max_amt1_in: u128,
    expected_effect: String,
    will_send_tx: bool,
    reason_codes: Vec<String>,
}

fn parse_f64(raw: &str) -> Option<f64> {
    raw.parse::<f64>().ok()
}

fn price_x96(raw: &str) -> Option<f64> {
    let v = parse_f64(raw)?;
    if v > 0.0 { Some(v / Q96_F64) } else { None }
}

fn sqrt_price_x96(raw: &str) -> Option<f64> {
    let s = price_x96(raw)?;
    Some(s * s)
}

fn scaled(raw: &str, scale: f64) -> f64 {
    parse_f64(raw).unwrap_or(0.0) / scale
}

fn divergence_bps(mark: f64, index: f64) -> f64 {
    ((mark / index) - 1.0).abs() * 10_000.0
}

fn history_stats(beacon: Option<&GoldskyBeacon>) -> (usize, Option<f64>) {
    let Some(beacon) = beacon else {
        return (0, None);
    };
    let mut prices: Vec<f64> = beacon
        .snapshots
        .iter()
        .filter_map(|s| price_x96(&s.index_x96))
        .filter(|p| p.is_finite() && *p > PATHOLOGICAL_INDEX_THRESHOLD)
        .collect();
    prices.reverse();
    if prices.len() < 2 {
        return (prices.len(), None);
    }
    let returns: Vec<f64> = prices
        .windows(2)
        .filter_map(|w| {
            if w[0] > 0.0 {
                Some((w[1] / w[0]).ln())
            } else {
                None
            }
        })
        .collect();
    if returns.is_empty() {
        return (prices.len(), None);
    }
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / returns.len() as f64;
    (prices.len(), Some(var.sqrt().abs()))
}

fn status_for(perp: GoldskyPerp) -> MarketStatus {
    let symbol = perp.symbol.clone().unwrap_or_else(|| perp.id.clone());
    let liquidity = scaled(&perp.liquidity, 1.0);
    let mark = sqrt_price_x96(&perp.sqrt_price_x96);
    let index = perp.beacon.as_ref().and_then(|b| price_x96(&b.index_x96));
    let divergence = match (mark, index) {
        (Some(m), Some(i)) if i > PATHOLOGICAL_INDEX_THRESHOLD => Some(divergence_bps(m, i)),
        _ => None,
    };
    let (history_points, history_volatility_pct) = history_stats(perp.beacon.as_ref());
    let mut reasons = Vec::new();
    if is_hormuz_mirror_market(&symbol, &perp.id) {
        reasons.push("hormuz_mirror_excluded_from_generic_convergence".to_string());
    }
    if liquidity <= 0.0 {
        reasons.push("zero_liquidity".to_string());
    }
    if scaled(&perp.capacity_long, 1_000_000.0) < 0.001
        || scaled(&perp.capacity_short, 1_000_000.0) < 0.001
    {
        reasons.push("low_capacity".to_string());
    }
    if index.is_none_or(|i| i <= PATHOLOGICAL_INDEX_THRESHOLD || !i.is_finite()) {
        reasons.push("pathological_index".to_string());
    }
    if history_points < 2 {
        reasons.push("stale_oracle".to_string());
    }
    if divergence.is_some_and(|d| d > 1_000.0) {
        reasons.push("high_divergence".to_string());
    }
    if liquidity > 0.0 && !reasons.iter().any(|r| r == "pathological_index") {
        reasons.push("active_candidate".to_string());
    }
    let mut score = 0.0;
    if liquidity <= 0.0 {
        score += 100.0;
    }
    if !reasons.iter().any(|r| r == "pathological_index") {
        score += 50.0;
    }
    if history_points >= 2 {
        score += 25.0;
    }
    if let Some(d) = divergence {
        score += (d / 100.0).min(25.0);
    }

    MarketStatus {
        symbol,
        name: perp.name,
        perp: perp.id,
        beacon: perp.beacon.map(|b| b.id),
        liquidity_raw: perp.liquidity,
        liquidity,
        capacity_long: scaled(&perp.capacity_long, 1_000_000.0),
        capacity_short: scaled(&perp.capacity_short, 1_000_000.0),
        open_interest_long: scaled(&perp.open_interest_long, 1_000_000.0),
        open_interest_short: scaled(&perp.open_interest_short, 1_000_000.0),
        funding_rate_daily: scaled(&perp.funding_per_day, 1_000_000_000_000_000_000.0),
        mark_price: mark,
        index_price: index,
        divergence_bps: divergence,
        history_points,
        history_volatility_pct,
        reason_codes: reasons,
        maker_priority_score: score,
    }
}

fn allocation_for(rank: usize, status: &MarketStatus, margin_usdc: f64) -> Option<MakerAllocation> {
    if status
        .reason_codes
        .iter()
        .any(|r| r == "hormuz_mirror_excluded_from_generic_convergence")
    {
        return None;
    }
    if status
        .reason_codes
        .iter()
        .any(|r| r == "pathological_index")
    {
        return None;
    }
    let center = status.index_price.or(status.mark_price)?;
    if center <= PATHOLOGICAL_INDEX_THRESHOLD || !center.is_finite() {
        return None;
    }
    let vol = status.history_volatility_pct.unwrap_or(0.0);
    let width_pct = (0.20_f64.max(vol * 6.0)).min(0.75);
    let price_lower = center * (1.0 - width_pct);
    let price_upper = center * (1.0 + width_pct);
    let tick_lower = align_tick_down(price_to_tick(price_lower).ok()?, TICK_SPACING);
    let tick_upper = align_tick_up(price_to_tick(price_upper).ok()?, TICK_SPACING);
    let current_sqrt =
        U256::from_str_radix(&format!("{:x}", (center.sqrt() * Q96_F64) as u128), 16).ok()?;
    let liq = liquidity_for_target_ratio(
        (margin_usdc * 1_000_000.0) as u128,
        tick_lower,
        tick_upper,
        current_sqrt,
        TARGET_MARGIN_RATIO,
    )
    .ok()?;
    Some(MakerAllocation {
        rank,
        symbol: status.symbol.clone(),
        perp: status.perp.clone(),
        margin_usdc,
        center_price: center,
        width_pct,
        price_lower,
        price_upper,
        tick_lower,
        tick_upper,
        liquidity: liq.to_string(),
        max_amt0_in: u128::MAX,
        max_amt1_in: u128::MAX,
        expected_effect: "adds maker capacity around current index; dry-run only, requires wallet USDC approval before live".to_string(),
        will_send_tx: false,
        reason_codes: status.reason_codes.clone(),
    })
}

async fn fetch(goldsky_url: &str) -> Result<GraphQlData, Box<dyn std::error::Error>> {
    let query = r#"
      query MakerAllocationMarkets {
        _meta { block { number } }
        perps(first: 1000) {
          id symbol name liquidity capacityLong capacityShort openInterestLong openInterestShort
          sqrtPriceX96 fundingPerDay lastTouchedTimestamp
          beacon { id indexX96 snapshots(first: 12, orderBy: timestamp, orderDirection: desc) { timestamp indexX96 } }
          snapshots(first: 3, orderBy: timestamp, orderDirection: desc) { timestamp ammPriceX96 }
        }
      }
    "#;
    let client = reqwest::Client::builder()
        .user_agent("perpcity-maker-allocation-dry-run/0.1")
        .build()?;
    let body: GraphQlResponse = client
        .post(goldsky_url)
        .json(&json!({"query": query}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if let Some(errors) = body.errors {
        return Err(format!("Goldsky errors: {errors}").into());
    }
    body.data.ok_or_else(|| "Goldsky missing data".into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let goldsky_url = env::var("GOLDSKY_URL").unwrap_or_else(|_| DEFAULT_GOLDSKY_URL.to_string());
    let margin_usdc = env::var("MAKER_MARGIN_USDC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MARGIN_USDC);
    let max_markets = env::var("MAKER_MAX_MARKETS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_MARKETS);
    let data = fetch(&goldsky_url).await?;
    let mut statuses: Vec<MarketStatus> = data.perps.into_iter().map(status_for).collect();
    statuses.sort_by(|a, b| {
        b.maker_priority_score
            .partial_cmp(&a.maker_priority_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let selected: Vec<MakerAllocation> = statuses
        .iter()
        .filter(|s| {
            !s.reason_codes
                .iter()
                .any(|r| r == "hormuz_mirror_excluded_from_generic_convergence")
                && (s.reason_codes.iter().any(|r| r == "zero_liquidity")
                    || s.liquidity < 1_000_000.0)
        })
        .filter_map(|s| allocation_for(0, s, margin_usdc))
        .take(max_markets)
        .enumerate()
        .map(|(i, mut a)| {
            a.rank = i + 1;
            a
        })
        .collect();
    let out = Output {
        dry_run: true,
        chain_id: CHAIN_ID,
        goldsky_block: data.meta.map(|m| m.block.number),
        market_count: statuses.len(),
        selected_count: selected.len(),
        default_margin_usdc: margin_usdc,
        selected_markets: selected,
        market_status: statuses,
        limitations: vec![
            "range width uses latest indexed beacon snapshots when available; otherwise conservative 20% default".to_string(),
            "dry-run only: no key, no approvals, no maker tx sent".to_string(),
        ],
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
