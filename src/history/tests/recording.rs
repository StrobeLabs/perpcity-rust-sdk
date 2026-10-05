//! Recordings: raw logs written and read back, decoded with no node, and
//! the tail check against the chain.

use super::*;

/// Four logs over three addresses, two of them without a timestamp.
fn recorded_logs() -> Vec<Log> {
    vec![
        mined_event_log(&taker_opened(1), PERP, 10, 0, Some(41)),
        mined_event_log(
            &IBeacon::IndexUpdated {
                index: Q96 * U256::from(3),
            },
            BEACON,
            10,
            1,
            Some(41),
        ),
        mined_event_log(
            &liquidity_change(POOL_ID, 2, 1_000),
            POOL_MANAGER,
            12,
            0,
            None,
        ),
        mined_event_log(&taker_opened(3), PERP, 20, 0, None),
    ]
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("perpcity-{name}-{}", std::process::id()))
}

/// A recording holds the raw logs, each with its block's timestamp, under
/// a manifest that says what they are; written and read back it is the
/// same recording, and it decodes to the tape with no node.
#[tokio::test]
async fn a_recording_holds_the_raw_logs_and_decodes_to_the_tape_offline() {
    let node = FakeNode::new(recorded_logs(), u64::MAX).with_head(30);
    let history = History::new(node.provider()).with_lag(0);
    let recording = history.record(addresses(), 0, Some(30)).await.unwrap();

    let manifest = recording.manifest();
    assert_eq!(manifest.format, FORMAT);
    assert_eq!(manifest.chain_id, CHAIN_ID);
    assert_eq!((manifest.from_block, manifest.to_block), (0, 30));
    assert_eq!(manifest.tip_hash, hash_of(30));
    assert_eq!(manifest.addresses, addresses());
    assert_eq!((manifest.logs, manifest.undecodable), (4, 0));
    assert_eq!(manifest.crate_version, env!("CARGO_PKG_VERSION"));
    assert!(
        recording
            .logs()
            .iter()
            .all(|log| log.block_timestamp.is_some()),
        "every log carries its timestamp"
    );
    assert_eq!(recording.logs()[2].block_timestamp, Some(timestamp_of(12)));

    let dir = scratch_dir("recording");
    recording.write(&dir).unwrap();
    let read = Recording::read(&dir).unwrap();
    assert_eq!(read, recording);
    assert_eq!(
        manifest.logs_hash,
        keccak256(fs::read(dir.join("logs.jsonl")).unwrap()),
        "the manifest hashes the file as written"
    );
    let scanned = history.market_tape(addresses(), 0, Some(30)).await.unwrap();
    assert_eq!(
        read.tape().unwrap(),
        scanned,
        "the file decodes to the scan"
    );

    // A file short of a log, or with two logs swapped, is refused rather
    // than read as a different tape: the hash binds order and content.
    let lines: Vec<String> = fs::read_to_string(dir.join("logs.jsonl"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    fs::write(dir.join("logs.jsonl"), lines[..3].join("\n") + "\n").unwrap();
    assert!(
        Recording::read(&dir).is_err(),
        "a truncated file is refused"
    );
    let swapped = [&lines[1], &lines[0], &lines[2], &lines[3]];
    fs::write(
        dir.join("logs.jsonl"),
        swapped.map(String::as_str).join("\n") + "\n",
    )
    .unwrap();
    assert!(
        Recording::read(&dir).is_err(),
        "a reordered file is refused"
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// The tail check asks the chain whether the recording's end still stands:
/// the same node agrees; a chain that lost the last event does not.
#[tokio::test]
async fn a_tail_check_holds_against_its_chain_and_fails_against_another() {
    let node = FakeNode::new(recorded_logs(), u64::MAX).with_head(30);
    let history = History::new(node.provider()).with_lag(0);
    let recording = history.record(addresses(), 0, Some(30)).await.unwrap();

    let check = recording.check_tail(&history, 15).await.unwrap();
    assert!(check.holds());
    assert_eq!((check.blocks, check.recorded, check.rescanned), (15, 1, 1));

    let mut shorter = recorded_logs();
    shorter.pop();
    let other = History::new(FakeNode::new(shorter, u64::MAX).with_head(30).provider()).with_lag(0);
    let check = recording.check_tail(&other, 15).await.unwrap();
    assert!(check.tip_hash_matches);
    assert!(!check.logs_match);
    assert_eq!((check.recorded, check.rescanned), (1, 0));
    assert!(!check.holds());
}
