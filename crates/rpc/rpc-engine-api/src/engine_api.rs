use crate::{
    capabilities::EngineCapabilities, metrics::EngineApiMetrics, EngineApiError, EngineApiResult,
};
use alloy_eips::{
    eip1898::BlockHashOrNumber,
    eip4844::{BlobAndProofV1, BlobAndProofV2},
    eip4895::Withdrawals,
    eip7685::RequestsOrHash,
};
use alloy_primitives::{BlockHash, BlockNumber, B256, U64};
use alloy_rpc_types_engine::{
    CancunPayloadFields, ClientVersionV1, ExecutionData, ExecutionPayloadBodiesV1,
    ExecutionPayloadBodyV1, ExecutionPayloadInputV2, ExecutionPayloadSidecar, ExecutionPayloadV1,
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadId, PayloadStatus,
    PraguePayloadFields,
};
use async_trait::async_trait;
use jsonrpsee_core::{server::RpcModule, RpcResult};
use parking_lot::Mutex;
use reth_chainspec::EthereumHardforks;
use reth_engine_primitives::{BeaconConsensusEngineHandle, EngineApiValidator, EngineTypes};
use reth_payload_builder::PayloadStore;
use reth_payload_primitives::{
    validate_payload_timestamp, EngineApiMessageVersion, ExecutionPayload,
    PayloadBuilderAttributes, PayloadOrAttributes, PayloadTypes,
};
use reth_primitives_traits::{Block, BlockBody};
use reth_rpc_api::{EngineApiServer, IntoEngineApiRpcModule};
use reth_storage_api::{BlockReader, HeaderProvider, StateProviderFactory};
use reth_tasks::TaskSpawner;
use reth_transaction_pool::TransactionPool;
use std::{sync::Arc, time::Instant};
use tokio::sync::oneshot;
use tracing::{debug, trace, warn};

/// The Engine API response sender.
pub type EngineApiSender<Ok> = oneshot::Sender<EngineApiResult<Ok>>;

/// The upper limit for payload bodies request.
const MAX_PAYLOAD_BODIES_LIMIT: u64 = 1024;

/// The upper limit for blobs in `engine_getBlobsVx`.
const MAX_BLOB_LIMIT: usize = 128;

/// The Engine API implementation that grants the Consensus layer access to data and
/// functions in the Execution layer that are crucial for the consensus process.
///
/// This type is generic over [`EngineTypes`] and intended to be used as the entrypoint for engine
/// API processing. It can be reused by other non L1 engine APIs that deviate from the L1 spec but
/// are still follow the engine API model.
///
/// ## Implementers
///
/// Implementing support for an engine API jsonrpsee RPC handler is done by defining the engine API
/// server trait and implementing it on a type that can either wrap this [`EngineApi`] type or
/// use a custom [`EngineTypes`] implementation if it mirrors ethereum's versioned engine API
/// endpoints (e.g. opstack).
/// See also [`EngineApiServer`] implementation for this type which is the
/// L1 implementation.
pub struct EngineApi<Provider, PayloadT: PayloadTypes, Pool, Validator, ChainSpec> {
    inner: Arc<EngineApiInner<Provider, PayloadT, Pool, Validator, ChainSpec>>,
}

impl<Provider, PayloadT: PayloadTypes, Pool, Validator, ChainSpec>
    EngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>
{
    /// Returns the configured chainspec.
    pub fn chain_spec(&self) -> &Arc<ChainSpec> {
        &self.inner.chain_spec
    }
}

impl<Provider, PayloadT, Pool, Validator, ChainSpec>
    EngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>
where
    Provider: HeaderProvider + BlockReader + StateProviderFactory + 'static,
    PayloadT: PayloadTypes,
    Pool: TransactionPool + 'static,
    Validator: EngineApiValidator<PayloadT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    /// Create new instance of [`EngineApi`].
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        provider: Provider,
        chain_spec: Arc<ChainSpec>,
        beacon_consensus: BeaconConsensusEngineHandle<PayloadT>,
        payload_store: PayloadStore<PayloadT>,
        tx_pool: Pool,
        task_spawner: Box<dyn TaskSpawner>,
        client: ClientVersionV1,
        capabilities: EngineCapabilities,
        validator: Validator,
        accept_execution_requests_hash: bool,
    ) -> Self {
        let inner = Arc::new(EngineApiInner {
            provider,
            chain_spec,
            beacon_consensus,
            payload_store,
            task_spawner,
            metrics: EngineApiMetrics::default(),
            client,
            capabilities,
            tx_pool,
            validator,
            latest_new_payload_response: Mutex::new(None),
            accept_execution_requests_hash,
        });
        Self { inner }
    }

    /// Fetches the client version.
    pub fn get_client_version_v1(
        &self,
        _client: ClientVersionV1,
    ) -> EngineApiResult<Vec<ClientVersionV1>> {
        Ok(vec![self.inner.client.clone()])
    }

    /// Fetches the attributes for the payload with the given id.
    async fn get_payload_attributes(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<PayloadT::PayloadBuilderAttributes> {
        Ok(self
            .inner
            .payload_store
            .payload_attributes(payload_id)
            .await
            .ok_or(EngineApiError::UnknownPayload)??)
    }

    /// See also <https://github.com/ethereum/execution-apis/blob/3d627c95a4d3510a8187dd02e0250ecb4331d27e/src/engine/paris.md#engine_newpayloadv1>
    /// Caution: This should not accept the `withdrawals` field
    pub async fn new_payload_v1(
        &self,
        payload: PayloadT::ExecutionData,
    ) -> EngineApiResult<PayloadStatus> {
        let payload_or_attrs = PayloadOrAttributes::<
            '_,
            PayloadT::ExecutionData,
            PayloadT::PayloadAttributes,
        >::from_execution_payload(&payload);

        self.inner
            .validator
            .validate_version_specific_fields(EngineApiMessageVersion::V1, payload_or_attrs)?;

        Ok(self
            .inner
            .beacon_consensus
            .new_payload(payload)
            .await
            .inspect(|_| self.inner.on_new_payload_response())?)
    }

    /// Metered version of `new_payload_v1`.
    pub async fn new_payload_v1_metered(
        &self,
        payload: PayloadT::ExecutionData,
    ) -> EngineApiResult<PayloadStatus> {
        let start = Instant::now();
        let gas_used = payload.gas_used();

        let res = Self::new_payload_v1(self, payload).await;
        let elapsed = start.elapsed();
        self.inner.metrics.latency.new_payload_v1.record(elapsed);
        self.inner.metrics.new_payload_response.update_response_metrics(&res, gas_used, elapsed);
        res
    }

    /// See also <https://github.com/ethereum/execution-apis/blob/584905270d8ad665718058060267061ecfd79ca5/src/engine/shanghai.md#engine_newpayloadv2>
    pub async fn new_payload_v2(
        &self,
        payload: PayloadT::ExecutionData,
    ) -> EngineApiResult<PayloadStatus> {
        let payload_or_attrs = PayloadOrAttributes::<
            '_,
            PayloadT::ExecutionData,
            PayloadT::PayloadAttributes,
        >::from_execution_payload(&payload);
        self.inner
            .validator
            .validate_version_specific_fields(EngineApiMessageVersion::V2, payload_or_attrs)?;
        Ok(self
            .inner
            .beacon_consensus
            .new_payload(payload)
            .await
            .inspect(|_| self.inner.on_new_payload_response())?)
    }

    /// Metered version of `new_payload_v2`.
    pub async fn new_payload_v2_metered(
        &self,
        payload: PayloadT::ExecutionData,
    ) -> EngineApiResult<PayloadStatus> {
        let start = Instant::now();
        let gas_used = payload.gas_used();

        let res = Self::new_payload_v2(self, payload).await;
        let elapsed = start.elapsed();
        self.inner.metrics.latency.new_payload_v2.record(elapsed);
        self.inner.metrics.new_payload_response.update_response_metrics(&res, gas_used, elapsed);
        res
    }

    /// See also <https://github.com/ethereum/execution-apis/blob/fe8e13c288c592ec154ce25c534e26cb7ce0530d/src/engine/cancun.md#engine_newpayloadv3>
    pub async fn new_payload_v3(
        &self,
        payload: PayloadT::ExecutionData,
    ) -> EngineApiResult<PayloadStatus> {
        let payload_or_attrs = PayloadOrAttributes::<
            '_,
            PayloadT::ExecutionData,
            PayloadT::PayloadAttributes,
        >::from_execution_payload(&payload);
        self.inner
            .validator
            .validate_version_specific_fields(EngineApiMessageVersion::V3, payload_or_attrs)?;

        Ok(self
            .inner
            .beacon_consensus
            .new_payload(payload)
            .await
            .inspect(|_| self.inner.on_new_payload_response())?)
    }

    /// Metrics version of `new_payload_v3`
    pub async fn new_payload_v3_metered(
        &self,
        payload: PayloadT::ExecutionData,
    ) -> RpcResult<PayloadStatus> {
        let start = Instant::now();
        let gas_used = payload.gas_used();

        let res = Self::new_payload_v3(self, payload).await;
        let elapsed = start.elapsed();
        self.inner.metrics.latency.new_payload_v3.record(elapsed);
        self.inner.metrics.new_payload_response.update_response_metrics(&res, gas_used, elapsed);
        Ok(res?)
    }

    /// See also <https://github.com/ethereum/execution-apis/blob/7907424db935b93c2fe6a3c0faab943adebe8557/src/engine/prague.md#engine_newpayloadv4>
    pub async fn new_payload_v4(
        &self,
        payload: PayloadT::ExecutionData,
    ) -> EngineApiResult<PayloadStatus> {
        let payload_or_attrs = PayloadOrAttributes::<
            '_,
            PayloadT::ExecutionData,
            PayloadT::PayloadAttributes,
        >::from_execution_payload(&payload);
        self.inner
            .validator
            .validate_version_specific_fields(EngineApiMessageVersion::V4, payload_or_attrs)?;

        Ok(self
            .inner
            .beacon_consensus
            .new_payload(payload)
            .await
            .inspect(|_| self.inner.on_new_payload_response())?)
    }

    /// Metrics version of `new_payload_v4`
    pub async fn new_payload_v4_metered(
        &self,
        payload: PayloadT::ExecutionData,
    ) -> RpcResult<PayloadStatus> {
        let start = Instant::now();
        let gas_used = payload.gas_used();

        let res = Self::new_payload_v4(self, payload).await;

        let elapsed = start.elapsed();
        self.inner.metrics.latency.new_payload_v4.record(elapsed);
        self.inner.metrics.new_payload_response.update_response_metrics(&res, gas_used, elapsed);
        Ok(res?)
    }

    /// Returns whether the engine accepts execution requests hash.
    pub fn accept_execution_requests_hash(&self) -> bool {
        self.inner.accept_execution_requests_hash
    }
}

impl<Provider, EngineT, Pool, Validator, ChainSpec>
    EngineApi<Provider, EngineT, Pool, Validator, ChainSpec>
where
    Provider: HeaderProvider + BlockReader + StateProviderFactory + 'static,
    EngineT: EngineTypes,
    Pool: TransactionPool + 'static,
    Validator: EngineApiValidator<EngineT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    /// Sends a message to the beacon consensus engine to update the fork choice _without_
    /// withdrawals.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/3d627c95a4d3510a8187dd02e0250ecb4331d27e/src/engine/paris.md#engine_forkchoiceUpdatedV1>
    ///
    /// Caution: This should not accept the `withdrawals` field
    pub async fn fork_choice_updated_v1(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<EngineT::PayloadAttributes>,
    ) -> EngineApiResult<ForkchoiceUpdated> {
        self.validate_and_execute_forkchoice(EngineApiMessageVersion::V1, state, payload_attrs)
            .await
    }

    /// Metrics version of `fork_choice_updated_v1`
    pub async fn fork_choice_updated_v1_metered(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<EngineT::PayloadAttributes>,
    ) -> EngineApiResult<ForkchoiceUpdated> {
        let start = Instant::now();
        let res = Self::fork_choice_updated_v1(self, state, payload_attrs).await;
        self.inner.metrics.latency.fork_choice_updated_v1.record(start.elapsed());
        self.inner.metrics.fcu_response.update_response_metrics(&res);
        res
    }

    /// Sends a message to the beacon consensus engine to update the fork choice _with_ withdrawals,
    /// but only _after_ shanghai.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/3d627c95a4d3510a8187dd02e0250ecb4331d27e/src/engine/shanghai.md#engine_forkchoiceupdatedv2>
    pub async fn fork_choice_updated_v2(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<EngineT::PayloadAttributes>,
    ) -> EngineApiResult<ForkchoiceUpdated> {
        self.validate_and_execute_forkchoice(EngineApiMessageVersion::V2, state, payload_attrs)
            .await
    }

    /// Metrics version of `fork_choice_updated_v2`
    pub async fn fork_choice_updated_v2_metered(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<EngineT::PayloadAttributes>,
    ) -> EngineApiResult<ForkchoiceUpdated> {
        let start = Instant::now();
        let res = Self::fork_choice_updated_v2(self, state, payload_attrs).await;
        self.inner.metrics.latency.fork_choice_updated_v2.record(start.elapsed());
        self.inner.metrics.fcu_response.update_response_metrics(&res);
        res
    }

    /// Sends a message to the beacon consensus engine to update the fork choice _with_ withdrawals,
    /// but only _after_ cancun.
    ///
    /// See also  <https://github.com/ethereum/execution-apis/blob/main/src/engine/cancun.md#engine_forkchoiceupdatedv3>
    pub async fn fork_choice_updated_v3(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<EngineT::PayloadAttributes>,
    ) -> EngineApiResult<ForkchoiceUpdated> {
        self.validate_and_execute_forkchoice(EngineApiMessageVersion::V3, state, payload_attrs)
            .await
    }

    /// Metrics version of `fork_choice_updated_v3`
    pub async fn fork_choice_updated_v3_metered(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<EngineT::PayloadAttributes>,
    ) -> EngineApiResult<ForkchoiceUpdated> {
        let start = Instant::now();
        let res = Self::fork_choice_updated_v3(self, state, payload_attrs).await;
        self.inner.metrics.latency.fork_choice_updated_v3.record(start.elapsed());
        self.inner.metrics.fcu_response.update_response_metrics(&res);
        res
    }

    /// Helper function for retrieving the build payload by id.
    async fn get_built_payload(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<EngineT::BuiltPayload> {
        self.inner
            .payload_store
            .resolve(payload_id)
            .await
            .ok_or(EngineApiError::UnknownPayload)?
            .map_err(|_| EngineApiError::UnknownPayload)
    }

    /// Helper function for validating the payload timestamp and retrieving & converting the payload
    /// into desired envelope.
    async fn get_payload_inner<R>(
        &self,
        payload_id: PayloadId,
        version: EngineApiMessageVersion,
    ) -> EngineApiResult<R>
    where
        EngineT::BuiltPayload: TryInto<R>,
    {
        // First we fetch the payload attributes to check the timestamp
        let attributes = self.get_payload_attributes(payload_id).await?;

        // validate timestamp according to engine rules
        validate_payload_timestamp(&self.inner.chain_spec, version, attributes.timestamp())?;

        // Now resolve the payload
        self.get_built_payload(payload_id).await?.try_into().map_err(|_| {
            warn!(?version, "could not transform built payload");
            EngineApiError::UnknownPayload
        })
    }

    /// Returns the most recent version of the payload that is available in the corresponding
    /// payload build process at the time of receiving this call.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/3d627c95a4d3510a8187dd02e0250ecb4331d27e/src/engine/paris.md#engine_getPayloadV1>
    ///
    /// Caution: This should not return the `withdrawals` field
    ///
    /// Note:
    /// > Provider software MAY stop the corresponding build process after serving this call.
    pub async fn get_payload_v1(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<EngineT::ExecutionPayloadEnvelopeV1> {
        self.get_built_payload(payload_id).await?.try_into().map_err(|_| {
            warn!(version = ?EngineApiMessageVersion::V1, "could not transform built payload");
            EngineApiError::UnknownPayload
        })
    }

    /// Metrics version of `get_payload_v1`
    pub async fn get_payload_v1_metered(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<EngineT::ExecutionPayloadEnvelopeV1> {
        let start = Instant::now();
        let res = Self::get_payload_v1(self, payload_id).await;
        self.inner.metrics.latency.get_payload_v1.record(start.elapsed());
        res
    }

    /// Returns the most recent version of the payload that is available in the corresponding
    /// payload build process at the time of receiving this call.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/3d627c95a4d3510a8187dd02e0250ecb4331d27e/src/engine/shanghai.md#engine_getpayloadv2>
    ///
    /// Note:
    /// > Provider software MAY stop the corresponding build process after serving this call.
    pub async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<EngineT::ExecutionPayloadEnvelopeV2> {
        self.get_payload_inner(payload_id, EngineApiMessageVersion::V2).await
    }

    /// Metrics version of `get_payload_v2`
    pub async fn get_payload_v2_metered(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<EngineT::ExecutionPayloadEnvelopeV2> {
        let start = Instant::now();
        let res = Self::get_payload_v2(self, payload_id).await;
        self.inner.metrics.latency.get_payload_v2.record(start.elapsed());
        res
    }

    /// Returns the most recent version of the payload that is available in the corresponding
    /// payload build process at the time of receiving this call.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/fe8e13c288c592ec154ce25c534e26cb7ce0530d/src/engine/cancun.md#engine_getpayloadv3>
    ///
    /// Note:
    /// > Provider software MAY stop the corresponding build process after serving this call.
    pub async fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<EngineT::ExecutionPayloadEnvelopeV3> {
        self.get_payload_inner(payload_id, EngineApiMessageVersion::V3).await
    }

    /// Metrics version of `get_payload_v3`
    pub async fn get_payload_v3_metered(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<EngineT::ExecutionPayloadEnvelopeV3> {
        let start = Instant::now();
        let res = Self::get_payload_v3(self, payload_id).await;
        self.inner.metrics.latency.get_payload_v3.record(start.elapsed());
        res
    }

    /// Returns the most recent version of the payload that is available in the corresponding
    /// payload build process at the time of receiving this call.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/7907424db935b93c2fe6a3c0faab943adebe8557/src/engine/prague.md#engine_newpayloadv4>
    ///
    /// Note:
    /// > Provider software MAY stop the corresponding build process after serving this call.
    pub async fn get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<EngineT::ExecutionPayloadEnvelopeV4> {
        self.get_payload_inner(payload_id, EngineApiMessageVersion::V4).await
    }

    /// Metrics version of `get_payload_v4`
    pub async fn get_payload_v4_metered(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<EngineT::ExecutionPayloadEnvelopeV4> {
        let start = Instant::now();
        let res = Self::get_payload_v4(self, payload_id).await;
        self.inner.metrics.latency.get_payload_v4.record(start.elapsed());
        res
    }

    /// Handler for `engine_getPayloadV5`
    ///
    /// Returns the most recent version of the payload that is available in the corresponding
    /// payload build process at the time of receiving this call.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/15399c2e2f16a5f800bf3f285640357e2c245ad9/src/engine/osaka.md#engine_getpayloadv5>
    ///
    /// Note:
    /// > Provider software MAY stop the corresponding build process after serving this call.
    pub async fn get_payload_v5(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<EngineT::ExecutionPayloadEnvelopeV5> {
        self.get_payload_inner(payload_id, EngineApiMessageVersion::V5).await
    }

    /// Metrics version of `get_payload_v5`
    pub async fn get_payload_v5_metered(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<EngineT::ExecutionPayloadEnvelopeV5> {
        let start = Instant::now();
        let res = Self::get_payload_v5(self, payload_id).await;
        self.inner.metrics.latency.get_payload_v5.record(start.elapsed());
        res
    }

    /// Fetches all the blocks for the provided range starting at `start`, containing `count`
    /// blocks and returns the mapped payload bodies.
    pub async fn get_payload_bodies_by_range_with<F, R>(
        &self,
        start: BlockNumber,
        count: u64,
        f: F,
    ) -> EngineApiResult<Vec<Option<R>>>
    where
        F: Fn(Provider::Block) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let inner = self.inner.clone();

        self.inner.task_spawner.spawn_blocking(Box::pin(async move {
            if count > MAX_PAYLOAD_BODIES_LIMIT {
                tx.send(Err(EngineApiError::PayloadRequestTooLarge { len: count })).ok();
                return;
            }

            if start == 0 || count == 0 {
                tx.send(Err(EngineApiError::InvalidBodiesRange { start, count })).ok();
                return;
            }

            let mut result = Vec::with_capacity(count as usize);

            // -1 so range is inclusive
            let mut end = start.saturating_add(count - 1);

            // > Client software MUST NOT return trailing null values if the request extends past the current latest known block.
            // truncate the end if it's greater than the last block
            if let Ok(best_block) = inner.provider.best_block_number() {
                if end > best_block {
                    end = best_block;
                }
            }

            for num in start..=end {
                let block_result = inner.provider.block(BlockHashOrNumber::Number(num));
                match block_result {
                    Ok(block) => {
                        result.push(block.map(&f));
                    }
                    Err(err) => {
                        tx.send(Err(EngineApiError::Internal(Box::new(err)))).ok();
                        return;
                    }
                };
            }
            tx.send(Ok(result)).ok();
        }));

        rx.await.map_err(|err| EngineApiError::Internal(Box::new(err)))?
    }

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
    pub async fn get_payload_bodies_by_range_v1(
        &self,
        start: BlockNumber,
        count: u64,
    ) -> EngineApiResult<ExecutionPayloadBodiesV1> {
        self.get_payload_bodies_by_range_with(start, count, |block| ExecutionPayloadBodyV1 {
            transactions: block.body().encoded_2718_transactions(),
            withdrawals: block.body().withdrawals().cloned().map(Withdrawals::into_inner),
        })
        .await
    }

    /// Metrics version of `get_payload_bodies_by_range_v1`
    pub async fn get_payload_bodies_by_range_v1_metered(
        &self,
        start: BlockNumber,
        count: u64,
    ) -> EngineApiResult<ExecutionPayloadBodiesV1> {
        let start_time = Instant::now();
        let res = Self::get_payload_bodies_by_range_v1(self, start, count).await;
        self.inner.metrics.latency.get_payload_bodies_by_range_v1.record(start_time.elapsed());
        res
    }

    /// Called to retrieve execution payload bodies by hashes.
    pub async fn get_payload_bodies_by_hash_with<F, R>(
        &self,
        hashes: Vec<BlockHash>,
        f: F,
    ) -> EngineApiResult<Vec<Option<R>>>
    where
        F: Fn(Provider::Block) -> R + Send + 'static,
        R: Send + 'static,
    {
        let len = hashes.len() as u64;
        if len > MAX_PAYLOAD_BODIES_LIMIT {
            return Err(EngineApiError::PayloadRequestTooLarge { len });
        }

        let (tx, rx) = oneshot::channel();
        let inner = self.inner.clone();

        self.inner.task_spawner.spawn_blocking(Box::pin(async move {
            let mut result = Vec::with_capacity(hashes.len());
            for hash in hashes {
                let block_result = inner.provider.block(BlockHashOrNumber::Hash(hash));
                match block_result {
                    Ok(block) => {
                        result.push(block.map(&f));
                    }
                    Err(err) => {
                        let _ = tx.send(Err(EngineApiError::Internal(Box::new(err))));
                        return;
                    }
                }
            }
            tx.send(Ok(result)).ok();
        }));

        rx.await.map_err(|err| EngineApiError::Internal(Box::new(err)))?
    }

    /// Called to retrieve execution payload bodies by hashes.
    pub async fn get_payload_bodies_by_hash_v1(
        &self,
        hashes: Vec<BlockHash>,
    ) -> EngineApiResult<ExecutionPayloadBodiesV1> {
        self.get_payload_bodies_by_hash_with(hashes, |block| ExecutionPayloadBodyV1 {
            transactions: block.body().encoded_2718_transactions(),
            withdrawals: block.body().withdrawals().cloned().map(Withdrawals::into_inner),
        })
        .await
    }

    /// Metrics version of `get_payload_bodies_by_hash_v1`
    pub async fn get_payload_bodies_by_hash_v1_metered(
        &self,
        hashes: Vec<BlockHash>,
    ) -> EngineApiResult<ExecutionPayloadBodiesV1> {
        let start = Instant::now();
        let res = Self::get_payload_bodies_by_hash_v1(self, hashes);
        self.inner.metrics.latency.get_payload_bodies_by_hash_v1.record(start.elapsed());
        res.await
    }

    /// Validates the `engine_forkchoiceUpdated` payload attributes and executes the forkchoice
    /// update.
    ///
    /// The payload attributes will be validated according to the engine API rules for the given
    /// message version:
    /// * If the version is [`EngineApiMessageVersion::V1`], then the payload attributes will be
    ///   validated according to the Paris rules.
    /// * If the version is [`EngineApiMessageVersion::V2`], then the payload attributes will be
    ///   validated according to the Shanghai rules, as well as the validity changes from cancun:
    ///   <https://github.com/ethereum/execution-apis/blob/584905270d8ad665718058060267061ecfd79ca5/src/engine/cancun.md#update-the-methods-of-previous-forks>
    ///
    /// * If the version above [`EngineApiMessageVersion::V3`], then the payload attributes will be
    ///   validated according to the Cancun rules.
    async fn validate_and_execute_forkchoice(
        &self,
        version: EngineApiMessageVersion,
        state: ForkchoiceState,
        payload_attrs: Option<EngineT::PayloadAttributes>,
    ) -> EngineApiResult<ForkchoiceUpdated> {
        self.inner.record_elapsed_time_on_fcu();

        if let Some(ref attrs) = payload_attrs {
            let attr_validation_res =
                self.inner.validator.ensure_well_formed_attributes(version, attrs);

            // From the engine API spec:
            //
            // Client software MUST ensure that payloadAttributes.timestamp is greater than
            // timestamp of a block referenced by forkchoiceState.headBlockHash. If this condition
            // isn't held client software MUST respond with -38003: Invalid payload attributes and
            // MUST NOT begin a payload build process. In such an event, the forkchoiceState
            // update MUST NOT be rolled back.
            //
            // NOTE: This will also apply to the validation result for the cancun or
            // shanghai-specific fields provided in the payload attributes.
            //
            // To do this, we set the payload attrs to `None` if attribute validation failed, but
            // we still apply the forkchoice update.
            if let Err(err) = attr_validation_res {
                let fcu_res =
                    self.inner.beacon_consensus.fork_choice_updated(state, None, version).await?;
                // TODO: decide if we want this branch - the FCU INVALID response might be more
                // useful than the payload attributes INVALID response
                if fcu_res.is_invalid() {
                    return Ok(fcu_res)
                }
                return Err(err.into())
            }
        }

        Ok(self.inner.beacon_consensus.fork_choice_updated(state, payload_attrs, version).await?)
    }

    /// Returns reference to supported capabilities.
    pub fn capabilities(&self) -> &EngineCapabilities {
        &self.inner.capabilities
    }

    fn get_blobs_v1(
        &self,
        versioned_hashes: Vec<B256>,
    ) -> EngineApiResult<Vec<Option<BlobAndProofV1>>> {
        if versioned_hashes.len() > MAX_BLOB_LIMIT {
            return Err(EngineApiError::BlobRequestTooLarge { len: versioned_hashes.len() })
        }

        self.inner
            .tx_pool
            .get_blobs_for_versioned_hashes_v1(&versioned_hashes)
            .map_err(|err| EngineApiError::Internal(Box::new(err)))
    }

    /// Metered version of `get_blobs_v1`.
    pub fn get_blobs_v1_metered(
        &self,
        versioned_hashes: Vec<B256>,
    ) -> EngineApiResult<Vec<Option<BlobAndProofV1>>> {
        let hashes_len = versioned_hashes.len();
        let start = Instant::now();
        let res = Self::get_blobs_v1(self, versioned_hashes);
        self.inner.metrics.latency.get_blobs_v1.record(start.elapsed());

        if let Ok(blobs) = &res {
            let blobs_found = blobs.iter().flatten().count();
            let blobs_missed = hashes_len - blobs_found;

            self.inner.metrics.blob_metrics.blob_count.increment(blobs_found as u64);
            self.inner.metrics.blob_metrics.blob_misses.increment(blobs_missed as u64);
        }

        res
    }

    fn get_blobs_v2(
        &self,
        versioned_hashes: Vec<B256>,
    ) -> EngineApiResult<Option<Vec<BlobAndProofV2>>> {
        if versioned_hashes.len() > MAX_BLOB_LIMIT {
            return Err(EngineApiError::BlobRequestTooLarge { len: versioned_hashes.len() })
        }

        self.inner
            .tx_pool
            .get_blobs_for_versioned_hashes_v2(&versioned_hashes)
            .map_err(|err| EngineApiError::Internal(Box::new(err)))
    }

    /// Metered version of `get_blobs_v2`.
    pub fn get_blobs_v2_metered(
        &self,
        versioned_hashes: Vec<B256>,
    ) -> EngineApiResult<Option<Vec<BlobAndProofV2>>> {
        let hashes_len = versioned_hashes.len();
        let start = Instant::now();
        let res = Self::get_blobs_v2(self, versioned_hashes);
        self.inner.metrics.latency.get_blobs_v2.record(start.elapsed());

        if let Ok(blobs) = &res {
            let blobs_found = blobs.iter().flatten().count();

            self.inner
                .metrics
                .blob_metrics
                .get_blobs_requests_blobs_total
                .increment(hashes_len as u64);
            self.inner
                .metrics
                .blob_metrics
                .get_blobs_requests_blobs_in_blobpool_total
                .increment(blobs_found as u64);

            if blobs_found == hashes_len {
                self.inner.metrics.blob_metrics.get_blobs_requests_success_total.increment(1);
            } else {
                self.inner.metrics.blob_metrics.get_blobs_requests_failure_total.increment(1);
            }
        } else {
            self.inner.metrics.blob_metrics.get_blobs_requests_failure_total.increment(1);
        }

        res
    }
}

// This is the concrete ethereum engine API implementation.
#[async_trait]
impl<Provider, EngineT, Pool, Validator, ChainSpec> EngineApiServer<EngineT>
    for EngineApi<Provider, EngineT, Pool, Validator, ChainSpec>
where
    Provider: HeaderProvider + BlockReader + StateProviderFactory + 'static,
    EngineT: EngineTypes<ExecutionData = ExecutionData>,
    Pool: TransactionPool + 'static,
    Validator: EngineApiValidator<EngineT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    /// Handler for `engine_newPayloadV1`
    /// See also <https://github.com/ethereum/execution-apis/blob/3d627c95a4d3510a8187dd02e0250ecb4331d27e/src/engine/paris.md#engine_newpayloadv1>
    /// Caution: This should not accept the `withdrawals` field
    async fn new_payload_v1(&self, payload: ExecutionPayloadV1) -> RpcResult<PayloadStatus> {
        trace!(target: "rpc::engine", "Serving engine_newPayloadV1");
        let payload =
            ExecutionData { payload: payload.into(), sidecar: ExecutionPayloadSidecar::none() };
        Ok(self.new_payload_v1_metered(payload).await?)
    }

    /// Handler for `engine_newPayloadV2`
    /// See also <https://github.com/ethereum/execution-apis/blob/584905270d8ad665718058060267061ecfd79ca5/src/engine/shanghai.md#engine_newpayloadv2>
    async fn new_payload_v2(&self, payload: ExecutionPayloadInputV2) -> RpcResult<PayloadStatus> {
        trace!(target: "rpc::engine", "Serving engine_newPayloadV2");
        let payload = ExecutionData {
            payload: payload.into_payload(),
            sidecar: ExecutionPayloadSidecar::none(),
        };

        Ok(self.new_payload_v2_metered(payload).await?)
    }

    /// Handler for `engine_newPayloadV3`
    /// See also <https://github.com/ethereum/execution-apis/blob/fe8e13c288c592ec154ce25c534e26cb7ce0530d/src/engine/cancun.md#engine_newpayloadv3>
    async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> RpcResult<PayloadStatus> {
        trace!(target: "rpc::engine", "Serving engine_newPayloadV3");
        let payload = ExecutionData {
            payload: payload.into(),
            sidecar: ExecutionPayloadSidecar::v3(CancunPayloadFields {
                versioned_hashes,
                parent_beacon_block_root,
            }),
        };

        Ok(self.new_payload_v3_metered(payload).await?)
    }

    /// Handler for `engine_newPayloadV4`
    /// See also <https://github.com/ethereum/execution-apis/blob/03911ffc053b8b806123f1fc237184b0092a485a/src/engine/prague.md#engine_newpayloadv4>
    async fn new_payload_v4(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        requests: RequestsOrHash,
    ) -> RpcResult<PayloadStatus> {
        trace!(target: "rpc::engine", "Serving engine_newPayloadV4");

        // Accept requests as a hash only if it is explicitly allowed
        if requests.is_hash() && !self.inner.accept_execution_requests_hash {
            return Err(EngineApiError::UnexpectedRequestsHash.into());
        }

        let payload = ExecutionData {
            payload: payload.into(),
            sidecar: ExecutionPayloadSidecar::v4(
                CancunPayloadFields { versioned_hashes, parent_beacon_block_root },
                PraguePayloadFields { requests },
            ),
        };

        Ok(self.new_payload_v4_metered(payload).await?)
    }

    /// Handler for `engine_forkchoiceUpdatedV1`
    /// See also <https://github.com/ethereum/execution-apis/blob/3d627c95a4d3510a8187dd02e0250ecb4331d27e/src/engine/paris.md#engine_forkchoiceupdatedv1>
    ///
    /// Caution: This should not accept the `withdrawals` field
    async fn fork_choice_updated_v1(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        trace!(target: "rpc::engine", "Serving engine_forkchoiceUpdatedV1");
        Ok(self.fork_choice_updated_v1_metered(fork_choice_state, payload_attributes).await?)
    }

    /// Handler for `engine_forkchoiceUpdatedV2`
    /// See also <https://github.com/ethereum/execution-apis/blob/3d627c95a4d3510a8187dd02e0250ecb4331d27e/src/engine/shanghai.md#engine_forkchoiceupdatedv2>
    async fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        trace!(target: "rpc::engine", "Serving engine_forkchoiceUpdatedV2");
        Ok(self.fork_choice_updated_v2_metered(fork_choice_state, payload_attributes).await?)
    }

    /// Handler for `engine_forkchoiceUpdatedV2`
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/main/src/engine/cancun.md#engine_forkchoiceupdatedv3>
    async fn fork_choice_updated_v3(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        trace!(target: "rpc::engine", "Serving engine_forkchoiceUpdatedV3");
        Ok(self.fork_choice_updated_v3_metered(fork_choice_state, payload_attributes).await?)
    }

    /// Handler for `engine_getPayloadV1`
    ///
    /// Returns the most recent version of the payload that is available in the corresponding
    /// payload build process at the time of receiving this call.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/3d627c95a4d3510a8187dd02e0250ecb4331d27e/src/engine/paris.md#engine_getPayloadV1>
    ///
    /// Caution: This should not return the `withdrawals` field
    ///
    /// Note:
    /// > Provider software MAY stop the corresponding build process after serving this call.
    async fn get_payload_v1(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV1> {
        trace!(target: "rpc::engine", "Serving engine_getPayloadV1");
        Ok(self.get_payload_v1_metered(payload_id).await?)
    }

    /// Handler for `engine_getPayloadV2`
    ///
    /// Returns the most recent version of the payload that is available in the corresponding
    /// payload build process at the time of receiving this call.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/3d627c95a4d3510a8187dd02e0250ecb4331d27e/src/engine/shanghai.md#engine_getpayloadv2>
    ///
    /// Note:
    /// > Provider software MAY stop the corresponding build process after serving this call.
    async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV2> {
        debug!(target: "rpc::engine", id = %payload_id, "Serving engine_getPayloadV2");
        Ok(self.get_payload_v2_metered(payload_id).await?)
    }

    /// Handler for `engine_getPayloadV3`
    ///
    /// Returns the most recent version of the payload that is available in the corresponding
    /// payload build process at the time of receiving this call.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/fe8e13c288c592ec154ce25c534e26cb7ce0530d/src/engine/cancun.md#engine_getpayloadv3>
    ///
    /// Note:
    /// > Provider software MAY stop the corresponding build process after serving this call.
    async fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV3> {
        trace!(target: "rpc::engine", "Serving engine_getPayloadV3");
        Ok(self.get_payload_v3_metered(payload_id).await?)
    }

    /// Handler for `engine_getPayloadV4`
    ///
    /// Returns the most recent version of the payload that is available in the corresponding
    /// payload build process at the time of receiving this call.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/main/src/engine/prague.md#engine_getpayloadv4>
    ///
    /// Note:
    /// > Provider software MAY stop the corresponding build process after serving this call.
    async fn get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV4> {
        trace!(target: "rpc::engine", "Serving engine_getPayloadV4");
        Ok(self.get_payload_v4_metered(payload_id).await?)
    }

    /// Handler for `engine_getPayloadV5`
    ///
    /// Returns the most recent version of the payload that is available in the corresponding
    /// payload build process at the time of receiving this call.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/15399c2e2f16a5f800bf3f285640357e2c245ad9/src/engine/osaka.md#engine_getpayloadv5>
    ///
    /// Note:
    /// > Provider software MAY stop the corresponding build process after serving this call.
    async fn get_payload_v5(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV5> {
        trace!(target: "rpc::engine", "Serving engine_getPayloadV5");
        Ok(self.get_payload_v5_metered(payload_id).await?)
    }

    /// Handler for `engine_getPayloadBodiesByHashV1`
    /// See also <https://github.com/ethereum/execution-apis/blob/6452a6b194d7db269bf1dbd087a267251d3cc7f8/src/engine/shanghai.md#engine_getpayloadbodiesbyhashv1>
    async fn get_payload_bodies_by_hash_v1(
        &self,
        block_hashes: Vec<BlockHash>,
    ) -> RpcResult<ExecutionPayloadBodiesV1> {
        trace!(target: "rpc::engine", "Serving engine_getPayloadBodiesByHashV1");
        Ok(self.get_payload_bodies_by_hash_v1_metered(block_hashes).await?)
    }

    /// Handler for `engine_getPayloadBodiesByRangeV1`
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/6452a6b194d7db269bf1dbd087a267251d3cc7f8/src/engine/shanghai.md#engine_getpayloadbodiesbyrangev1>
    ///
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
    /// Note: If a block is pre shanghai, `withdrawals` field will be `null`.
    async fn get_payload_bodies_by_range_v1(
        &self,
        start: U64,
        count: U64,
    ) -> RpcResult<ExecutionPayloadBodiesV1> {
        trace!(target: "rpc::engine", "Serving engine_getPayloadBodiesByRangeV1");
        Ok(self.get_payload_bodies_by_range_v1_metered(start.to(), count.to()).await?)
    }

    /// Handler for `engine_getClientVersionV1`
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/03911ffc053b8b806123f1fc237184b0092a485a/src/engine/identification.md>
    async fn get_client_version_v1(
        &self,
        client: ClientVersionV1,
    ) -> RpcResult<Vec<ClientVersionV1>> {
        trace!(target: "rpc::engine", "Serving engine_getClientVersionV1");
        Ok(Self::get_client_version_v1(self, client)?)
    }

    /// Handler for `engine_exchangeCapabilitiesV1`
    /// See also <https://github.com/ethereum/execution-apis/blob/6452a6b194d7db269bf1dbd087a267251d3cc7f8/src/engine/common.md#capabilities>
    async fn exchange_capabilities(&self, _capabilities: Vec<String>) -> RpcResult<Vec<String>> {
        Ok(self.capabilities().list())
    }

    async fn get_blobs_v1(
        &self,
        versioned_hashes: Vec<B256>,
    ) -> RpcResult<Vec<Option<BlobAndProofV1>>> {
        trace!(target: "rpc::engine", "Serving engine_getBlobsV1");
        Ok(self.get_blobs_v1_metered(versioned_hashes)?)
    }

    async fn get_blobs_v2(
        &self,
        versioned_hashes: Vec<B256>,
    ) -> RpcResult<Option<Vec<BlobAndProofV2>>> {
        trace!(target: "rpc::engine", "Serving engine_getBlobsV2");
        Ok(self.get_blobs_v2_metered(versioned_hashes)?)
    }
}

impl<Provider, EngineT, Pool, Validator, ChainSpec> IntoEngineApiRpcModule
    for EngineApi<Provider, EngineT, Pool, Validator, ChainSpec>
where
    EngineT: EngineTypes,
    Self: EngineApiServer<EngineT>,
{
    fn into_rpc_module(self) -> RpcModule<()> {
        self.into_rpc().remove_context()
    }
}

impl<Provider, PayloadT, Pool, Validator, ChainSpec> std::fmt::Debug
    for EngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>
where
    PayloadT: PayloadTypes,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineApi").finish_non_exhaustive()
    }
}

impl<Provider, PayloadT, Pool, Validator, ChainSpec> Clone
    for EngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>
where
    PayloadT: PayloadTypes,
{
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

/// The container type for the engine API internals.
struct EngineApiInner<Provider, PayloadT: PayloadTypes, Pool, Validator, ChainSpec> {
    /// The provider to interact with the chain.
    provider: Provider,
    /// Consensus configuration
    chain_spec: Arc<ChainSpec>,
    /// The channel to send messages to the beacon consensus engine.
    beacon_consensus: BeaconConsensusEngineHandle<PayloadT>,
    /// The type that can communicate with the payload service to retrieve payloads.
    payload_store: PayloadStore<PayloadT>,
    /// For spawning and executing async tasks
    task_spawner: Box<dyn TaskSpawner>,
    /// The latency and response type metrics for engine api calls
    metrics: EngineApiMetrics,
    /// Identification of the execution client used by the consensus client
    client: ClientVersionV1,
    /// The list of all supported Engine capabilities available over the engine endpoint.
    capabilities: EngineCapabilities,
    /// Transaction pool.
    tx_pool: Pool,
    /// Engine validator.
    validator: Validator,
    /// Start time of the latest payload request
    latest_new_payload_response: Mutex<Option<Instant>>,
    accept_execution_requests_hash: bool,
}

impl<Provider, PayloadT, Pool, Validator, ChainSpec>
    EngineApiInner<Provider, PayloadT, Pool, Validator, ChainSpec>
where
    PayloadT: PayloadTypes,
{
    /// Tracks the elapsed time between the new payload response and the received forkchoice update
    /// request.
    fn record_elapsed_time_on_fcu(&self) {
        if let Some(start_time) = self.latest_new_payload_response.lock().take() {
            let elapsed_time = start_time.elapsed();
            self.metrics.latency.new_payload_forkchoice_updated_time_diff.record(elapsed_time);
        }
    }

    /// Updates the timestamp for the latest new payload response.
    fn on_new_payload_response(&self) {
        self.latest_new_payload_response.lock().replace(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rpc_types_engine::{ClientCode, ClientVersionV1};
    use assert_matches::assert_matches;
    use reth_chainspec::{ChainSpec, MAINNET};
    use reth_engine_primitives::BeaconEngineMessage;
    use reth_ethereum_engine_primitives::EthEngineTypes;
    use reth_ethereum_primitives::Block;
    use reth_node_ethereum::EthereumEngineValidator;
    use reth_payload_builder::test_utils::spawn_test_payload_service;
    use reth_provider::test_utils::MockEthProvider;
    use reth_tasks::TokioTaskExecutor;
    use reth_transaction_pool::noop::NoopTransactionPool;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    fn setup_engine_api() -> (
        EngineApiTestHandle,
        EngineApi<
            Arc<MockEthProvider>,
            EthEngineTypes,
            NoopTransactionPool,
            EthereumEngineValidator,
            ChainSpec,
        >,
    ) {
        let client = ClientVersionV1 {
            code: ClientCode::RH,
            name: "Reth".to_string(),
            version: "v0.2.0-beta.5".to_string(),
            commit: "defa64b2".to_string(),
        };

        let chain_spec: Arc<ChainSpec> = MAINNET.clone();
        let provider = Arc::new(MockEthProvider::default());
        let payload_store = spawn_test_payload_service();
        let (to_engine, engine_rx) = unbounded_channel();
        let task_executor = Box::<TokioTaskExecutor>::default();
        let api = EngineApi::new(
            provider.clone(),
            chain_spec.clone(),
            BeaconConsensusEngineHandle::new(to_engine),
            payload_store.into(),
            NoopTransactionPool::default(),
            task_executor,
            client,
            EngineCapabilities::default(),
            EthereumEngineValidator::new(chain_spec.clone()),
            false,
        );
        let handle = EngineApiTestHandle { chain_spec, provider, from_api: engine_rx };
        (handle, api)
    }

    #[tokio::test]
    async fn engine_client_version_v1() {
        let client = ClientVersionV1 {
            code: ClientCode::RH,
            name: "Reth".to_string(),
            version: "v0.2.0-beta.5".to_string(),
            commit: "defa64b2".to_string(),
        };
        let (_, api) = setup_engine_api();
        let res = api.get_client_version_v1(client.clone());
        assert_eq!(res.unwrap(), vec![client]);
    }

    struct EngineApiTestHandle {
        #[allow(dead_code)]
        chain_spec: Arc<ChainSpec>,
        provider: Arc<MockEthProvider>,
        from_api: UnboundedReceiver<BeaconEngineMessage<EthEngineTypes>>,
    }

    #[tokio::test]
    async fn forwards_responses_to_consensus_engine() {
        let (mut handle, api) = setup_engine_api();

        tokio::spawn(async move {
            let payload_v1 = ExecutionPayloadV1::from_block_slow(&Block::default());
            let execution_data = ExecutionData {
                payload: payload_v1.into(),
                sidecar: ExecutionPayloadSidecar::none(),
            };

            api.new_payload_v1(execution_data).await.unwrap();
        });
        assert_matches!(handle.from_api.recv().await, Some(BeaconEngineMessage::NewPayload { .. }));
    }

    // tests covering `engine_getPayloadBodiesByRange` and `engine_getPayloadBodiesByHash`
    mod get_payload_bodies {
        use super::*;
        use alloy_rpc_types_engine::ExecutionPayloadBodyV1;
        use reth_testing_utils::generators::{self, random_block_range, BlockRangeParams};

        #[tokio::test]
        async fn invalid_params() {
            let (_, api) = setup_engine_api();

            let by_range_tests = [
                // (start, count)
                (0, 0),
                (0, 1),
                (1, 0),
            ];

            // test [EngineApiMessage::GetPayloadBodiesByRange]
            for (start, count) in by_range_tests {
                let res = api.get_payload_bodies_by_range_v1(start, count).await;
                assert_matches!(res, Err(EngineApiError::InvalidBodiesRange { .. }));
            }
        }

        #[tokio::test]
        async fn request_too_large() {
            let (_, api) = setup_engine_api();

            let request_count = MAX_PAYLOAD_BODIES_LIMIT + 1;
            let res = api.get_payload_bodies_by_range_v1(0, request_count).await;
            assert_matches!(res, Err(EngineApiError::PayloadRequestTooLarge { .. }));
        }

        #[tokio::test]
        async fn returns_payload_bodies() {
            let mut rng = generators::rng();
            let (handle, api) = setup_engine_api();

            let (start, count) = (1, 10);
            let blocks = random_block_range(
                &mut rng,
                start..=start + count - 1,
                BlockRangeParams { tx_count: 0..2, ..Default::default() },
            );
            handle
                .provider
                .extend_blocks(blocks.iter().cloned().map(|b| (b.hash(), b.into_block())));

            let expected = blocks
                .iter()
                .cloned()
                .map(|b| Some(ExecutionPayloadBodyV1::from_block(b.into_block())))
                .collect::<Vec<_>>();

            let res = api.get_payload_bodies_by_range_v1(start, count).await.unwrap();
            assert_eq!(res, expected);
        }

        #[tokio::test]
        async fn returns_payload_bodies_with_gaps() {
            let mut rng = generators::rng();
            let (handle, api) = setup_engine_api();

            let (start, count) = (1, 100);
            let blocks = random_block_range(
                &mut rng,
                start..=start + count - 1,
                BlockRangeParams { tx_count: 0..2, ..Default::default() },
            );

            // Insert only blocks in ranges 1-25 and 50-75
            let first_missing_range = 26..=50;
            let second_missing_range = 76..=100;
            handle.provider.extend_blocks(
                blocks
                    .iter()
                    .filter(|b| {
                        !first_missing_range.contains(&b.number) &&
                            !second_missing_range.contains(&b.number)
                    })
                    .map(|b| (b.hash(), b.clone().into_block())),
            );

            let expected = blocks
                .iter()
                // filter anything after the second missing range to ensure we don't expect trailing
                // `None`s
                .filter(|b| !second_missing_range.contains(&b.number))
                .cloned()
                .map(|b| {
                    if first_missing_range.contains(&b.number) {
                        None
                    } else {
                        Some(ExecutionPayloadBodyV1::from_block(b.into_block()))
                    }
                })
                .collect::<Vec<_>>();

            let res = api.get_payload_bodies_by_range_v1(start, count).await.unwrap();
            assert_eq!(res, expected);

            let expected = blocks
                .iter()
                .cloned()
                // ensure we still return trailing `None`s here because by-hash will not be aware
                // of the missing block's number, and cannot compare it to the current best block
                .map(|b| {
                    if first_missing_range.contains(&b.number) ||
                        second_missing_range.contains(&b.number)
                    {
                        None
                    } else {
                        Some(ExecutionPayloadBodyV1::from_block(b.into_block()))
                    }
                })
                .collect::<Vec<_>>();

            let hashes = blocks.iter().map(|b| b.hash()).collect();
            let res = api.get_payload_bodies_by_hash_v1(hashes).await.unwrap();
            assert_eq!(res, expected);
        }
    }
}
