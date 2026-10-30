# Operations for the next contract version

Up: [client](../DESIGN.md), and [root](../../../DESIGN.md).
This component uses private [contract bindings](../../contracts/DESIGN.md), exact
[units](../../units/DESIGN.md), typed [errors](../../errors/DESIGN.md), and verified
[historical events](../../history/DESIGN.md). The caller decides when to act and
which authorities to trust.

## Type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`CurrentDeployment`](../operations.rs#L33) | reviewed proxy and implementation identity, checked independently of the cached legacy era | deployment configuration supplied by the operator | [`StateAt::verify_current`](../DESIGN.md), [`StateAt::operations`](../DESIGN.md), [`StateAt::operational_last_touch`](../DESIGN.md), [`StateAt::backstop_position`](../DESIGN.md), [`StateAt::backstop_call`](../DESIGN.md); the strategy layer supplies reviewed pins. |
| [`ContractCode`](../operations.rs#L45) | reviewed address and runtime hash for a direct contract | deployment configuration supplied by the operator | [`StateAt::verify_contract`](../DESIGN.md), [`StateAt::fee_state`](../DESIGN.md), [`StateAt::controller_state`](../DESIGN.md), [`StateAt::withdrawal_claim`](../DESIGN.md), [`StateAt::safe_nonce`](../DESIGN.md), [`StateAt::safe_version`](../DESIGN.md), [`StateAt::safe_master_copy`](../DESIGN.md); the strategy layer enrolls modules, controllers and executors. |
| [`ContractCall`](../operations.rs#L54) | exact call, never proof of deployment support or completion | [`touch_call`](../operations.rs#L766), [`StateAt::backstop_call`](../DESIGN.md), [`set_tvl_cap_call`](../operations.rs#L770), [`set_utilization_curve_call`](../operations.rs#L774), [`controller_call`](../operations.rs#L789), [`usdc_transfer_call`](../operations.rs#L837), [`multisend_call`](../operations.rs#L856) | [`ContractCall::simulate`](../operations.rs#L65), [`ContractCall::send`](../operations.rs#L73), [`multisend_call`](../operations.rs#L856); the strategy layer prepares administrative proposals or sends permissionless calls. |
| [`OperationalState`](../operations.rs#L84) | current-build authority, checkpoint and TVL reads from one pinned block | [`StateAt::operations`](../DESIGN.md) | the strategy layer's operational policies; TVL is exact USDC atoms in U256 because the cap can be U256::MAX. |
| [`UtilizationCurve`](../operations.rs#L108) | immutable curve in WAD; construction and deserialization enforce contract bounds | [`UtilizationCurve::new`](../operations.rs#L133), or checked deserialization | [`set_utilization_curve_call`](../operations.rs#L774); the strategy layer compares old and proposed slopes. |
| [`FeeState`](../operations.rs#L182) | separately owned module and valid curve from one pinned block | [`StateAt::fee_state`](../DESIGN.md) | the strategy layer's parameter-change authorization. |
| [`ControllerState`](../operations.rs#L195) | verified code and permanent market binding; current bucket and reservation state | [`StateAt::controller_state`](../DESIGN.md) | the strategy layer's withdrawal monitor and guardian plans. |
| [`ClaimStatus`](../operations.rs#L223) | the source controller's four claim states; unknown values fail decoding | decoded inside the claim read | the strategy layer's claim-response policy. |
| [`WithdrawalClaim`](../operations.rs#L236) | exact claim from a verified bound controller | [`StateAt::withdrawal_claim`](../DESIGN.md) | the strategy layer's incident plans and observations. |
| [`BackstopKind`](../operations.rs#L256) | maker versus taker acquisition entry point | the caller or maker liquidity decoded inside the position read | [`BackstopOrder`](../operations.rs#L266); the strategy layer selects and validates the acquired exposure. |
| [`BackstopOrder`](../operations.rs#L266) | a backstop request whose margin, recipient and deadline are checked before encoding | the caller | [`StateAt::backstop_call`](../DESIGN.md); the strategy layer enforces capital budgets and recipient policy. |
| [`BackstopPosition`](../operations.rs#L281) | stored owner, classification and complete position/maker/taker fingerprint at the handle's block | [`StateAt::backstop_position`](../DESIGN.md) | the strategy layer refuses acquisition after a position changes; this is not an atomic on-chain guard. |
| [`FundingState`](../operations.rs#L292) | balance and market allowance from one pinned block | [`StateAt::funding_state`](../DESIGN.md) | the strategy layer's explicit capital checks. |
| [`ControllerAction`](../operations.rs#L302) | a typed controller request with no arbitrary calldata option | the caller | [`controller_call`](../operations.rs#L789); the strategy layer separately proves the required live guardian or governor role. |

## Deployment checks and execution

`operations` targets contracts commit `5141c2f`. Legacy bindings, the cached
`Era`, and existing trading APIs remain unchanged. No production deployment
addresses or code hashes are included.

Operators must review each deployment and supply its proxy runtime hash,
implementation address, and implementation runtime hash. Each market read checks
all three at its block. Module, controller, and Safe reads also check their code.
These checks compare deployed code with the configuration. They do not infer
compatibility from a matching function selector. RPC failures return read errors.
A code mismatch returns `UnsupportedOperations`.

Callers supply deployment pins, which the SDK compares with chain state.
`UtilizationCurve::new` and deserialization check curve bounds. Callers cannot
mutate the fields afterward. TVL fields use `U256` USDC atoms because the
contract allows `U256::MAX` as its cap. Funding and claim amounts use `UsdcAtoms`. The
strategy sets headroom requirements, decides when to reduce fees or investigate
claims, and configures trusted PerpCity authorities.

`ContractCall::send` uses the existing transaction pipeline. It does not prevent
state changes after an earlier read. Callers must revalidate exported Safe batches
before execution. The Safe runs MultiSend through delegatecall. MultiSend then
executes the calls in order, with the Safe as their sender. Multicall3 cannot
replace it.

Claim reduction and cash return are separate transactions. Recovery uses the
amount in the confirmed reduction event. `TxBuilder::with_max_fee_per_gas` checks
the prepared fee before signing. If it exceeds the limit, the builder rejects the
transaction and releases the reserved nonce.

## Accepted structure

- Deployment pins specify expected code. Each read compares them with the code at its block. Callers must check authority separately.
- Reads return `BackstopKind` inside `BackstopPosition` and `ClaimStatus` inside `WithdrawalClaim`. No read returns either enum on its own.
- `ContractCall` contains calldata for a Safe or external wallet. Callers must check authority and contract compatibility before sending. They must also recheck state and confirm the transaction result.
- `UtilizationCurve` validates immutable parameters at construction, like `Ratio`. The strategy decides whether to change them.

## Terminology

- A current deployment identifies the reviewed proxy and implementation for source commit `5141c2f`. Each read verifies that identity at its block.
- Contract code identifies an address and its reviewed runtime hash. For a Safe proxy, the caller also checks the singleton.
- Operational state contains market values read at one fixed block.
