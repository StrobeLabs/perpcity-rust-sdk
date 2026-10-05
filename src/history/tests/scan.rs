//! The chunked scan: growing windows, a learned width, narrowing under a
//! cap, newest-first, and the failures a smaller range does not fix.

use super::*;

#[tokio::test]
async fn an_unlimited_provider_is_read_in_growing_chunks() {
    let node = FakeNode::new(logs_every(1_000, 350_000), u64::MAX);
    let logs = get_logs_chunked(&node.provider(), &filter(), 0, 350_000)
        .await
        .unwrap();
    assert_eq!(
        blocks(&logs),
        (0..=350_000).step_by(1_000).collect::<Vec<_>>()
    );
    assert_eq!(
        node.requests(),
        vec![(0, 99_999), (100_000, 299_999), (300_000, 350_000)]
    );
}

#[tokio::test]
async fn a_capped_provider_is_learned_once_and_the_range_read_exactly() {
    let cap = 10_000;
    let node = FakeNode::new(logs_every(777, 120_000), cap);
    let logs = get_logs_chunked(&node.provider(), &filter(), 0, 120_000)
        .await
        .unwrap();
    assert_eq!(
        blocks(&logs),
        (0..=120_000).step_by(777).collect::<Vec<_>>()
    );

    let requests = node.requests();
    let (accepted, rejected): (Vec<_>, Vec<_>) =
        requests.iter().partition(|(from, to)| to - from < cap);
    assert_tiles(&accepted, 0, 120_000);
    // Four halvings from 100k to an accepted 6,250, then two probes
    // between the accepted and rejected widths; none after that.
    assert_eq!(rejected.len(), 6, "rejected requests: {requests:?}");
}

#[tokio::test]
async fn a_dense_stretch_under_a_result_cap_is_narrowed_by_halving() {
    let sparse = logs_every(1_000, 400_000)
        .into_iter()
        .filter(|log| !(300_000..=310_000).contains(&log.block_number.unwrap()));
    let dense = (300_000..=310_000).map(|block| mined_log(EMITTER, TOPIC, Bytes::new(), block, 0));
    let mut logs: Vec<Log> = sparse.chain(dense).collect();
    logs.sort_by_key(|log| log.block_number);
    let expected = blocks(&logs);
    let node = FakeNode::new(logs, u64::MAX).with_max_results(1_000);

    let read = get_logs_chunked(&node.provider(), &filter(), 0, 400_000)
        .await
        .unwrap();
    assert_eq!(blocks(&read), expected);
    let requests = node.requests();
    // Eleven chunks are the least that fit the dense stretch under the
    // cap; the rest is the halving into it and the regrowth after it.
    assert!(
        requests.len() < 40,
        "{} requests to read one dense stretch: {requests:?}",
        requests.len()
    );
}

#[tokio::test]
async fn a_long_capped_scan_retests_the_limit_ever_less_often() {
    let cap = 10_000;
    let node = FakeNode::new(logs_every(5_000, 3_000_000), cap);
    let logs = get_logs_chunked(&node.provider(), &filter(), 0, 3_000_000)
        .await
        .unwrap();
    assert_eq!(logs.len(), 601);
    let requests = node.requests();
    let (accepted, rejected): (Vec<_>, Vec<_>) =
        requests.iter().partition(|(from, to)| to - from < cap);
    assert_tiles(&accepted, 0, 3_000_000);
    // 300 chunks at the cap is the floor. Six rejections learn the
    // limit; each retest (after 16, 32, 64, 128 and 256 accepted
    // chunks) costs about three more.
    assert!(accepted.len() <= 330, "{} accepted", accepted.len());
    assert!(rejected.len() <= 24, "rejected requests: {rejected:?}");
}

#[tokio::test]
async fn newest_first_reads_the_same_range_from_the_top() {
    let node = FakeNode::new(logs_every(500, 60_000), 7_000);
    let provider = node.provider();
    let widths = SharedWidths::new();
    let filter = filter();
    let mut chunks = std::pin::pin!(scan_newest(&provider, &filter, 1_000, 60_000, &widths));
    let mut seen = Vec::new();
    while let Some(chunk) = chunks.try_next().await.unwrap() {
        let chunk = blocks(&chunk);
        if let (Some(last), Some(first)) = (seen.last(), chunk.first()) {
            assert!(first < last, "chunks must arrive newest first");
        }
        seen.extend(chunk.into_iter().rev());
    }
    seen.reverse();
    assert_eq!(seen, (1_000..=60_000).step_by(500).collect::<Vec<_>>());
    let accepted: Vec<_> = node
        .requests()
        .into_iter()
        .filter(|(from, to)| to - from < 7_000)
        .collect();
    assert_tiles(&accepted, 1_000, 60_000);
}

#[tokio::test]
async fn a_single_rejected_block_is_the_error() {
    let node = FakeNode::new(Vec::new(), 0);
    let error = get_logs_chunked(&node.provider(), &filter(), 0, 3)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PerpCityError::Contract(ContractError::LogsRejected {
            from_block: 0,
            to_block: 0,
            ..
        })
    ));
    assert!(!error.is_transient());
    assert_eq!(node.requests().last(), Some(&(0, 0)));
}

#[tokio::test]
async fn failures_no_narrower_range_fixes_are_returned_at_once() {
    let unanswered = [
        Mode::Http(429),
        Mode::Http(503),
        Mode::RpcError(
            429,
            "Your app has exceeded its compute units per second capacity",
        ),
        Mode::RpcError(
            -32_005,
            "daily request count exceeded, request rate limited",
        ),
    ];
    let refused = [
        Mode::Http(401),
        Mode::Http(403),
        Mode::RpcError(-32_601, "the method eth_getLogs does not exist"),
        Mode::RpcError(-32_700, "parse error"),
    ];
    let cases = unanswered
        .into_iter()
        .map(|mode| (mode, true))
        .chain(refused.into_iter().map(|mode| (mode, false)));
    for (mode, transient) in cases {
        let node = FakeNode::new(Vec::new(), u64::MAX).with_mode(mode);
        let error = get_logs_chunked(&node.provider(), &filter(), 0, 500_000)
            .await
            .unwrap_err();
        assert_eq!(error.is_transient(), transient, "{mode:?}: {error}");
        assert_eq!(node.requests(), vec![(0, 99_999)], "{mode:?}");
    }
}

#[tokio::test]
async fn range_rejections_narrow_to_a_single_block() {
    let modes = [
        Mode::Http(504),
        Mode::RpcError(-32_005, "query returned more than 10000 results"),
        Mode::RpcError(-32_602, "Log response size exceeded"),
    ];
    for mode in modes {
        let node = FakeNode::new(Vec::new(), u64::MAX).with_mode(mode);
        get_logs_chunked(&node.provider(), &filter(), 0, 3)
            .await
            .unwrap_err();
        assert_eq!(node.requests().last(), Some(&(0, 0)), "{mode:?}");
    }
}
