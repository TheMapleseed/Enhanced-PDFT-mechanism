use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::RwLock;
use tokio::sync::{mpsc, oneshot, broadcast};
use serde::{Serialize, Deserialize};

// Consensus specific types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConsensusMsg {
    // Phase 1: Pre-prepare
    PrePrepare {
        view: u64,
        sequence: u64,
        digest: [u8; 32],
        data: Vec<u8>,
    },
    // Phase 2: Prepare
    Prepare {
        view: u64,
        sequence: u64,
        digest: [u8; 32],
        node_id: NodeId,
    },
    // Phase 3: Commit
    Commit {
        view: u64,
        sequence: u64,
        digest: [u8; 32],
        node_id: NodeId,
    },
    // View change messages
    ViewChange {
        new_view: u64,
        node_id: NodeId,
        last_sequence: u64,
    },
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct NodeId(pub String);

#[derive(Debug)]
pub struct ConsensusState {
    // Current view
    view: u64,
    // Current sequence number
    sequence: u64,
    // Active nodes
    nodes: HashMap<NodeId, NodeState>,
    // Prepare messages received
    prepares: HashMap<(u64, u64), Vec<NodeId>>,
    // Commit messages received
    commits: HashMap<(u64, u64), Vec<NodeId>>,
    // Committed sequences
    committed: Vec<u64>,
    // View change votes
    view_changes: HashMap<u64, Vec<NodeId>>,
}

#[derive(Debug)]
struct NodeState {
    last_seen: Instant,
    is_leader: bool,
}

pub struct ConsensusCore {
    // Node identity
    node_id: NodeId,
    // Consensus state
    state: RwLock<ConsensusState>,
    // Security manager
    security: Arc<SecurityManager>,
    // Message channels
    msg_tx: mpsc::Sender<ConsensusMsg>,
    msg_rx: mpsc::Receiver<ConsensusMsg>,
    // Broadcast channel for committed messages
    commit_tx: broadcast::Sender<Vec<u8>>,
    // Configuration
    config: ConsensusConfig,
}

#[derive(Clone, Debug)]
pub struct ConsensusConfig {
    // Fault tolerance (f), can tolerate up to f faulty nodes
    pub fault_tolerance: usize,
    // Minimum number of nodes needed (3f + 1)
    pub min_nodes: usize,
    // Timeout configurations
    pub timeouts: TimeoutConfig,
}

#[derive(Clone, Debug)]
pub struct TimeoutConfig {
    pub prepare_timeout: Duration,
    pub commit_timeout: Duration,
    pub view_change_timeout: Duration,
}

impl ConsensusCore {
    pub async fn new(
        node_id: NodeId,
        config: ConsensusConfig,
        security: Arc<SecurityManager>,
    ) -> Result<Arc<Self>, ConsensusError> {
        let (msg_tx, msg_rx) = mpsc::channel(1000);
        let (commit_tx, _) = broadcast::channel(100);

        let state = ConsensusState {
            view: 0,
            sequence: 0,
            nodes: HashMap::new(),
            prepares: HashMap::new(),
            commits: HashMap::new(),
            committed: Vec::new(),
            view_changes: HashMap::new(),
        };

        let core = Arc::new(Self {
            node_id,
            state: RwLock::new(state),
            security,
            msg_tx,
            msg_rx,
            commit_tx,
            config,
        });

        // Start consensus message processor
        let processor_core = Arc::clone(&core);
        tokio::spawn(async move {
            processor_core.process_messages().await;
        });

        Ok(core)
    }

    pub async fn propose(&self, data: Vec<u8>) -> Result<(), ConsensusError> {
        let state = self.state.read();
        
        // Only leader can propose
        if !self.is_leader() {
            return Err(ConsensusError::NotLeader);
        }

        // Create pre-prepare message
        let digest = self.security.hash(&data).await?;
        let msg = ConsensusMsg::PrePrepare {
            view: state.view,
            sequence: state.sequence,
            digest,
            data,
        };

        // Broadcast to all nodes
        self.broadcast(msg).await?;

        Ok(())
    }

    async fn process_messages(&self) {
        while let Some(msg) = self.msg_rx.recv().await {
            match msg {
                ConsensusMsg::PrePrepare { view, sequence, digest, data } => {
                    self.handle_pre_prepare(view, sequence, digest, data).await;
                },
                ConsensusMsg::Prepare { view, sequence, digest, node_id } => {
                    self.handle_prepare(view, sequence, digest, node_id).await;
                },
                ConsensusMsg::Commit { view, sequence, digest, node_id } => {
                    self.handle_commit(view, sequence, digest, node_id).await;
                },
                ConsensusMsg::ViewChange { new_view, node_id, last_sequence } => {
                    self.handle_view_change(new_view, node_id, last_sequence).await;
                },
            }
        }
    }

    async fn handle_pre_prepare(
        &self,
        view: u64,
        sequence: u64,
        digest: [u8; 32],
        data: Vec<u8>,
    ) -> Result<(), ConsensusError> {
        let state = self.state.read();
        
        // Validate view and sequence
        if view != state.view {
            return Err(ConsensusError::InvalidView);
        }

        // Verify data matches digest
        let computed_digest = self.security.hash(&data).await?;
        if computed_digest != digest {
            return Err(ConsensusError::InvalidDigest);
        }

        // Send prepare message
        let prepare = ConsensusMsg::Prepare {
            view,
            sequence,
            digest,
            node_id: self.node_id.clone(),
        };

        self.broadcast(prepare).await?;

        Ok(())
    }

    async fn handle_prepare(
        &self,
        view: u64,
        sequence: u64,
        digest: [u8; 32],
        node_id: NodeId,
    ) -> Result<(), ConsensusError> {
        let mut state = self.state.write();
        
        // Validate prepare message
        if view != state.view {
            return Err(ConsensusError::InvalidView);
        }

        // Record prepare message
        let prepares = state.prepares
            .entry((view, sequence))
            .or_insert_with(Vec::new);
        
        if !prepares.contains(&node_id) {
            prepares.push(node_id);
        }

        // Check if we have enough prepares
        if prepares.len() >= self.config.min_nodes - self.config.fault_tolerance {
            // Send commit message
            let commit = ConsensusMsg::Commit {
                view,
                sequence,
                digest,
                node_id: self.node_id.clone(),
            };

            self.broadcast(commit).await?;
        }

        Ok(())
    }

    async fn handle_commit(
        &self,
        view: u64,
        sequence: u64,
        digest: [u8; 32],
        node_id: NodeId,
    ) -> Result<(), ConsensusError> {
        let mut state = self.state.write();
        
        // Validate commit message
        if view != state.view {
            return Err(ConsensusError::InvalidView);
        }

        // Record commit message
        let commits = state.commits
            .entry((view, sequence))
            .or_insert_with(Vec::new);
        
        if !commits.contains(&node_id) {
            commits.push(node_id);
        }

        // Check if we have enough commits
        if commits.len() >= self.config.min_nodes - self.config.fault_tolerance {
            // Mark sequence as committed
            state.committed.push(sequence);
            
            // Notify listeners
            let _ = self.commit_tx.send(Vec::new()); // Send committed data
        }

        Ok(())
    }

    async fn handle_view_change(
        &self,
        new_view: u64,
        node_id: NodeId,
        last_sequence: u64,
    ) -> Result<(), ConsensusError> {
        let mut state = self.state.write();
        
        // Record view change vote
        let votes = state.view_changes
            .entry(new_view)
            .or_insert_with(Vec::new);
        
        if !votes.contains(&node_id) {
            votes.push(node_id);
        }

        // Check if we have enough votes for view change
        if votes.len() >= self.config.min_nodes - self.config.fault_tolerance {
            // Perform view change
            state.view = new_view;
            state.sequence = last_sequence + 1;
            
            // Clear old state
            state.prepares.clear();
            state.commits.clear();
            state.view_changes.clear();
        }

        Ok(())
    }

    fn is_leader(&self) -> bool {
        let state = self.state.read();
        let leader_id = self.calculate_leader(state.view);
        leader_id == self.node_id
    }

    fn calculate_leader(&self, view: u64) -> NodeId {
        // Simple round-robin leader selection
        let nodes: Vec<_> = self.state.read().nodes.keys().cloned().collect();
        let leader_idx = (view as usize) % nodes.len();
        nodes[leader_idx].clone()
    }

    async fn broadcast(&self, msg: ConsensusMsg) -> Result<(), ConsensusError> {
        // In practice, this would send to all other nodes
        self.msg_tx.send(msg).await.map_err(|_| ConsensusError::SendError)?;
        Ok(())
    }
}

// Property-based tests
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn test_consensus_flow(
            data in prop::collection::vec(any::<u8>(), 1..1000)
        ) {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            
            runtime.block_on(async {
                // Create test network of nodes
                let config = ConsensusConfig {
                    fault_tolerance: 1,
                    min_nodes: 4,
                    timeouts: TimeoutConfig {
                        prepare_timeout: Duration::from_secs(5),
                        commit_timeout: Duration::from_secs(5),
                        view_change_timeout: Duration::from_secs(10),
                    },
                };

                let security = Arc::new(SecurityManager::new(&SecurityConfig::default()).unwrap());
                
                let mut nodes = Vec::new();
                for i in 0..4 {
                    let node = ConsensusCore::new(
                        NodeId(format!("node{}", i)),
                        config.clone(),
                        Arc::clone(&security),
                    ).await.unwrap();
                    nodes.push(node);
                }

                // Leader proposes data
                let leader = &nodes[0];
                leader.propose(data.clone()).await.unwrap();

                // Verify consensus is reached
                for node in &nodes {
                    let state = node.state.read();
                    assert!(!state.committed.is_empty());
                }
            });
        }
    }
}
