//! Read-only testnet market sizing for directional maker/taker convergence.

use std::{collections::HashMap, env};

use alloy::network::Ethereum;
use alloy::primitives::{Address, U256};
use alloy::providers::RootProvider;
use alloy::rpc::client::RpcClient;
use alloy::transports::BoxTransport;
use perpcity_sdk::{HftTransport, IBeacon, IPriceImpact, Perp, TransportConfig};
use serde::{Deserialize, Serialize};
use serde_json::json;

const DEFAULT_RPC_URL: &str = "https://sepolia-rollup.arbitrum.io/rpc";
const DEFAULT_GOLDSKY_URL: &str = "https://api.goldsky.com/api/public/project_cmbawn40q70fj01ws4jmsfj7f/subgraphs/perp-city/df503f4b-20260601_172038/gn";
const Q96: f64 = 79_228_162_514_264_337_593_543_950_336.0;
const TICK_SPACING: f64 = 30.0;
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

#[derive(Clone, Copy)]
struct Market<'a> {
    symbol: &'a str,
    perp: &'a str,
}

#[derive(Debug, Deserialize)]
struct GraphQlResponse {
    data: GraphQlData,
}

#[derive(Debug, Deserialize)]
struct GraphQlData {
    perps: Vec<GoldskyPerp>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoldskyPerp {
    id: String,
    symbol: String,
    ema_amm_price_x96: Option<String>,
    ema_index_price_x96: Option<String>,
}

#[derive(Clone)]
struct Emas {
    amm: U256,
    index: U256,
}

#[derive(Serialize)]
struct MarketSizing {
    symbol: String,
    perp: String,
    mark: f64,
    index: f64,
    direction: String,
    current_tick: i32,
    target_tick: i32,
    capacity_long: f64,
    capacity_short: f64,
    oi_long: f64,
    oi_short: f64,
    price_impact_lower: f64,
    price_impact_upper: f64,
    impact_target_price: f64,
    impact_target_tick: i32,
    full_range_lower: f64,
    full_range_upper: f64,
    tick_lower_full: i32,
    tick_upper_full: i32,
    min_margin_usdc_for_1e12_full: f64,
    min_margin_usdc_for_1e9_full: f64,
    min_margin_usdc_for_1e12_first_impact_step: f64,
    min_margin_usdc_for_1e9_first_impact_step: f64,
    full_gap_bps: f64,
    impact_step_gap_bps: f64,
    est_steps_at_current_impact: u32,
}

fn x96_to_price(v: U256) -> f64 {
    v.to_string().parse::<f64>().unwrap_or(0.0) / Q96
}

fn sqrt_x96_to_price(v: U256) -> f64 {
    let s = x96_to_price(v);
    s * s
}

fn tick(p: f64) -> i32 {
    (p.ln() / 1.0001_f64.ln()).round() as i32
}

fn align_down(t: i32) -> i32 {
    ((t as f64 / TICK_SPACING).floor() * TICK_SPACING) as i32
}

fn align_up(t: i32) -> i32 {
    ((t as f64 / TICK_SPACING).ceil() * TICK_SPACING) as i32
}

fn margin_for_delta_range_usdc(delta_perp: f64, from: f64, to: f64) -> f64 {
    if delta_perp <= 0.0 || from <= 0.0 || to <= 0.0 || (from - to).abs() < f64::EPSILON {
        return 0.0;
    }
    let a = from.sqrt().min(to.sqrt());
    let b = from.sqrt().max(to.sqrt());
    let raw_delta = delta_perp * 1e18;
    let liquidity = raw_delta * (a * b) / (b - a);
    liquidity * (b - a) / 1e6
}

fn gap_bps(from: f64, to: f64) -> f64 {
    ((to / from) - 1.0).abs() * 10_000.0
}

fn parse_u256_dec(v: &Option<String>, fallback: U256) -> U256 {
    v.as_ref()
        .and_then(|s| U256::from_str_radix(s, 10).ok())
        .unwrap_or(fallback)
}

async fn fetch_emas(url: &str) -> Result<HashMap<String, Emas>, Box<dyn std::error::Error>> {
    let query = r#"query Q { perps(first: 1000) { id symbol emaAmmPriceX96 emaIndexPriceX96 } }"#;
    let resp: GraphQlResponse = reqwest::Client::new()
        .post(url)
        .json(&json!({"query": query}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut map = HashMap::new();
    for p in resp.data.perps {
        let zero = U256::ZERO;
        map.insert(
            p.symbol.clone(),
            Emas {
                amm: parse_u256_dec(&p.ema_amm_price_x96, zero),
                index: parse_u256_dec(&p.ema_index_price_x96, zero),
            },
        );
        map.entry(p.id).or_insert(Emas {
            amm: zero,
            index: zero,
        });
    }
    Ok(map)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc_url = env::var("PERPCITY_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());
    let goldsky_url = env::var("GOLDSKY_URL").unwrap_or_else(|_| DEFAULT_GOLDSKY_URL.to_string());
    let emas_by_symbol = fetch_emas(&goldsky_url).await.unwrap_or_default();
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&rpc_url)
            .build()?,
    )?;
    let rpc_client = RpcClient::new(BoxTransport::new(transport), false);
    let provider: RootProvider<Ethereum> = RootProvider::new(rpc_client);

    // Generic convergence sizing must never include the Hormuz mirror markets:
    // - HRMZ-TT mirrors mainnet tonnage
    // - HRMZ-CT mirrors mainnet cargo count
    // They require exact mainnet replay manifests instead of heuristic convergence.
    let markets = [
        Market {
            symbol: "MCNT-OAK",
            perp: "0x07d340e33cb6b73ddab3a53f4af971921bb55e49",
        },
        Market {
            symbol: "MCNT-RDP",
            perp: "0xe149d179751b57c3b1f8d06c1b58d1ec7968366a",
        },
        Market {
            symbol: "MCNT-HOU",
            perp: "0x2975ff1307408a3c93606e517f9e02176d2ee289",
        },
        Market {
            symbol: "CITI-NYC",
            perp: "0x6d4051ffb71f391a5b4d8643a29ec6f66f67df50",
        },
    ];

    let mut out = Vec::new();
    for m in markets {
        if is_hormuz_mirror_market(m.symbol, m.perp) {
            continue;
        }
        let perp_addr: Address = m.perp.parse()?;
        let perp = Perp::new(perp_addr, &provider);
        let modules = perp.modules().call().await?;
        let pool = perp.poolState().call().await?;
        let cap = perp.capacity().call().await?;
        let oi = perp.openInterest().call().await?;
        let beacon = IBeacon::new(modules.beacon, &provider);
        let index_raw: U256 = beacon.index().call().await?;
        let emas = emas_by_symbol.get(m.symbol).cloned().unwrap_or(Emas {
            amm: pool.ammPrice,
            index: index_raw,
        });
        let ema_amm = if emas.amm.is_zero() {
            pool.ammPrice
        } else {
            emas.amm
        };
        let ema_index = if emas.index.is_zero() {
            index_raw
        } else {
            emas.index
        };
        let pi = IPriceImpact::new(modules.priceImpact, &provider);
        let bounds = pi
            .sqrtPriceBounds(pool.ammPrice, index_raw, ema_amm, ema_index)
            .call()
            .await?;

        let mark = x96_to_price(pool.ammPrice);
        let index = x96_to_price(index_raw);
        let lower = sqrt_x96_to_price(bounds.sqrtMin);
        let upper = sqrt_x96_to_price(bounds.sqrtMax);
        let direction = if index > mark { "LONG" } else { "SHORT" };
        let impact_target = if direction == "LONG" {
            index.min(upper)
        } else {
            index.max(lower)
        };
        let full_low = mark.min(index);
        let full_up = mark.max(index);
        let first_low = mark.min(impact_target);
        let first_up = mark.max(impact_target);
        let full_gap = gap_bps(mark, index);
        let step_gap = gap_bps(mark, impact_target);
        let steps = if step_gap <= 0.0 {
            0
        } else {
            (full_gap / step_gap).ceil() as u32
        };

        out.push(MarketSizing {
            symbol: m.symbol.to_string(),
            perp: m.perp.to_string(),
            mark,
            index,
            direction: direction.to_string(),
            current_tick: pool.tick.try_into().unwrap_or(0),
            target_tick: tick(index),
            capacity_long: cap.long as f64 / 1e18,
            capacity_short: cap.short as f64 / 1e18,
            oi_long: oi.long as f64 / 1e18,
            oi_short: oi.short as f64 / 1e18,
            price_impact_lower: lower,
            price_impact_upper: upper,
            impact_target_price: impact_target,
            impact_target_tick: tick(impact_target),
            full_range_lower: full_low,
            full_range_upper: full_up,
            tick_lower_full: align_down(tick(full_low)),
            tick_upper_full: align_up(tick(full_up)),
            min_margin_usdc_for_1e12_full: margin_for_delta_range_usdc(1e-12, full_low, full_up),
            min_margin_usdc_for_1e9_full: margin_for_delta_range_usdc(1e-9, full_low, full_up),
            min_margin_usdc_for_1e12_first_impact_step: margin_for_delta_range_usdc(
                1e-12, first_low, first_up,
            ),
            min_margin_usdc_for_1e9_first_impact_step: margin_for_delta_range_usdc(
                1e-9, first_low, first_up,
            ),
            full_gap_bps: full_gap,
            impact_step_gap_bps: step_gap,
            est_steps_at_current_impact: steps,
        });
    }

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
