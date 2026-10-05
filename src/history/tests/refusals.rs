//! What every reader refuses before any request goes out: a zero address,
//! a token query with no address set, a range the caller got backwards.

use super::*;

fn refused_config(error: PerpCityError) {
    assert!(
        matches!(
            error,
            PerpCityError::Validation(ValidationError::InvalidConfig { .. })
        ),
        "{error:?}"
    );
}

/// Every reader checks its addresses before it scans: a zero perp, beacon,
/// token or PoolManager, and a token query with no address set, are refused
/// with no request sent.
#[tokio::test]
async fn a_zero_address_is_refused_by_every_reader_before_any_request() {
    let node = FakeNode::new(Vec::new(), u64::MAX);
    let provider = node.provider();

    refused_config(
        market_events(&provider, Address::ZERO, 0, 10)
            .await
            .unwrap_err(),
    );
    refused_config(
        latest_market_events(&provider, Address::ZERO, 0, 10, 1)
            .await
            .unwrap_err(),
    );
    let mut addresses = addresses();
    addresses.pool_manager = Address::ZERO;
    refused_config(market_tape(&provider, addresses, 0, 10).await.unwrap_err());
    refused_config(
        beacon_prints(&provider, Address::ZERO, 0, 10)
            .await
            .unwrap_err(),
    );
    refused_config(
        latest_beacon_prints(&provider, Address::ZERO, 0, 10, 1)
            .await
            .unwrap_err(),
    );
    refused_config(
        token_transfers(&provider, Address::ZERO, Some(&[TREASURY]), None, 0, 100)
            .await
            .unwrap_err(),
    );
    refused_config(
        token_transfers(&provider, USDC, None, None, 0, 100)
            .await
            .unwrap_err(),
    );

    assert!(node.requests().is_empty());
}

/// A range the caller got backwards is refused before any request.
#[tokio::test]
async fn a_reversed_range_is_refused_before_any_request() {
    let node = FakeNode::new(Vec::new(), u64::MAX);
    let error = get_logs_chunked(&node.provider(), &filter(), 5, 4)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PerpCityError::Validation(ValidationError::InvalidBlockRange {
            from_block: 5,
            to_block: 4
        })
    ));
    assert!(node.requests().is_empty());
}
