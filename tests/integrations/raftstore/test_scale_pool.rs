// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use engine_traits::CF_DEFAULT;
use kvproto::raft_cmdpb::RaftCmdResponse;
use libc::{getpid, pid_t};
use procinfo::pid;
use raftstore::Result;
use std::collections::HashMap;
use std::time::Duration;
use test_raftstore::*;
use tikv_util::{metrics::get_thread_ids, HandyRwLock};

fn put_with_timeout<T: Simulator>(
    cluster: &mut Cluster<T>,
    key: &[u8],
    value: &[u8],
    timeout: Duration,
) -> Result<RaftCmdResponse> {
    let mut region = cluster.get_region(key);
    let region_id = region.get_id();
    let req = new_request(
        region_id,
        region.take_region_epoch(),
        vec![new_put_cf_cmd(CF_DEFAULT, key, value)],
        false,
    );
    cluster.call_command_on_node(0, req, timeout)
}

#[test]
fn test_increase_pool() {
    let mut cluster = new_node_cluster(0, 1);
    cluster.cfg.raft_store.store_batch_system.pool_size = 1;
    cluster.cfg.raft_store.apply_batch_system.pool_size = 1;
    cluster.pd_client.disable_default_operator();
    let fp1 = "poll";
    fail::cfg(fp1, "3*pause").unwrap();
    let _ = cluster.run_conf_change();

    put_with_timeout(&mut cluster, b"k1", b"k1", Duration::from_secs(1)).unwrap();
    must_get_none(&cluster.get_engine(1), b"k1");

    {
        let sim = cluster.sim.rl();
        let cfg_controller = sim.get_cfg_controller().unwrap();

        let change = {
            let mut change = HashMap::new();
            change.insert(
                "raftstore.store-batch-system.pool-size".to_owned(),
                "2".to_owned(),
            );
            change.insert(
                "raftstore.apply-batch-system.pool-size".to_owned(),
                "2".to_owned(),
            );
            change
        };
        cfg_controller.update(change).unwrap();
        cluster.cfg.raft_store.store_batch_system.pool_size = 2;
        cluster.cfg.raft_store.apply_batch_system.pool_size = 2;
        assert_eq!(cfg_controller.get_current(), cluster.cfg.tikv);
    }

    cluster.must_put(b"k2", b"v2");
    must_get_equal(&cluster.get_engine(1), b"k2", b"v2");

    fail::remove(fp1);
}

fn get_poller_thread_ids() -> Vec<pid_t> {
    let prefixs = ("raftstore", "apply-");
    let pid: pid_t = unsafe { getpid() };
    let mut poller_tids = vec![];
    for tid in get_thread_ids(pid).unwrap() {
        if let Ok(stat) = pid::stat_task(pid, tid) {
            if !stat.command.starts_with(prefixs.0) && !stat.command.starts_with(prefixs.1) {
                continue;
            }
        }
        poller_tids.push(tid);
    }
    poller_tids
}

#[test]
fn test_decrease_pool() {
    let mut cluster = new_node_cluster(0, 1);
    cluster.pd_client.disable_default_operator();
    cluster.cfg.raft_store.store_batch_system.pool_size = 2;
    cluster.cfg.raft_store.apply_batch_system.pool_size = 2;
    let _ = cluster.run_conf_change();

    let original_poller_tids = get_poller_thread_ids();

    cluster.must_put(b"k1", b"v1");
    must_get_equal(&cluster.get_engine(1), b"k1", b"v1");

    {
        let sim = cluster.sim.rl();
        let cfg_controller = sim.get_cfg_controller().unwrap();
        let change = {
            let mut change = HashMap::new();
            change.insert(
                "raftstore.store-batch-system.pool-size".to_owned(),
                "1".to_owned(),
            );
            change.insert(
                "raftstore.apply-batch-system.pool-size".to_owned(),
                "1".to_owned(),
            );
            change
        };
        cfg_controller.update(change).unwrap();
        cluster.cfg.raft_store.store_batch_system.pool_size = 1;
        cluster.cfg.raft_store.apply_batch_system.pool_size = 1;
        assert_eq!(cfg_controller.get_current(), cluster.cfg.tikv);
    }

    let current_poller_tids = get_poller_thread_ids();
    assert_eq!(current_poller_tids.len(), original_poller_tids.len() - 2);

    cluster.must_put(b"k2", b"v2");
    must_get_equal(&cluster.get_engine(1), b"k2", b"v2");
}
