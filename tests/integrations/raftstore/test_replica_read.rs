// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::collections::HashMap;
use std::mem;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kvproto::raft_serverpb::RaftMessage;
use raft::eraftpb::MessageType;
use raftstore::Result;
use test_raftstore::*;
use tikv_util::config::*;
use tikv_util::HandyRwLock;

#[test]
fn test_replica_read_not_applied() {
    let mut cluster = new_node_cluster(0, 3);

    // Increase the election tick to make this test case running reliably.
    configure_for_lease_read(&mut cluster, Some(50), Some(30));
    let max_lease = Duration::from_secs(1);
    cluster.cfg.raft_store.raft_store_max_leader_lease = ReadableDuration(max_lease);
    // After the leader has committed to its term, pending reads on followers can be responsed.
    // However followers can receive `ReadIndexResp` after become candidate if the leader has
    // hibernated. So, disable the feature to avoid read requests on followers to be cleared as
    // stale.
    cluster.cfg.raft_store.hibernate_regions = false;

    cluster.pd_client.disable_default_operator();
    let r1 = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    cluster.pd_client.must_add_peer(r1, new_peer(2, 2));
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");
    cluster.pd_client.must_add_peer(r1, new_peer(3, 3));
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    cluster.must_transfer_leader(1, new_peer(1, 1));

    // Add a filter to forbid peer 2 and 3 to know the last entry is committed.
    let committed_indices = Arc::new(Mutex::new(HashMap::default()));
    let filter = Box::new(CommitToFilter::new(committed_indices));
    cluster.sim.wl().add_send_filter(1, filter);

    cluster.must_put(b"k1", b"v2");
    must_get_equal(&cluster.get_engine(1), b"k1", b"v2");
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");

    // Add a filter to forbid the new leader to commit its first entry.
    let dropped_msgs = Arc::new(Mutex::new(Vec::new()));
    let filter = Box::new(
        RegionPacketFilter::new(1, 2)
            .direction(Direction::Recv)
            .msg_type(MessageType::MsgAppendResponse)
            .reserve_dropped(Arc::clone(&dropped_msgs)),
    );
    cluster.sim.wl().add_recv_filter(2, filter);

    cluster.must_transfer_leader(1, new_peer(2, 2));
    let r1 = cluster.get_region(b"k1");

    // Read index on follower should be blocked instead of get an old value.
    let resp1_ch = async_read_on_peer(&mut cluster, new_peer(3, 3), r1.clone(), b"k1", true, true);
    assert!(resp1_ch.recv_timeout(Duration::from_secs(1)).is_err());

    // Unpark all append responses so that the new leader can commit its first entry.
    let router = cluster.sim.wl().get_router(2).unwrap();
    for raft_msg in mem::replace(dropped_msgs.lock().unwrap().as_mut(), vec![]) {
        router.send_raft_message(raft_msg).unwrap();
    }

    // The old read index request won't be blocked forever as it's retried internally.
    cluster.sim.wl().clear_send_filters(1);
    cluster.sim.wl().clear_recv_filters(2);
    let resp1 = resp1_ch.recv_timeout(Duration::from_secs(6)).unwrap();
    let exp_value = resp1.get_responses()[0].get_get().get_value();
    assert_eq!(exp_value, b"v2");

    // New read index requests can be resolved quickly.
    let resp2_ch = async_read_on_peer(&mut cluster, new_peer(3, 3), r1, b"k1", true, true);
    let resp2 = resp2_ch.recv_timeout(Duration::from_secs(3)).unwrap();
    let exp_value = resp2.get_responses()[0].get_get().get_value();
    assert_eq!(exp_value, b"v2");
}

#[test]
fn test_replica_read_on_hibernate() {
    let mut cluster = new_node_cluster(0, 3);

    configure_for_lease_read(&mut cluster, Some(50), Some(20));
    // let max_lease = Duration::from_secs(2);
    // cluster.cfg.raft_store.raft_store_max_leader_lease = ReadableDuration(max_lease);

    cluster.pd_client.disable_default_operator();
    let r1 = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    cluster.pd_client.must_add_peer(r1, new_peer(2, 2));
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");
    cluster.pd_client.must_add_peer(r1, new_peer(3, 3));
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    let filter = Box::new(
        RegionPacketFilter::new(1, 3)
            .direction(Direction::Recv)
            .msg_type(MessageType::MsgReadIndex),
    );
    cluster.sim.wl().add_recv_filter(3, filter);
    cluster.must_transfer_leader(1, new_peer(3, 3));

    let r1 = cluster.get_region(b"k1");

    // Read index on follower should be blocked.
    let resp1_ch = async_read_on_peer(&mut cluster, new_peer(1, 1), r1, b"k1", true, true);
    assert!(resp1_ch.recv_timeout(Duration::from_secs(1)).is_err());

    let (tx, rx) = mpsc::sync_channel(1024);
    let cb = Arc::new(move |msg: &RaftMessage| {
        let _ = tx.send(msg.clone());
    }) as Arc<dyn Fn(&RaftMessage) + Send + Sync>;
    for i in 1..=3 {
        let filter = Box::new(
            RegionPacketFilter::new(1, i)
                .when(Arc::new(AtomicBool::new(false)))
                .set_msg_callback(Arc::clone(&cb)),
        );
        cluster.sim.wl().add_send_filter(i, filter);
    }

    // In the loop, peer 1 will keep sending read index messages to 3,
    // but peer 3 and peer 2 will hibernate later. So, peer 1 will start
    // a new election finally because it always ticks.
    let start = Instant::now();
    loop {
        if start.elapsed() >= Duration::from_secs(6) {
            break;
        }
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(m) => {
                let m = m.get_message();
                if m.get_msg_type() == MessageType::MsgRequestPreVote && m.from == 1 {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => panic!("shouldn't hibernate"),
            Err(_) => unreachable!(),
        }
    }
}

#[derive(Default)]
struct CommitToFilter {
    // map[peer_id] -> committed index.
    committed: Arc<Mutex<HashMap<u64, u64>>>,
}

impl CommitToFilter {
    fn new(committed: Arc<Mutex<HashMap<u64, u64>>>) -> Self {
        Self { committed }
    }
}

impl Filter for CommitToFilter {
    fn before(&self, msgs: &mut Vec<RaftMessage>) -> Result<()> {
        let mut committed = self.committed.lock().unwrap();
        for msg in msgs.iter_mut() {
            let cmt = msg.get_message().get_commit();
            if cmt != 0 {
                let to = msg.get_message().get_to();
                committed.insert(to, cmt);
                msg.mut_message().set_commit(0);
            }
        }
        Ok(())
    }

    fn after(&self, _: Result<()>) -> Result<()> {
        Ok(())
    }
}
