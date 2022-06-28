// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::Arc;

use engine_traits::{self, Error, Mutable, Result, WriteBatchExt, WriteOptions};
use rocksdb::{Writable, WriteBatch as RawWriteBatch, DB};

use crate::{engine::RocksEngine, options::RocksWriteOptions, util::get_cf_handle};

const WRITE_BATCH_MAX_BATCH: usize = 16;
const WRITE_BATCH_LIMIT: usize = 16;

impl WriteBatchExt for RocksEngine {
    type WriteBatch = RocksWriteBatchVec;

    const WRITE_BATCH_MAX_KEYS: usize = 256;

    fn write_batch(&self) -> RocksWriteBatchVec {
        RocksWriteBatchVec::new(Arc::clone(self.as_inner()), WRITE_BATCH_LIMIT, 1)
    }

    fn write_batch_with_cap(&self, cap: usize) -> RocksWriteBatchVec {
        RocksWriteBatchVec::with_capacity(self, cap)
    }
}

/// `RocksWriteBatchVec` is for method `MultiBatchWrite` of RocksDB, which splits a large WriteBatch
/// into many smaller ones and then any thread could help to deal with these small WriteBatch when it
/// is calling `MultiBatchCommit` and wait the front writer to finish writing. `MultiBatchWrite` will
/// perform much better than traditional `pipelined_write` when TiKV writes very large data into RocksDB.
/// We will remove this feature when `unordered_write` of RocksDB becomes more stable and becomes compatible
/// with Titan.
pub struct RocksWriteBatchVec {
    db: Arc<DB>,
    wbs: Vec<RawWriteBatch>,
    save_points: Vec<usize>,
    index: usize,
    cur_batch_size: usize,
    batch_size_limit: usize,
    support_write_batch_vec: bool,
}

impl RocksWriteBatchVec {
    pub fn new(db: Arc<DB>, batch_size_limit: usize, cap: usize) -> RocksWriteBatchVec {
        let wb = RawWriteBatch::with_capacity(cap);
        RocksWriteBatchVec {
            db: db.clone(),
            wbs: vec![wb],
            save_points: vec![],
            index: 0,
            cur_batch_size: 0,
            batch_size_limit,
            support_write_batch_vec: db.get_db_options().is_enable_multi_batch_write(),
        }
    }

    pub fn with_capacity(engine: &RocksEngine, cap: usize) -> RocksWriteBatchVec {
        Self::new(engine.as_inner().clone(), WRITE_BATCH_LIMIT, cap)
    }

    pub fn as_inner(&self) -> &[RawWriteBatch] {
        &self.wbs[0..=self.index]
    }

    pub fn as_raw(&self) -> &RawWriteBatch {
        &self.wbs[0]
    }

    pub fn get_db(&self) -> &DB {
        self.db.as_ref()
    }

    /// `check_switch_batch` will split a large WriteBatch into many smaller ones. This is to avoid
    /// a large WriteBatch blocking write_thread too long.
    fn check_switch_batch(&mut self) {
        if self.support_write_batch_vec {
            if self.batch_size_limit > 0 && self.cur_batch_size >= self.batch_size_limit {
                self.index += 1;
                self.cur_batch_size = 0;
                if self.index >= self.wbs.len() {
                    self.wbs.push(RawWriteBatch::default());
                }
            }
        }
        self.cur_batch_size += 1;
    }
}

impl engine_traits::WriteBatch for RocksWriteBatchVec {
    fn write_opt(&self, opts: &WriteOptions) -> Result<()> {
        let opt: RocksWriteOptions = opts.into();
        if self.index > 0 {
            self.get_db()
                .multi_batch_write(self.as_inner(), &opt.into_raw())
                .map_err(Error::Engine)
        } else {
            self.get_db()
                .write_opt(&self.wbs[0], &opt.into_raw())
                .map_err(Error::Engine)
        }
    }

    fn data_size(&self) -> usize {
        self.wbs.iter().fold(0, |a, b| a + b.data_size())
    }

    fn count(&self) -> usize {
        if self.support_write_batch_vec {
            self.cur_batch_size + self.index * self.batch_size_limit
        } else {
            self.as_raw().count()
        }
    }

    fn is_empty(&self) -> bool {
        self.wbs[0].is_empty()
    }

    fn should_write_to_engine(&self) -> bool {
        if self.support_write_batch_vec {
            self.index >= WRITE_BATCH_MAX_BATCH
        } else {
            self.count() > RocksEngine::WRITE_BATCH_MAX_KEYS
        }
    }

    fn clear(&mut self) {
        for i in 0..=self.index {
            self.wbs[i].clear();
        }
        self.save_points.clear();
        self.index = 0;
        self.cur_batch_size = 0;
    }

    fn set_save_point(&mut self) {
        self.wbs[self.index].set_save_point();
        self.save_points.push(self.index);
    }

    fn pop_save_point(&mut self) -> Result<()> {
        if let Some(x) = self.save_points.pop() {
            return self.wbs[x].pop_save_point().map_err(Error::Engine);
        }
        Err(Error::Engine("no save point".into()))
    }

    fn rollback_to_save_point(&mut self) -> Result<()> {
        if let Some(x) = self.save_points.pop() {
            for i in x + 1..=self.index {
                self.wbs[i].clear();
            }
            self.index = x;
            return self.wbs[x].rollback_to_save_point().map_err(Error::Engine);
        }
        Err(Error::Engine("no save point".into()))
    }

    fn merge(&mut self, other: Self) -> Result<()> {
        for wb in other.as_inner() {
            self.check_switch_batch();
            self.wbs[self.index].append(wb.data());
        }
        Ok(())
    }
}

impl Mutable for RocksWriteBatchVec {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.check_switch_batch();
        self.wbs[self.index].put(key, value).map_err(Error::Engine)
    }

    fn put_cf(&mut self, cf: &str, key: &[u8], value: &[u8]) -> Result<()> {
        self.check_switch_batch();
        let handle = get_cf_handle(self.db.as_ref(), cf)?;
        self.wbs[self.index]
            .put_cf(handle, key, value)
            .map_err(Error::Engine)
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.check_switch_batch();
        self.wbs[self.index].delete(key).map_err(Error::Engine)
    }

    fn delete_cf(&mut self, cf: &str, key: &[u8]) -> Result<()> {
        self.check_switch_batch();
        let handle = get_cf_handle(self.db.as_ref(), cf)?;
        self.wbs[self.index]
            .delete_cf(handle, key)
            .map_err(Error::Engine)
    }

    fn delete_range(&mut self, begin_key: &[u8], end_key: &[u8]) -> Result<()> {
        self.check_switch_batch();
        self.wbs[self.index]
            .delete_range(begin_key, end_key)
            .map_err(Error::Engine)
    }

    fn delete_range_cf(&mut self, cf: &str, begin_key: &[u8], end_key: &[u8]) -> Result<()> {
        self.check_switch_batch();
        let handle = get_cf_handle(self.db.as_ref(), cf)?;
        self.wbs[self.index]
            .delete_range_cf(handle, begin_key, end_key)
            .map_err(Error::Engine)
    }
}

#[cfg(test)]
mod tests {
    use engine_traits::WriteBatch;
    use rocksdb::DBOptions as RawDBOptions;
    use tempfile::Builder;

    use super::{
        super::{util::new_engine_opt, RocksDBOptions},
        *,
    };

    #[test]
    fn test_should_write_to_engine() {
        let path = Builder::new()
            .prefix("test-should-write-to-engine")
            .tempdir()
            .unwrap();
        let opt = RawDBOptions::default();
        opt.enable_unordered_write(false);
        opt.enable_pipelined_write(false);
        opt.enable_multi_batch_write(true);
        let engine = new_engine_opt(
            path.path().join("db").to_str().unwrap(),
            RocksDBOptions::from_raw(opt),
            vec![],
        )
        .unwrap();
        assert!(
            engine
                .as_inner()
                .get_db_options()
                .is_enable_multi_batch_write()
        );
        let mut wb = engine.write_batch();
        for _i in 0..RocksEngine::WRITE_BATCH_MAX_KEYS {
            wb.put(b"aaa", b"bbb").unwrap();
        }
        assert!(!wb.should_write_to_engine());
        wb.put(b"aaa", b"bbb").unwrap();
        assert!(wb.should_write_to_engine());
        let mut wb = RocksWriteBatchVec::with_capacity(&engine, 1024);
        for _i in 0..WRITE_BATCH_MAX_BATCH * WRITE_BATCH_LIMIT {
            wb.put(b"aaa", b"bbb").unwrap();
        }
        assert!(!wb.should_write_to_engine());
        wb.put(b"aaa", b"bbb").unwrap();
        assert!(wb.should_write_to_engine());
        wb.clear();
        assert!(!wb.should_write_to_engine());
    }
}
