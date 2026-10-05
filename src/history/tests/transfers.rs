//! ERC-20 transfers between address sets: the topic filters, their
//! limits, and a mainnet log decoded to its parties.

use super::*;

fn flows(transfers: &[TokenTransfer]) -> Vec<(Address, Address, U256, u64)> {
    transfers
        .iter()
        .map(|t| (t.from, t.to, t.value, t.block_number))
        .collect()
}

/// A real Arbitrum One USDC transfer of 89.999998 USDC, as the node
/// returned it in the receipt of tx 0xa55d3cc1…e26e.
#[tokio::test]
async fn a_mainnet_usdc_transfer_decodes_to_its_parties_and_value() {
    let sender = address!("c3da549ee508386a12f3908d5bf3060fd04b89f5");
    let recipient = address!("e4fb292b59e3d2cdcc16a332035058f9796b5786");
    let tx_hash = b256!("a55d3cc1c657e72ac6d34f47fc20ad1ac7dce3de2c497b3bcf1d559057e6e26e");
    let log = Log {
        inner: PrimitiveLog {
            address: USDC,
            data: LogData::new_unchecked(
                vec![
                    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"),
                    b256!("000000000000000000000000c3da549ee508386a12f3908d5bf3060fd04b89f5"),
                    b256!("000000000000000000000000e4fb292b59e3d2cdcc16a332035058f9796b5786"),
                ],
                bytes!("00000000000000000000000000000000000000000000000000000000055d4a7e"),
            ),
        },
        block_hash: Some(b256!(
            "5414fbeaa040956c8fce9279e1253bbf0772f9a472030ebd72f9c04fb5d90071"
        )),
        block_number: Some(0x1e40_09af),
        block_timestamp: Some(0x6ab1_68d8),
        transaction_hash: Some(tx_hash),
        transaction_index: Some(1),
        log_index: Some(0),
        removed: false,
    };
    let node = FakeNode::new(vec![log], u64::MAX);
    let transfers = token_transfers(
        &node.provider(),
        USDC,
        Some(&[sender]),
        Some(&[recipient]),
        0x1e40_0000,
        0x1e41_0000,
    )
    .await
    .unwrap();
    assert_eq!(
        transfers,
        vec![TokenTransfer {
            block_number: 507_513_263,
            log_index: 0,
            tx_hash,
            from: sender,
            to: recipient,
            value: U256::from(89_999_998u64),
        }]
    );
}

#[tokio::test]
async fn transfers_are_filtered_by_both_sets_and_none_is_any() {
    let logs = vec![
        transfer_log(TREASURY, WALLET_A, 100, 10),
        transfer_log(TREASURY, OUTSIDER, 7, 11),
        transfer_log(WALLET_B, TREASURY, 40, 12),
        transfer_log(OUTSIDER, TREASURY, 5, 13),
        transfer_log(WALLET_A, WALLET_B, 1, 14),
    ];
    let node = FakeNode::new(logs, u64::MAX);
    let provider = node.provider();
    let wallets = [WALLET_A, WALLET_B];

    let out = token_transfers(&provider, USDC, Some(&[TREASURY]), Some(&wallets), 0, 100)
        .await
        .unwrap();
    assert_eq!(flows(&out), vec![(TREASURY, WALLET_A, U256::from(100), 10)]);

    let back = token_transfers(&provider, USDC, Some(&wallets), Some(&[TREASURY]), 0, 100)
        .await
        .unwrap();
    assert_eq!(flows(&back), vec![(WALLET_B, TREASURY, U256::from(40), 12)]);

    let any_recipient = token_transfers(&provider, USDC, Some(&[TREASURY]), None, 0, 100)
        .await
        .unwrap();
    assert_eq!(
        flows(&any_recipient),
        vec![
            (TREASURY, WALLET_A, U256::from(100), 10),
            (TREASURY, OUTSIDER, U256::from(7), 11),
        ]
    );
}

#[tokio::test]
async fn an_unfiltered_or_zero_token_query_is_refused_before_any_request() {
    let node = FakeNode::new(Vec::new(), u64::MAX);
    let provider = node.provider();
    for (token, senders) in [(USDC, None), (Address::ZERO, Some(&[TREASURY][..]))] {
        let error = token_transfers(&provider, token, senders, None, 0, 100)
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                PerpCityError::Validation(ValidationError::InvalidConfig { .. })
            ),
            "{error:?}"
        );
    }
    assert!(node.requests().is_empty());
}

#[tokio::test]
async fn an_empty_set_matches_nothing_without_a_request() {
    let node = FakeNode::new(vec![transfer_log(TREASURY, WALLET_A, 100, 10)], u64::MAX);
    let provider = node.provider();
    for (senders, recipients) in [
        (Some(&[][..]), None),
        (None, Some(&[][..])),
        (Some(&[TREASURY][..]), Some(&[][..])),
    ] {
        let out = token_transfers(&provider, USDC, senders, recipients, 0, 100)
            .await
            .unwrap();
        assert!(out.is_empty(), "{out:?}");
    }
    assert!(node.requests().is_empty());

    let error = token_transfers(&provider, USDC, Some(&[]), None, 100, 0)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            PerpCityError::Validation(ValidationError::InvalidBlockRange { .. })
        ),
        "{error:?}"
    );
}

/// A set over the 1,000-value topic limit is refused before any
/// request; a set at the limit is sent.
#[tokio::test]
async fn a_set_over_the_topic_limit_is_refused() {
    let senders: Vec<Address> = (1..=1_001u64)
        .map(|i| Address::from_word(U256::from(i).into()))
        .collect();
    let node = FakeNode::new(vec![], u64::MAX);
    let provider = node.provider();
    let error = token_transfers(&provider, USDC, Some(&senders), Some(&[WALLET_A]), 0, 100)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            PerpCityError::Validation(ValidationError::InvalidConfig { .. })
        ),
        "{error:?}"
    );
    assert!(node.requests().is_empty());
    token_transfers(&provider, USDC, Some(&senders[..1_000]), None, 0, 100)
        .await
        .unwrap();
    assert_eq!(node.requests(), vec![(0, 100)]);
}

/// An ERC-721 `Transfer` shares the topic but indexes the token id, so
/// it has no data to decode as a value.
#[tokio::test]
async fn a_transfer_that_does_not_decode_is_an_error_not_a_gap() {
    let mut nft = transfer_log(TREASURY, WALLET_A, 0, 10);
    nft.inner.data = LogData::new_unchecked(
        vec![
            IERC20::Transfer::SIGNATURE_HASH,
            TREASURY.into_word(),
            WALLET_A.into_word(),
            B256::with_last_byte(9),
        ],
        Bytes::new(),
    );
    let node = FakeNode::new(vec![nft], u64::MAX);
    let error = token_transfers(&node.provider(), USDC, Some(&[TREASURY]), None, 0, 100)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            PerpCityError::Validation(ValidationError::DecodeFailed { .. })
        ),
        "{error:?}"
    );
}
