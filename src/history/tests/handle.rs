//! The `History` handle: the width kept across scans, concurrent windows,
//! the lagged head, and its accounting.

use super::*;

#[tokio::test]
async fn a_history_handle_keeps_the_learned_width_across_scans() {
    let cap = 10_000;
    let node = FakeNode::new(logs_every(777, 120_000), cap);
    let history = History::new(node.provider());
    history.logs(&filter(), 0, Some(120_000)).await.unwrap();
    let after_first = node.requests().len();

    // A second scan starts at the learned width instead of re-paying the
    // halvings from the initial span.
    history
        .logs(&filter(), 120_001, Some(240_000))
        .await
        .unwrap();
    let second: Vec<_> = node.requests().into_iter().skip(after_first).collect();
    let (first_from, first_to) = second[0];
    assert!(
        first_to - first_from < cap,
        "second scan re-learned the width from scratch: {second:?}"
    );
    // The only rejections left are one periodic retest re-narrowing
    // (a few probes each, as the long-scan budget test prices them),
    // not the initial halvings from the 100k span.
    let rejected = second.iter().filter(|(from, to)| to - from >= cap).count();
    assert!(
        rejected <= 4,
        "more than a periodic retest was rejected: {second:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_handle_reads_windows_concurrently() {
    let latency = Duration::from_millis(250);
    let node = FakeNode::new(logs_every(1_000, 350_000), u64::MAX).with_latency(latency);
    let history = History::new(node.provider());
    let started = tokio::time::Instant::now();
    let logs = history.logs(&filter(), 0, Some(350_000)).await.unwrap();
    assert_eq!(
        blocks(&logs),
        (0..=350_000).step_by(1_000).collect::<Vec<_>>()
    );
    // Four windows carve at the initial span and fly together, so the
    // scan takes about one round trip, not four.
    assert_eq!(node.peak_in_flight(), 4);
    assert!(
        started.elapsed() < latency * 2,
        "windows were read sequentially: {:?}",
        started.elapsed()
    );
    assert_tiles(&node.requests(), 0, 350_000);
}

#[tokio::test]
async fn a_concurrent_scan_learns_a_cap_and_still_reads_exactly() {
    let cap = 10_000;
    let node = FakeNode::new(logs_every(777, 120_000), cap);
    let history = History::new(node.provider());
    let logs = history.logs(&filter(), 0, Some(120_000)).await.unwrap();
    assert_eq!(
        blocks(&logs),
        (0..=120_000).step_by(777).collect::<Vec<_>>()
    );
    let requests = node.requests();
    let (accepted, rejected): (Vec<_>, Vec<_>) =
        requests.iter().partition(|(from, to)| to - from < cap);
    assert_tiles(&accepted, 0, 120_000);
    // The windows in flight when the scan starts each pay their own
    // rejections at the unlearned widths; the budget must stay the same
    // order as the sequential search's six.
    assert!(rejected.len() <= 12, "rejected requests: {requests:?}");
}

#[tokio::test]
async fn a_handles_newest_read_stays_sequential_and_tight() {
    let logs = (0..=600_000)
        .step_by(500)
        .map(|block| print_log(block, 0, Q96 * U256::from(block + 1), Some(block)))
        .collect();
    let node = FakeNode::new(logs, 7_000);
    let history = History::new(node.provider()).with_in_flight(8);
    let prints = history
        .latest_beacon_prints(BEACON, 0, Some(600_000), 5)
        .await
        .unwrap();
    assert_eq!(prints.len(), 5);
    assert_eq!(prints.last().unwrap().block_number, 600_000);
    let served: Vec<_> = node
        .requests()
        .into_iter()
        .filter(|(from, to)| to - from < 7_000)
        .collect();
    assert_eq!(
        served.len(),
        1,
        "a newest-first read must not fan out below its limit: {served:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_handle_accounts_for_every_request_it_sent() {
    let latency = Duration::from_millis(100);
    let cap = 10_000;
    let node = FakeNode::new(logs_every(777, 120_000), cap).with_latency(latency);
    let history = History::new(node.provider()).with_in_flight(1);
    // A fresh handle has counted nothing; its width is the initial span.
    assert_eq!(history.stats().requests, 0);
    assert_eq!(history.stats().learned_width, 100_000);

    let logs = history.logs(&filter(), 0, Some(120_000)).await.unwrap();
    let stats = history.stats();
    let requests = node.requests();
    assert_eq!(stats.requests, requests.len() as u64);
    assert_eq!(
        stats.rejections,
        requests
            .iter()
            .filter(|(from, to)| to - from >= cap)
            .count() as u64
    );
    assert_eq!(stats.logs, logs.len() as u64);
    assert!(
        stats.learned_width > 0 && stats.learned_width < cap,
        "width not learned: {stats:?}"
    );
    // Sequential scan under a paused clock: exactly one latency quantum
    // per request.
    assert_eq!(stats.request_time, latency * stats.requests as u32);

    // Counters are cumulative across the handle's scans; the second
    // range holds no logs, so only requests and time move.
    history
        .logs(&filter(), 120_001, Some(240_000))
        .await
        .unwrap();
    let later = history.stats();
    assert!(later.requests > stats.requests);
    assert_eq!(later.logs, stats.logs);
}

#[tokio::test]
async fn the_handle_reads_to_the_lagged_head_by_default() {
    let node = FakeNode::new(logs_every(1_000, 100_000), u64::MAX).with_head(100_000);
    let history = History::new(node.provider()).with_lag(8);
    assert_eq!(history.tip().await.unwrap(), 99_992);

    let logs = history.logs(&filter(), 0, None).await.unwrap();
    assert_eq!(node.requests(), vec![(0, 99_992)]);
    assert_eq!(logs.last().unwrap().block_number.unwrap(), 99_000);
}
