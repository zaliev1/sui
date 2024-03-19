// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! CommitSyncer implements efficient synchronization of past committed data.
//!
//! During the operation of a committee of authorities for consensus, one or more authorities
//! can fall behind the quorum in their received and accepted blocks. This can happen due to
//! network disruptions, host crash, or other reasons. Authories fell behind need to catch up to
//! the quorum to be able to vote on the latest leaders. So efficient synchronization is necessary
//! to minimize the impact of temporary disruptions and maintain smooth operations of the network.
//!  
//! CommitSyncer achieves efficient synchronization by relying on the following: when blocks
//! are included in commits with >= 2f+1 certifiers by stake, these blocks must have passed
//! verifications on some honest validators, so re-verifying them is unnecessary. In fact, the
//! quorum certified commits themselves can be trusted to be sent to Sui directly, but for
//! simplicity this is not done. Blocks from trusted commits still go through Core and committer.
//!
//! Another way CommitSyncer improves the efficiency of synchronization is parallel fetching:
//! commits have simpler dependency (previous commit) than blocks (many ancestors), so it is easier
//! to fetch ranges of commits by rounds in parallel, compared to fetching blocks in a similar way.
//!
//! Commit sychronization is an expensive operation, involving transfering large amount of data via
//! the network. And it is not on the critical path of block processing. So the heuristics for
//! synchronization, including triggers and retries, should be chosen to favor throughput and
//! efficient resource usage, over faster reactions.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use futures::stream::FuturesOrdered;
use parking_lot::{Mutex, RwLock};
use tokio::{task::JoinSet, time::MissedTickBehavior};

use crate::{
    block::{BlockAPI, Round, VerifiedBlock},
    commit::{CommitRef, TrustedCommit},
    context::Context,
    dag_state::DagState,
};

pub(crate) struct CommitSyncer {
    context: Arc<Context>,
    dag_state: Arc<RwLock<DagState>>,
    inner: Arc<Mutex<Inner>>,
    tasks: Arc<Mutex<JoinSet<()>>>,
}

impl CommitSyncer {
    pub(crate) fn new(context: Arc<Context>, dag_state: Arc<RwLock<DagState>>) -> Self {
        CommitSyncer {
            context,
            dag_state,
            inner: Arc::new(Mutex::new(Inner {
                highest_received_commits: vec![CommitRef::GENESIS; context.committee.size()],
                pending_fetch_commits: VecDeque::new(),
            })),
            tasks: Arc::new(Mutex::new(JoinSet::new())),
        }
    }

    /// Rounds of received blocks are used to trigger CommitSyncer, when they
    /// are more advanced than the highest local accepted round.
    pub(crate) fn observe(&self, block: &VerifiedBlock) {
        let mut inner = self.inner.lock();
        for vote in block.commit_votes() {
            if vote.index > inner.highest_received_commits[block.author()].index {
                inner.highest_received_commits[block.author()] = *vote;
            }
        }
    }

    fn start(&self) {
        // let requests = FuturesOrdered::new();
    }

    async fn fetch_commit_loop(
        context: Arc<Context>,
        dag_state: Arc<RwLock<DagState>>,
        inner: Arc<Mutex<Inner>>,
        tx_certified_commits: tokio::sync::mpsc::Sender<TrustedCommit>,
    ) {
        const COMMIT_LAG_THRESHOLD: Round = 20;

        // let mut fetch_commits_tasks = FuturesOrdered::new();

        // let requests = VecDeque::new();
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let mut inner = inner.lock();
                    let quorum_highest_received_commit = inner.quorum_highest_received_commit(&context);
                    let last_commit = dag_state.read().last_commit();
                }
                // result = pending_fetch_commits_tasks.next(), if !pending_fetch_commits_tasks.is_empty() => {
                //     let mut inner = inner.lock();
                //     let pending_fetch_commits = inner.pending_fetch_commits.pop_front().unwrap();
                    // if let Some(result) = result {
                    //     let (start, end, handle) = result;
                    //     if let Err(e) = handle.await {
                    //         log::warn!("Failed to fetch commits from {} to {}: {}", start, end, e);
                    //     }
                    // }
                // }
            }

            // pending_fetch_commits_tasks.push_(tokio::spawn(async move {
            //     Err(ConsensusError::UnexpectedGenesisBlock)
            // }));

            // let commit_round_lower_bound = std::cmp::max(dag_state.read().last_commit_round(), pending_fetch_commits_tasks.back().map(|t| t.end).unwrap_or(0));
            // if commit_round_lower_bound + COMMIT_LAG_THRESHOLD < peer_highest_committed_round {
            // }
        }
    }
}

struct Inner {
    highest_received_commits: Vec<CommitRef>,
    pending_fetch_commits: VecDeque<PendingFetchCommits>,
}

impl Inner {
    fn quorum_highest_received_commit(&self, context: &Context) -> Option<CommitRef> {
        let mut highest_received_commits = context
            .committee
            .authorities()
            .zip(self.highest_received_commits.iter())
            .map(|((_i, a), r)| (*r, a.stake))
            .collect::<Vec<_>>();
        // Sort by commit ref / index then stake, descending.
        highest_received_commits.sort_by(|a, b| a.cmp(&b).reverse());
        let mut total_stake = 0;
        for (commit_ref, stake) in highest_received_commits {
            total_stake += stake;
            if total_stake >= context.committee.validity_threshold() {
                return Some(commit_ref);
            }
        }
        None
    }
}

struct PendingFetchCommits {
    start: Round,
    end: Round,
    handle: tokio::task::JoinHandle<()>,
}
