//! Implements the Optimism engine API RPC methods.

use alloy_eips::eip7685::Requests;
use alloy_primitives::{BlockHash, B256, B64, U64};
use alloy_rpc_types_engine::{
    ClientVersionV1, ExecutionPayloadBodiesV1, ExecutionPayloadInputV2, ExecutionPayloadV3,
    ForkchoiceState, ForkchoiceUpdated, PayloadId, PayloadStatus,
};
use derive_more::Constructor;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::{server::RpcModule, RpcResult};
use op_alloy_rpc_types_engine::{
    OpExecutionData, OpExecutionPayloadV4, ProtocolVersion, ProtocolVersionFormatV0,
    SuperchainSignal,
};
use reth_chainspec::EthereumHardforks;
use reth_node_api::{EngineApiValidator, EngineTypes};
use reth_rpc_api::IntoEngineApiRpcModule;
use reth_rpc_engine_api::EngineApi;
use reth_storage_api::{BlockReader, HeaderProvider, StateProviderFactory};
use reth_transaction_pool::TransactionPool;
use tracing::{debug, info, trace};

/// The list of all supported Engine capabilities available over the engine endpoint.
///
/// Spec: <https://specs.optimism.io/protocol/exec-engine.html>
pub const OP_ENGINE_CAPABILITIES: &[&str] = &[
    "engine_forkchoiceUpdatedV1",
    "engine_forkchoiceUpdatedV2",
    "engine_forkchoiceUpdatedV3",
    "engine_getClientVersionV1",
    "engine_getPayloadV2",
    "engine_getPayloadV3",
    "engine_getPayloadV4",
    "engine_newPayloadV2",
    "engine_newPayloadV3",
    "engine_newPayloadV4",
    "engine_getPayloadBodiesByHashV1",
    "engine_getPayloadBodiesByRangeV1",
    "engine_signalSuperchainV1",
];

/// OP Stack protocol version
/// See also: <https://github.com/ethereum-optimism/op-geth/blob/c3a989eb882d150a936df27bcfa791838b474d55/params/superchain.go#L13-L13>
pub const OP_STACK_SUPPORT: ProtocolVersion = ProtocolVersion::V0(ProtocolVersionFormatV0 {
    build: B64::ZERO,
    major: 9,
    minor: 0,
    patch: 0,
    pre_release: 0,
});

/// Extension trait that gives access to Optimism engine API RPC methods.
///
/// Note:
/// > The provider should use a JWT authentication layer.
///
/// This follows the Optimism specs that can be found at:
/// <https://specs.optimism.io/protocol/exec-engine.html#engine-api>
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "engine"), server_bounds(Engine::PayloadAttributes: jsonrpsee::core::DeserializeOwned))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "engine", client_bounds(Engine::PayloadAttributes: jsonrpsee::core::Serialize + Clone), server_bounds(Engine::PayloadAttributes: jsonrpsee::core::DeserializeOwned)))]
pub trait OpEngineApi<Engine: EngineTypes> {
    /// Sends the given payload to the execution layer client, as specified for the Shanghai fork.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/584905270d8ad665718058060267061ecfd79ca5/src/engine/shanghai.md#engine_newpayloadv2>
    ///
    /// No modifications needed for OP compatibility.
    #[method(name = "newPayloadV2")]
    async fn new_payload_v2(&self, payload: ExecutionPayloadInputV2) -> RpcResult<PayloadStatus>;

    /// Sends the given payload to the execution layer client, as specified for the Cancun fork.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/main/src/engine/cancun.md#engine_newpayloadv3>
    ///
    /// OP modifications:
    /// - expected versioned hashes MUST be an empty array: therefore the `versioned_hashes`
    ///   parameter is removed.
    /// - parent beacon block root MUST be the parent beacon block root from the L1 origin block of
    ///   the L2 block.
    /// - blob versioned hashes MUST be empty list.
    #[method(name = "newPayloadV3")]
    async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> RpcResult<PayloadStatus>;

    /// Sends the given payload to the execution layer client, as specified for the Prague fork.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/03911ffc053b8b806123f1fc237184b0092a485a/src/engine/prague.md#engine_newpayloadv4>
    ///
    /// - blob versioned hashes MUST be empty list.
    /// - execution layer requests MUST be empty list.
    #[method(name = "newPayloadV4")]
    async fn new_payload_v4(
        &self,
        payload: OpExecutionPayloadV4,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Requests,
    ) -> RpcResult<PayloadStatus>;

    /// See also <https://github.com/ethereum/execution-apis/blob/6709c2a795b707202e93c4f2867fa0bf2640a84f/src/engine/paris.md#engine_forkchoiceupdatedv1>
    ///
    /// This exists because it is used by op-node: <https://github.com/ethereum-optimism/optimism/blob/0bc5fe8d16155dc68bcdf1fa5733abc58689a618/op-node/rollup/types.go#L615-L617>
    ///
    /// Caution: This should not accept the `withdrawals` field in the payload attributes.
    #[method(name = "forkchoiceUpdatedV1")]
    async fn fork_choice_updated_v1(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<Engine::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated>;

    /// Updates the execution layer client with the given fork choice, as specified for the Shanghai
    /// fork.
    ///
    /// Caution: This should not accept the `parentBeaconBlockRoot` field in the payload attributes.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/6709c2a795b707202e93c4f2867fa0bf2640a84f/src/engine/shanghai.md#engine_forkchoiceupdatedv2>
    ///
    /// OP modifications:
    /// - The `payload_attributes` parameter is extended with the [`EngineTypes::PayloadAttributes`](EngineTypes) type as described in <https://specs.optimism.io/protocol/exec-engine.html#extended-payloadattributesv2>
    #[method(name = "forkchoiceUpdatedV2")]
    async fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<Engine::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated>;

    /// Updates the execution layer client with the given fork choice, as specified for the Cancun
    /// fork.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/main/src/engine/cancun.md#engine_forkchoiceupdatedv3>
    ///
    /// OP modifications:
    /// - Must be called with an Ecotone payload
    /// - Attributes must contain the parent beacon block root field
    /// - The `payload_attributes` parameter is extended with the [`EngineTypes::PayloadAttributes`](EngineTypes) type as described in <https://specs.optimism.io/protocol/exec-engine.html#extended-payloadattributesv2>
    #[method(name = "forkchoiceUpdatedV3")]
    async fn fork_choice_updated_v3(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<Engine::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated>;

    /// Retrieves an execution payload from a previously started build process, as specified for the
    /// Shanghai fork.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/6709c2a795b707202e93c4f2867fa0bf2640a84f/src/engine/shanghai.md#engine_getpayloadv2>
    ///
    /// Note:
    /// > Provider software MAY stop the corresponding build process after serving this call.
    ///
    /// No modifications needed for OP compatibility.
    #[method(name = "getPayloadV2")]
    async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<Engine::ExecutionPayloadEnvelopeV2>;

    /// Retrieves an execution payload from a previously started build process, as specified for the
    /// Cancun fork.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/main/src/engine/cancun.md#engine_getpayloadv3>
    ///
    /// Note:
    /// > Provider software MAY stop the corresponding build process after serving this call.
    ///
    /// OP modifications:
    /// - the response type is extended to [`EngineTypes::ExecutionPayloadEnvelopeV3`].
    #[method(name = "getPayloadV3")]
    async fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<Engine::ExecutionPayloadEnvelopeV3>;

    /// Returns the most recent version of the payload that is available in the corresponding
    /// payload build process at the time of receiving this call.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/main/src/engine/prague.md#engine_getpayloadv4>
    ///
    /// Note:
    /// > Provider software MAY stop the corresponding build process after serving this call.
    ///
    /// OP modifications:
    /// - the response type is extended to [`EngineTypes::ExecutionPayloadEnvelopeV4`].
    #[method(name = "getPayloadV4")]
    async fn get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<Engine::ExecutionPayloadEnvelopeV4>;

    /// Returns the execution payload bodies by the given hash.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/6452a6b194d7db269bf1dbd087a267251d3cc7f8/src/engine/shanghai.md#engine_getpayloadbodiesbyhashv1>
    #[method(name = "getPayloadBodiesByHashV1")]
    async fn get_payload_bodies_by_hash_v1(
        &self,
        block_hashes: Vec<BlockHash>,
    ) -> RpcResult<ExecutionPayloadBodiesV1>;

    /// Returns the execution payload bodies by the range starting at `start`, containing `count`
    /// blocks.
    ///
    /// WARNING: This method is associated with the `BeaconBlocksByRange` message in the consensus
    /// layer p2p specification, meaning the input should be treated as untrusted or potentially
    /// adversarial.
    ///
    /// Implementers should take care when acting on the input to this method, specifically
    /// ensuring that the range is limited properly, and that the range boundaries are computed
    /// correctly and without panics.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/6452a6b194d7db269bf1dbd087a267251d3cc7f8/src/engine/shanghai.md#engine_getpayloadbodiesbyrangev1>
    #[method(name = "getPayloadBodiesByRangeV1")]
    async fn get_payload_bodies_by_range_v1(
        &self,
        start: U64,
        count: U64,
    ) -> RpcResult<ExecutionPayloadBodiesV1>;

    /// Signals superchain information to the Engine.
    /// Returns the latest supported OP-Stack protocol version of the execution engine.
    /// See also <https://specs.optimism.io/protocol/exec-engine.html#engine_signalsuperchainv1>
    #[method(name = "engine_signalSuperchainV1")]
    async fn signal_superchain_v1(&self, _signal: SuperchainSignal) -> RpcResult<ProtocolVersion>;

    /// Returns the execution client version information.
    ///
    /// Note:
    /// > The `client_version` parameter identifies the consensus client.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/main/src/engine/identification.md#engine_getclientversionv1>
    #[method(name = "getClientVersionV1")]
    async fn get_client_version_v1(
        &self,
        client_version: ClientVersionV1,
    ) -> RpcResult<Vec<ClientVersionV1>>;

    /// Returns the list of Engine API methods supported by the execution layer client software.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/6452a6b194d7db269bf1dbd087a267251d3cc7f8/src/engine/common.md#capabilities>
    #[method(name = "exchangeCapabilities")]
    async fn exchange_capabilities(&self, capabilities: Vec<String>) -> RpcResult<Vec<String>>;
}

/// The Engine API implementation that grants the Consensus layer access to data and
/// functions in the Execution layer that are crucial for the consensus process.
#[derive(Debug, Constructor)]
pub struct OpEngineApi<Provider, EngineT: EngineTypes, Pool, Validator, ChainSpec> {
    inner: EngineApi<Provider, EngineT, Pool, Validator, ChainSpec>,
}

impl<Provider, PayloadT, Pool, Validator, ChainSpec> Clone
    for OpEngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>
where
    PayloadT: EngineTypes,
{
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

#[async_trait::async_trait]
impl<Provider, EngineT, Pool, Validator, ChainSpec> OpEngineApiServer<EngineT>
    for OpEngineApi<Provider, EngineT, Pool, Validator, ChainSpec>
where
    Provider: HeaderProvider + BlockReader + StateProviderFactory + 'static,
    EngineT: EngineTypes<ExecutionData = OpExecutionData>,
    Pool: TransactionPool + 'static,
    Validator: EngineApiValidator<EngineT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    async fn new_payload_v2(&self, payload: ExecutionPayloadInputV2) -> RpcResult<PayloadStatus> {
        trace!(target: "rpc::engine", "Serving engine_newPayloadV2");
        let payload = OpExecutionData::v2(payload);
        Ok(self.inner.new_payload_v2_metered(payload).await?)
    }

    async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> RpcResult<PayloadStatus> {
        trace!(target: "rpc::engine", "Serving engine_newPayloadV3");
        let payload = OpExecutionData::v3(payload, versioned_hashes, parent_beacon_block_root);

        Ok(self.inner.new_payload_v3_metered(payload).await?)
    }

    async fn new_payload_v4(
        &self,
        payload: OpExecutionPayloadV4,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Requests,
    ) -> RpcResult<PayloadStatus> {
        trace!(target: "rpc::engine", "Serving engine_newPayloadV4");
        let payload = OpExecutionData::v4(
            payload,
            versioned_hashes,
            parent_beacon_block_root,
            execution_requests,
        );

        Ok(self.inner.new_payload_v4_metered(payload).await?)
    }

    async fn fork_choice_updated_v1(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        Ok(self.inner.fork_choice_updated_v1_metered(fork_choice_state, payload_attributes).await?)
    }

    async fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        trace!(target: "rpc::engine", "Serving engine_forkchoiceUpdatedV2");
        Ok(self.inner.fork_choice_updated_v2_metered(fork_choice_state, payload_attributes).await?)
    }

    async fn fork_choice_updated_v3(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        trace!(target: "rpc::engine", "Serving engine_forkchoiceUpdatedV3");
        Ok(self.inner.fork_choice_updated_v3_metered(fork_choice_state, payload_attributes).await?)
    }

    async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV2> {
        debug!(target: "rpc::engine", id = %payload_id, "Serving engine_getPayloadV2");
        Ok(self.inner.get_payload_v2_metered(payload_id).await?)
    }

    async fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV3> {
        trace!(target: "rpc::engine", "Serving engine_getPayloadV3");
        Ok(self.inner.get_payload_v3_metered(payload_id).await?)
    }

    async fn get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV4> {
        trace!(target: "rpc::engine", "Serving engine_getPayloadV4");
        Ok(self.inner.get_payload_v4_metered(payload_id).await?)
    }

    async fn get_payload_bodies_by_hash_v1(
        &self,
        block_hashes: Vec<BlockHash>,
    ) -> RpcResult<ExecutionPayloadBodiesV1> {
        trace!(target: "rpc::engine", "Serving engine_getPayloadBodiesByHashV1");
        Ok(self.inner.get_payload_bodies_by_hash_v1_metered(block_hashes).await?)
    }

    async fn get_payload_bodies_by_range_v1(
        &self,
        start: U64,
        count: U64,
    ) -> RpcResult<ExecutionPayloadBodiesV1> {
        trace!(target: "rpc::engine", "Serving engine_getPayloadBodiesByRangeV1");
        Ok(self.inner.get_payload_bodies_by_range_v1_metered(start.to(), count.to()).await?)
    }

    async fn signal_superchain_v1(&self, signal: SuperchainSignal) -> RpcResult<ProtocolVersion> {
        trace!(target: "rpc::engine", "Serving signal_superchain_v1");
        info!(
            target: "rpc::engine",
            "Received superchain version signal local={:?} required={:?} recommended={:?}",
            OP_STACK_SUPPORT,
            signal.required,
            signal.recommended
        );
        Ok(OP_STACK_SUPPORT)
    }

    async fn get_client_version_v1(
        &self,
        client: ClientVersionV1,
    ) -> RpcResult<Vec<ClientVersionV1>> {
        trace!(target: "rpc::engine", "Serving engine_getClientVersionV1");
        Ok(self.inner.get_client_version_v1(client)?)
    }

    async fn exchange_capabilities(&self, _capabilities: Vec<String>) -> RpcResult<Vec<String>> {
        Ok(self.inner.capabilities().list())
    }
}

impl<Provider, EngineT, Pool, Validator, ChainSpec> IntoEngineApiRpcModule
    for OpEngineApi<Provider, EngineT, Pool, Validator, ChainSpec>
where
    EngineT: EngineTypes,
    Self: OpEngineApiServer<EngineT>,
{
    fn into_rpc_module(self) -> RpcModule<()> {
        self.into_rpc().remove_context()
    }
}
