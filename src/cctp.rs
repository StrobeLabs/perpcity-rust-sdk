//! Circle's Cross-Chain Transfer Protocol (CCTP V2): how native USDC
//! reaches a market's chain from another one.
//!
//! A transfer is three steps, and this module gives each its shape:
//!
//! 1. [`Burn`] — `depositForBurn` on the source chain's `TokenMessengerV2`
//!    burns the USDC and emits a message naming the destination and the
//!    recipient. The caller approves the messenger for the amount first.
//! 2. [`IrisClient::attestation`] — Circle's attestation service signs the
//!    message once the source chain reaches the burn's [`Finality`].
//! 3. [`Attested::receive_calldata`] — `receiveMessage` on the destination
//!    chain's `MessageTransmitterV2` checks the signature and mints the USDC
//!    to the recipient. With no destination caller set, anyone may submit
//!    it; a message mints once, which [`CctpChain::nonce_used`] reads.
//!
//! The module builds calldata and reads state; it never signs, so any
//! sender (a [`PerpClient`](crate::PerpClient) pipeline, a plain alloy
//! provider with a wallet) can carry it.
#![doc = "\n\nThe design of this module: [`src/cctp/DESIGN.md`](https://github.com/StrobeLabs/perpcity-rust-sdk/blob/main/src/cctp/DESIGN.md)."]

use std::ops::Range;

use alloy::primitives::{Address, B256, Bytes, U256, address};
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use serde::Deserialize;
use thiserror::Error;

use crate::UsdcAtoms;
use crate::client::{ARBITRUM_CHAIN_ID, ARBITRUM_SEPOLIA_CHAIN_ID, ARBITRUM_USDC};
use crate::contracts::{IMessageTransmitterV2, ITokenMessengerV2};
use crate::errors::Result;

/// `TokenMessengerV2` on every CCTP V2 mainnet chain (deployed with CREATE2).
const TOKEN_MESSENGER_V2: Address = address!("28b5a0e9C621a5BadaA536219b3a228C8168cf5d");
/// `MessageTransmitterV2` on every CCTP V2 mainnet chain.
const MESSAGE_TRANSMITTER_V2: Address = address!("81D40F21F12A8F0E3252Bccb954D722d4c464B64");
/// `TokenMessengerV2` on every CCTP V2 testnet chain.
const TOKEN_MESSENGER_V2_TESTNET: Address = address!("8FE6B999Dc680CcFDD5Bf7EB0974218be2542DAA");
/// `MessageTransmitterV2` on every CCTP V2 testnet chain.
const MESSAGE_TRANSMITTER_V2_TESTNET: Address =
    address!("E737e5cEBEEBa77EFE34D4aa090756590b1CE275");

/// Base mainnet chain ID.
pub const BASE_CHAIN_ID: u64 = 8453;
/// Base Sepolia chain ID.
pub const BASE_SEPOLIA_CHAIN_ID: u64 = 84532;

/// One chain as CCTP V2 addresses it: its domain, its native USDC, and its
/// two CCTP contracts.
///
/// The USDC is Circle's, which is the market's collateral on Arbitrum One
/// but not on Arbitrum Sepolia, whose markets settle in a mock token
/// ([`ARBITRUM_SEPOLIA_USDC`](crate::ARBITRUM_SEPOLIA_USDC)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CctpChain {
    /// The chain's EVM chain ID.
    pub chain_id: u64,
    /// The chain's CCTP domain, which messages address it by.
    pub domain: u32,
    /// Circle's native USDC on the chain.
    pub usdc: Address,
    /// The `TokenMessengerV2` a burn is sent to.
    pub token_messenger: Address,
    /// The `MessageTransmitterV2` a mint is sent to.
    pub message_transmitter: Address,
}

/// Arbitrum One, CCTP domain 3.
pub const CCTP_ARBITRUM: CctpChain = CctpChain {
    chain_id: ARBITRUM_CHAIN_ID,
    domain: 3,
    usdc: ARBITRUM_USDC,
    token_messenger: TOKEN_MESSENGER_V2,
    message_transmitter: MESSAGE_TRANSMITTER_V2,
};

/// Base, CCTP domain 6.
pub const CCTP_BASE: CctpChain = CctpChain {
    chain_id: BASE_CHAIN_ID,
    domain: 6,
    usdc: address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"),
    token_messenger: TOKEN_MESSENGER_V2,
    message_transmitter: MESSAGE_TRANSMITTER_V2,
};

/// Arbitrum Sepolia, CCTP domain 3, with Circle's testnet USDC.
pub const CCTP_ARBITRUM_SEPOLIA: CctpChain = CctpChain {
    chain_id: ARBITRUM_SEPOLIA_CHAIN_ID,
    domain: 3,
    usdc: address!("75faf114eafb1BDbe2F0316DF893fd58CE46AA4d"),
    token_messenger: TOKEN_MESSENGER_V2_TESTNET,
    message_transmitter: MESSAGE_TRANSMITTER_V2_TESTNET,
};

/// Base Sepolia, CCTP domain 6.
pub const CCTP_BASE_SEPOLIA: CctpChain = CctpChain {
    chain_id: BASE_SEPOLIA_CHAIN_ID,
    domain: 6,
    usdc: address!("036CbD53842c5426634e7929541eC2318f3dCF7e"),
    token_messenger: TOKEN_MESSENGER_V2_TESTNET,
    message_transmitter: MESSAGE_TRANSMITTER_V2_TESTNET,
};

impl CctpChain {
    /// The chain with this chain ID, among the four this module knows.
    pub fn for_chain_id(chain_id: u64) -> Option<Self> {
        [
            CCTP_ARBITRUM,
            CCTP_BASE,
            CCTP_ARBITRUM_SEPOLIA,
            CCTP_BASE_SEPOLIA,
        ]
        .into_iter()
        .find(|chain| chain.chain_id == chain_id)
    }

    /// Whether this chain has already minted the message with `nonce`.
    ///
    /// `provider` must be connected to this chain.
    pub async fn nonce_used<P: Provider>(&self, provider: &P, nonce: B256) -> Result<bool> {
        let used = IMessageTransmitterV2::new(self.message_transmitter, provider)
            .usedNonces(nonce)
            .call()
            .await?;
        Ok(!used.is_zero())
    }
}

/// How final the source chain must be before Circle attests a burn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finality {
    /// Attested at the source chain's confirmed level, in seconds, for a fee
    /// Circle sets per route.
    Fast,
    /// Attested at the source chain's hard finality, in minutes; Circle
    /// charges no fee.
    Standard,
}

impl Finality {
    /// The `minFinalityThreshold` a burn carries for this level.
    pub const fn threshold(self) -> u32 {
        match self {
            Self::Fast => 1000,
            Self::Standard => 2000,
        }
    }
}

/// One `depositForBurn` order: burn `amount` of the source chain's USDC and
/// mint it, less the fee Circle executes, to `recipient` on `destination`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Burn {
    /// The USDC burned on the source chain.
    pub amount: UsdcAtoms,
    /// The chain the USDC is minted on.
    pub destination: CctpChain,
    /// The address the USDC is minted to.
    pub recipient: Address,
    /// The finality Circle waits for before attesting.
    pub finality: Finality,
    /// The most Circle may keep from `amount` as its fee; zero suffices
    /// for [`Finality::Standard`].
    pub max_fee: UsdcAtoms,
}

impl Burn {
    /// A free [`Finality::Standard`] transfer of `amount` to `recipient`.
    pub fn standard(amount: UsdcAtoms, destination: CctpChain, recipient: Address) -> Self {
        Self {
            amount,
            destination,
            recipient,
            finality: Finality::Standard,
            max_fee: UsdcAtoms::ZERO,
        }
    }

    /// The `depositForBurn` calldata, to send to `source.token_messenger`
    /// after approving it for `amount` of `source.usdc`.
    ///
    /// The destination caller is left empty, so anyone may submit the mint.
    pub fn calldata(&self, source: &CctpChain) -> Bytes {
        ITokenMessengerV2::depositForBurnCall {
            amount: U256::from(self.amount.atoms()),
            destinationDomain: self.destination.domain,
            mintRecipient: self.recipient.into_word(),
            burnToken: source.usdc,
            destinationCaller: B256::ZERO,
            maxFee: U256::from(self.max_fee.atoms()),
            minFinalityThreshold: self.finality.threshold(),
        }
        .abi_encode()
        .into()
    }
}

/// Byte offsets into a V2 message: the header, then the burn body at 148.
const NONCE: Range<usize> = 12..44;
const DESTINATION_DOMAIN: Range<usize> = 8..12;
const MINT_RECIPIENT: Range<usize> = 196..216;
const AMOUNT: Range<usize> = 216..248;
const FEE_EXECUTED: Range<usize> = 312..344;
const BURN_MESSAGE_LEN: usize = 376;

/// A burn message with Circle's signature over it, ready to mint.
///
/// Only a burn message is accepted: the fields read here are the burn
/// body's, at the offsets `MessageV2` and `BurnMessageV2` define.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attested {
    message: Bytes,
    attestation: Bytes,
}

impl Attested {
    /// Pair a message with its attestation.
    ///
    /// # Errors
    ///
    /// [`IrisError::Malformed`], inside [`PerpCityError::Iris`](crate::PerpCityError::Iris),
    /// when `message` is shorter than a burn message.
    pub fn new(message: Bytes, attestation: Bytes) -> Result<Self> {
        if message.len() < BURN_MESSAGE_LEN {
            return Err(IrisError::Malformed(format!(
                "message of {} bytes is shorter than a burn message",
                message.len()
            ))
            .into());
        }
        Ok(Self {
            message,
            attestation,
        })
    }

    /// The nonce Circle assigned, which the destination marks used on mint.
    pub fn nonce(&self) -> B256 {
        B256::from_slice(&self.message[NONCE])
    }

    /// The CCTP domain the message mints on.
    pub fn destination_domain(&self) -> u32 {
        u32::from_be_bytes(
            self.message[DESTINATION_DOMAIN]
                .try_into()
                .expect("a four-byte range"),
        )
    }

    /// The address the message mints to.
    pub fn recipient(&self) -> Address {
        Address::from_slice(&self.message[MINT_RECIPIENT])
    }

    /// The USDC burned, before Circle's fee.
    pub fn amount(&self) -> U256 {
        U256::from_be_slice(&self.message[AMOUNT])
    }

    /// The fee Circle keeps from the amount.
    pub fn fee_executed(&self) -> U256 {
        U256::from_be_slice(&self.message[FEE_EXECUTED])
    }

    /// The `receiveMessage` calldata, to send to the destination's
    /// `message_transmitter`.
    pub fn receive_calldata(&self) -> Bytes {
        IMessageTransmitterV2::receiveMessageCall {
            message: self.message.clone(),
            attestation: self.attestation.clone(),
        }
        .abi_encode()
        .into()
    }
}

/// Where Circle's attestation of one burn stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestationStatus {
    /// Iris has not indexed the burn yet, or the hash holds no burn.
    NotFound,
    /// Iris has the burn and waits for its finality.
    Pending,
    /// Signed and ready to mint.
    Complete(Attested),
}

/// A failure talking to Iris, Circle's attestation service.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IrisError {
    /// The request did not complete: a connection, TLS or timeout failure.
    #[error("Iris request failed: {0}")]
    Request(#[source] reqwest::Error),
    /// Iris answered with an unexpected HTTP status.
    #[error("Iris answered HTTP {status}: {body}")]
    Status {
        /// The HTTP status code.
        status: u16,
        /// The response body, truncated.
        body: String,
    },
    /// The response did not have the shape of an attestation.
    #[error("Iris response malformed: {0}")]
    Malformed(String),
}

impl IrisError {
    /// Whether retrying later may succeed: a failed request, a rate limit,
    /// or a server error. A malformed answer or a client error stays so.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Request(_) => true,
            Self::Status { status, .. } => *status == 429 || *status >= 500,
            Self::Malformed(_) => false,
        }
    }
}

/// Circle's attestation service (Iris), version 2.
#[derive(Debug, Clone)]
pub struct IrisClient {
    base_url: String,
    http: reqwest::Client,
}

impl IrisClient {
    /// The mainnet service, which attests burns on mainnet chains.
    pub fn mainnet() -> Self {
        Self::new("https://iris-api.circle.com")
    }

    /// The sandbox service, which attests burns on testnet chains.
    pub fn sandbox() -> Self {
        Self::new("https://iris-api-sandbox.circle.com")
    }

    /// The service at `base_url`, without a trailing slash.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::new(),
        }
    }

    /// The attestation of the burn in transaction `tx_hash` on the chain
    /// with CCTP domain `source_domain`.
    ///
    /// Iris allows 35 requests a second and blocks a caller that exceeds
    /// it for five minutes; poll at a human pace.
    pub async fn attestation(
        &self,
        source_domain: u32,
        tx_hash: B256,
    ) -> Result<AttestationStatus> {
        let url = format!(
            "{}/v2/messages/{source_domain}?transactionHash={tx_hash}",
            self.base_url
        );
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(IrisError::Request)?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(IrisError::Request)?;
        parse_messages(status, &body)
    }
}

#[derive(Deserialize)]
struct MessagesResponse {
    messages: Vec<IrisMessage>,
}

#[derive(Deserialize)]
struct IrisMessage {
    status: String,
    message: Option<String>,
    attestation: Option<String>,
}

const BODY_EXCERPT: usize = 200;

fn parse_messages(status: u16, body: &str) -> Result<AttestationStatus> {
    if status == 404 {
        return Ok(AttestationStatus::NotFound);
    }
    if status != 200 {
        return Err(IrisError::Status {
            status,
            body: body.chars().take(BODY_EXCERPT).collect(),
        }
        .into());
    }
    let response: MessagesResponse =
        serde_json::from_str(body).map_err(|e| IrisError::Malformed(e.to_string()))?;
    let Some(first) = response.messages.into_iter().next() else {
        return Ok(AttestationStatus::NotFound);
    };
    if first.status != "complete" {
        return Ok(AttestationStatus::Pending);
    }
    let hex = |field: Option<String>, name: &str| {
        field
            .as_deref()
            .unwrap_or_default()
            .parse::<Bytes>()
            .map_err(|e| IrisError::Malformed(format!("{name}: {e}")))
    };
    let message = hex(first.message, "message")?;
    let attestation = hex(first.attestation, "attestation")?;
    Attested::new(message, attestation).map(AttestationStatus::Complete)
}

#[cfg(test)]
mod tests {
    use alloy::hex;

    use super::*;
    use crate::PerpCityError;

    /// A burn message as Iris returns it: a Standard transfer of 5 USDC
    /// from Base (domain 6) to `recipient` on Arbitrum (domain 3), built
    /// field by field at the `MessageV2` / `BurnMessageV2` offsets.
    fn burn_message(nonce: B256, recipient: Address, amount: u64, fee: u64) -> Vec<u8> {
        let mut message = Vec::new();
        message.extend_from_slice(&1u32.to_be_bytes()); // version
        message.extend_from_slice(&6u32.to_be_bytes()); // source domain
        message.extend_from_slice(&3u32.to_be_bytes()); // destination domain
        message.extend_from_slice(nonce.as_slice());
        message.extend_from_slice(TOKEN_MESSENGER_V2.into_word().as_slice()); // sender
        message.extend_from_slice(TOKEN_MESSENGER_V2.into_word().as_slice()); // recipient
        message.extend_from_slice(B256::ZERO.as_slice()); // destination caller
        message.extend_from_slice(&2000u32.to_be_bytes());
        message.extend_from_slice(&2000u32.to_be_bytes());
        assert_eq!(message.len(), 148);
        message.extend_from_slice(&1u32.to_be_bytes()); // body version
        message.extend_from_slice(CCTP_BASE.usdc.into_word().as_slice());
        message.extend_from_slice(recipient.into_word().as_slice());
        message.extend_from_slice(&U256::from(amount).to_be_bytes::<32>());
        message.extend_from_slice(recipient.into_word().as_slice()); // message sender
        message.extend_from_slice(&U256::ZERO.to_be_bytes::<32>()); // max fee
        message.extend_from_slice(&U256::from(fee).to_be_bytes::<32>());
        message.extend_from_slice(&U256::ZERO.to_be_bytes::<32>()); // expiration block
        assert_eq!(message.len(), BURN_MESSAGE_LEN);
        message
    }

    /// Selectors as deployed: each is a PUSH4 in the bytecode of the
    /// mainnet implementation behind its proxy (read 2026-10-12 on Base).
    #[test]
    fn selectors_match_the_deployed_contracts() {
        assert_eq!(
            ITokenMessengerV2::depositForBurnCall::SELECTOR,
            hex!("8e0250ee")
        );
        assert_eq!(
            IMessageTransmitterV2::receiveMessageCall::SELECTOR,
            hex!("57ecfd28")
        );
        assert_eq!(
            IMessageTransmitterV2::usedNoncesCall::SELECTOR,
            hex!("feb61724")
        );
    }

    #[test]
    fn a_standard_burn_mints_to_the_recipient_on_the_destination_domain() {
        let recipient = address!("c3dA549ee508386A12f3908d5BF3060Fd04b89f5");
        let burn = Burn::standard(UsdcAtoms::new(5_000_000), CCTP_ARBITRUM, recipient);
        let call =
            ITokenMessengerV2::depositForBurnCall::abi_decode(&burn.calldata(&CCTP_BASE)).unwrap();
        assert_eq!(call.amount, U256::from(5_000_000));
        assert_eq!(call.destinationDomain, 3);
        assert_eq!(call.mintRecipient, recipient.into_word());
        assert_eq!(call.burnToken, CCTP_BASE.usdc);
        assert_eq!(call.destinationCaller, B256::ZERO);
        assert_eq!(call.maxFee, U256::ZERO);
        assert_eq!(call.minFinalityThreshold, 2000);
    }

    #[test]
    fn an_attested_message_reads_its_burn_fields() {
        let nonce = B256::repeat_byte(0xab);
        let recipient = Address::repeat_byte(0x42);
        let attested = Attested::new(
            burn_message(nonce, recipient, 5_000_000, 650).into(),
            Bytes::from_static(&[0x01; 65]),
        )
        .unwrap();
        assert_eq!(attested.nonce(), nonce);
        assert_eq!(attested.destination_domain(), 3);
        assert_eq!(attested.recipient(), recipient);
        assert_eq!(attested.amount(), U256::from(5_000_000));
        assert_eq!(attested.fee_executed(), U256::from(650));
        let call =
            IMessageTransmitterV2::receiveMessageCall::abi_decode(&attested.receive_calldata())
                .unwrap();
        assert_eq!(call.attestation.len(), 65);
    }

    #[test]
    fn a_short_message_is_refused() {
        assert!(matches!(
            Attested::new(Bytes::from_static(&[0; 148]), Bytes::new()),
            Err(PerpCityError::Iris(IrisError::Malformed(_)))
        ));
    }

    #[test]
    fn iris_answers_parse_to_their_status() {
        // The body Iris returns for an unknown hash (read 2026-10-12).
        let missing = r#"{"error":"Message not found for provided parameters"}"#;
        assert_eq!(
            parse_messages(404, missing).unwrap(),
            AttestationStatus::NotFound
        );

        let pending = r#"{"messages":[{"attestation":"PENDING","message":"0x","eventNonce":"0x00","cctpVersion":2,"status":"pending_confirmations"}]}"#;
        assert_eq!(
            parse_messages(200, pending).unwrap(),
            AttestationStatus::Pending
        );

        let message = hex::encode_prefixed(burn_message(B256::repeat_byte(7), Address::ZERO, 1, 0));
        let complete = format!(
            r#"{{"messages":[{{"attestation":"0x{}","message":"{message}","status":"complete"}}]}}"#,
            "11".repeat(65)
        );
        let AttestationStatus::Complete(attested) = parse_messages(200, &complete).unwrap() else {
            panic!("a complete message is ready to mint");
        };
        assert_eq!(attested.nonce(), B256::repeat_byte(7));

        let limited = parse_messages(429, "slow down").unwrap_err();
        assert!(limited.is_transient());
        let malformed = parse_messages(200, "{}").unwrap_err();
        assert!(!malformed.is_transient());
    }

    /// Live: the mainnet service answers an unknown hash with `NotFound`.
    #[tokio::test]
    #[ignore = "network: calls Circle's Iris API"]
    async fn iris_mainnet_answers_an_unknown_hash() {
        let status = IrisClient::mainnet()
            .attestation(CCTP_BASE.domain, B256::ZERO)
            .await
            .unwrap();
        assert_eq!(status, AttestationStatus::NotFound);
    }

    #[test]
    fn chains_resolve_by_chain_id() {
        assert_eq!(CctpChain::for_chain_id(BASE_CHAIN_ID), Some(CCTP_BASE));
        assert_eq!(
            CctpChain::for_chain_id(ARBITRUM_CHAIN_ID),
            Some(CCTP_ARBITRUM)
        );
        assert_eq!(CctpChain::for_chain_id(1), None);
    }
}
