// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{pin::Pin, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use consensus_config::AuthorityIndex;
use futures::{ready, stream, task, Stream, StreamExt};
use parking_lot::RwLock;
use sui_macros::{fail_point, fail_point_async};
use tokio::{sync::broadcast, time::sleep};
use tokio_util::sync::ReusableBoxFuture;
use tracing::{debug, info, warn};

use crate::{
    block::{BlockAPI as _, BlockRef, SignedBlock, VerifiedBlock, GENESIS_ROUND},
    block_verifier::BlockVerifier,
    commit::{CommitAPI as _, TrustedCommit},
    commit_syncer::CommitVoteMonitor,
    context::Context,
    core_thread::CoreThreadDispatcher,
    dag_state::DagState,
    error::{ConsensusError, ConsensusResult},
    network::{BlockStream, NetworkService},
    stake_aggregator::{QuorumThreshold, StakeAggregator},
    storage::Store,
    synchronizer::SynchronizerHandle,
    CommitIndex, Round,
};

/// Authority's network service implementation, agnostic to the actual networking stack used.
pub(crate) struct AuthorityService<C: CoreThreadDispatcher> {
    context: Arc<Context>,
    commit_vote_monitor: Arc<CommitVoteMonitor>,
    block_verifier: Arc<dyn BlockVerifier>,
    synchronizer: Arc<SynchronizerHandle>,
    core_dispatcher: Arc<C>,
    rx_block_broadcaster: broadcast::Receiver<VerifiedBlock>,
    dag_state: Arc<RwLock<DagState>>,
    store: Arc<dyn Store>,
}

impl<C: CoreThreadDispatcher> AuthorityService<C> {
    pub(crate) fn new(
        context: Arc<Context>,
        block_verifier: Arc<dyn BlockVerifier>,
        commit_vote_monitor: Arc<CommitVoteMonitor>,
        synchronizer: Arc<SynchronizerHandle>,
        core_dispatcher: Arc<C>,
        rx_block_broadcaster: broadcast::Receiver<VerifiedBlock>,
        dag_state: Arc<RwLock<DagState>>,
        store: Arc<dyn Store>,
    ) -> Self {
        Self {
            context,
            block_verifier,
            commit_vote_monitor,
            synchronizer,
            core_dispatcher,
            rx_block_broadcaster,
            dag_state,
            store,
        }
    }
}

#[async_trait]
impl<C: CoreThreadDispatcher> NetworkService for AuthorityService<C> {
    async fn handle_send_block(
        &self,
        peer: AuthorityIndex,
        serialized_block: Bytes,
    ) -> ConsensusResult<()> {
        fail_point_async!("consensus-rpc-response");

        let peer_hostname = &self.context.committee.authority(peer).hostname;

        // TODO: dedup block verifications, here and with fetched blocks.
        let signed_block: SignedBlock =
            bcs::from_bytes(&serialized_block).map_err(ConsensusError::MalformedBlock)?;

        // Reject blocks not produced by the peer.
        if peer != signed_block.author() {
            self.context
                .metrics
                .node_metrics
                .invalid_blocks
                .with_label_values(&[peer_hostname, "handle_send_block"])
                .inc();
            let e = ConsensusError::UnexpectedAuthority(signed_block.author(), peer);
            info!("Block with wrong authority from {}: {}", peer, e);
            return Err(e);
        }
        let peer_hostname = &self.context.committee.authority(peer).hostname;

        // Reject blocks failing validations.
        if let Err(e) = self.block_verifier.verify(&signed_block) {
            self.context
                .metrics
                .node_metrics
                .invalid_blocks
                .with_label_values(&[peer_hostname, "handle_send_block"])
                .inc();
            info!("Invalid block from {}: {}", peer, e);
            return Err(e);
        }
        let verified_block = VerifiedBlock::new_verified(signed_block, serialized_block);

        // Reject block with timestamp too far in the future.
        let now = self.context.clock.timestamp_utc_ms();
        let forward_time_drift =
            Duration::from_millis(verified_block.timestamp_ms().saturating_sub(now));
        if forward_time_drift > self.context.parameters.max_forward_time_drift {
            self.context
                .metrics
                .node_metrics
                .rejected_future_blocks
                .with_label_values(&[&peer_hostname])
                .inc();
            debug!(
                "Block {:?} timestamp ({} > {}) is too far in the future, rejected.",
                verified_block.reference(),
                verified_block.timestamp_ms(),
                now,
            );
            return Err(ConsensusError::BlockRejected {
                block_ref: verified_block.reference(),
                reason: format!(
                    "Block timestamp is too far in the future: {} > {}",
                    verified_block.timestamp_ms(),
                    now
                ),
            });
        }

        // Wait until the block's timestamp is current.
        if forward_time_drift > Duration::ZERO {
            self.context
                .metrics
                .node_metrics
                .block_timestamp_drift_wait_ms
                .with_label_values(&[peer_hostname, &"handle_send_block"])
                .inc_by(forward_time_drift.as_millis() as u64);
            debug!(
                "Block {:?} timestamp ({} > {}) is in the future, waiting for {}ms",
                verified_block.reference(),
                verified_block.timestamp_ms(),
                now,
                forward_time_drift.as_millis(),
            );
            sleep(forward_time_drift).await;
        }

        // Observe the block for the commit votes. When local commit is lagging too much,
        // commit sync loop will trigger fetching.
        self.commit_vote_monitor.observe(&verified_block);

        // Reject blocks when local commit index is lagging too far from quorum commit index.
        //
        // IMPORTANT: this must be done after observing votes from the block, otherwise
        // observed quorum commit will no longer progress.
        //
        // Since the main issue with too many suspended blocks is memory usage not CPU,
        // it is ok to reject after block verifications instead of before.
        let last_commit_index = self.dag_state.read().last_commit_index();
        let quorum_commit_index = self.commit_vote_monitor.quorum_commit_index();
        // The threshold to ignore block should be larger than commit_sync_batch_size,
        // to avoid excessive block rejections and synchronizations.
        const COMMIT_LAG_MULTIPLIER: u32 = 5;
        if last_commit_index
            + self.context.parameters.commit_sync_batch_size * COMMIT_LAG_MULTIPLIER
            < quorum_commit_index
        {
            self.context
                .metrics
                .node_metrics
                .rejected_blocks
                .with_label_values(&[&"commit_lagging"])
                .inc();
            debug!(
                "Block {:?} is rejected because last commit index is lagging quorum commit index too much ({} < {})",
                verified_block.reference(),
                last_commit_index,
                quorum_commit_index,
            );
            return Err(ConsensusError::BlockRejected {
                block_ref: verified_block.reference(),
                reason: format!(
                    "Last commit index is lagging quorum commit index too much ({} < {})",
                    last_commit_index, quorum_commit_index,
                ),
            });
        }

        self.context
            .metrics
            .node_metrics
            .verified_blocks
            .with_label_values(&[&peer_hostname])
            .inc();

        let missing_ancestors = self
            .core_dispatcher
            .add_blocks(vec![verified_block])
            .await
            .map_err(|_| ConsensusError::Shutdown)?;
        if !missing_ancestors.is_empty() {
            // schedule the fetching of them from this peer
            if let Err(err) = self
                .synchronizer
                .fetch_blocks(missing_ancestors, peer)
                .await
            {
                warn!("Errored while trying to fetch missing ancestors via synchronizer: {err}");
            }
        }

        Ok(())
    }

    async fn handle_subscribe_blocks(
        &self,
        peer: AuthorityIndex,
        last_received: Round,
    ) -> ConsensusResult<BlockStream> {
        fail_point_async!("consensus-rpc-response");

        let dag_state = self.dag_state.read();
        // Find recent own blocks that have not been received by the peer.
        // If last_received is a valid and more blocks have been proposed since then, this call is
        // guaranteed to return at least some recent blocks, which will help with liveness.
        let missed_blocks = stream::iter(
            dag_state
                .get_cached_blocks(self.context.own_index, last_received + 1)
                .into_iter()
                .map(|block| block.serialized().clone()),
        );
        let broadcasted_blocks =
            BroadcastedBlockStream::new(peer, self.rx_block_broadcaster.resubscribe());

        // Return a stream of blocks that first yields missed blocks as requested, then new blocks.
        Ok(Box::pin(missed_blocks.chain(
            broadcasted_blocks.map(|block| block.serialized().clone()),
        )))
    }

    async fn handle_fetch_blocks(
        &self,
        peer: AuthorityIndex,
        block_refs: Vec<BlockRef>,
        highest_accepted_rounds: Vec<Round>,
    ) -> ConsensusResult<Vec<Bytes>> {
        fail_point_async!("consensus-rpc-response");

        const MAX_ADDITIONAL_BLOCKS: usize = 10;
        if block_refs.len() > self.context.parameters.max_blocks_per_fetch {
            return Err(ConsensusError::TooManyFetchBlocksRequested(peer));
        }

        if !highest_accepted_rounds.is_empty()
            && highest_accepted_rounds.len() != self.context.committee.size()
        {
            return Err(ConsensusError::InvalidSizeOfHighestAcceptedRounds(
                highest_accepted_rounds.len(),
                self.context.committee.size(),
            ));
        }

        // Some quick validation of the requested block refs
        for block in &block_refs {
            if !self.context.committee.is_valid_index(block.author) {
                return Err(ConsensusError::InvalidAuthorityIndex {
                    index: block.author,
                    max: self.context.committee.size(),
                });
            }
            if block.round == GENESIS_ROUND {
                return Err(ConsensusError::UnexpectedGenesisBlockRequested);
            }
        }

        // For now ask dag state directly
        let blocks = self.dag_state.read().get_blocks(&block_refs);

        // Now check if an ancestor's round is higher than the one that the peer has. If yes, then serve
        // that ancestor blocks up to `MAX_ADDITIONAL_BLOCKS`.
        let mut ancestor_blocks = vec![];
        if !highest_accepted_rounds.is_empty() {
            let all_ancestors = blocks
                .iter()
                .flatten()
                .flat_map(|block| block.ancestors().to_vec())
                .filter(|block_ref| highest_accepted_rounds[block_ref.author] < block_ref.round)
                .take(MAX_ADDITIONAL_BLOCKS)
                .collect::<Vec<_>>();

            if !all_ancestors.is_empty() {
                ancestor_blocks = self.dag_state.read().get_blocks(&all_ancestors);
            }
        }

        // Return the serialised blocks & the ancestor blocks
        let result = blocks
            .into_iter()
            .chain(ancestor_blocks)
            .flatten()
            .map(|block| block.serialized().clone())
            .collect::<Vec<_>>();

        Ok(result)
    }

    async fn handle_fetch_commits(
        &self,
        _peer: AuthorityIndex,
        start: CommitIndex,
        end: CommitIndex,
    ) -> ConsensusResult<(Vec<TrustedCommit>, Vec<VerifiedBlock>)> {
        fail_point_async!("consensus-rpc-response");

        // Compute an exclusive end index and bound the maximum number of commits scanned.
        let exclusive_end =
            (end + 1).min(start + self.context.parameters.commit_sync_batch_size as CommitIndex);
        let mut commits = self.store.scan_commits((start..exclusive_end).into())?;
        let mut certifier_block_refs = vec![];
        'commit: while let Some(c) = commits.last() {
            let index = c.index();
            let votes = self.store.read_commit_votes(index)?;
            let mut stake_aggregator = StakeAggregator::<QuorumThreshold>::new();
            for v in &votes {
                stake_aggregator.add(v.author, &self.context.committee);
            }
            if stake_aggregator.reached_threshold(&self.context.committee) {
                certifier_block_refs = votes;
                break 'commit;
            } else {
                commits.pop();
            }
        }
        let certifier_blocks = self
            .store
            .read_blocks(&certifier_block_refs)?
            .into_iter()
            .flatten()
            .collect();
        Ok((commits, certifier_blocks))
    }
}

/// Each broadcasted block stream wraps a broadcast receiver for blocks.
/// It yields blocks that are broadcasted after the stream is created.
pub(crate) type BroadcastedBlockStream = BroadcastStream<VerifiedBlock>;

/// Adapted from `tokio_stream::wrappers::BroadcastStream`. The main difference is that
/// this tolerates lags with only logging, without yielding errors.
pub(crate) struct BroadcastStream<T> {
    peer: AuthorityIndex,
    // Stores the receiver across poll_next() calls.
    inner: ReusableBoxFuture<
        'static,
        (
            Result<T, broadcast::error::RecvError>,
            broadcast::Receiver<T>,
        ),
    >,
}

impl<T: 'static + Clone + Send> BroadcastStream<T> {
    pub fn new(peer: AuthorityIndex, rx: broadcast::Receiver<T>) -> Self {
        Self {
            peer,
            inner: ReusableBoxFuture::new(make_recv_future(rx)),
        }
    }
}

impl<T: 'static + Clone + Send> Stream for BroadcastStream<T> {
    type Item = T;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        let peer = self.peer;
        let maybe_item = loop {
            let (result, rx) = ready!(self.inner.poll(cx));
            self.inner.set(make_recv_future(rx));

            fail_point!("consensus-rpc-response");

            match result {
                Ok(item) => break Some(item),
                Err(broadcast::error::RecvError::Closed) => {
                    info!("Block BroadcastedBlockStream {} closed", peer);
                    break None;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        "Block BroadcastedBlockStream {} lagged by {} messages",
                        peer, n
                    );
                    continue;
                }
            }
        };
        task::Poll::Ready(maybe_item)
    }
}

async fn make_recv_future<T: Clone>(
    mut rx: broadcast::Receiver<T>,
) -> (
    Result<T, broadcast::error::RecvError>,
    broadcast::Receiver<T>,
) {
    let result = rx.recv().await;
    (result, rx)
}

// TODO: add a unit test for BroadcastStream.
