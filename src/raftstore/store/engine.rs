// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::option::Option;
use std::ops::Deref;
use std::sync::Arc;
use std::fmt::{self, Debug, Formatter};

use rocksdb::{CFHandle, DBIterator, DBVector, ReadOptions, Writable, WriteBatch, DB};
use rocksdb::rocksdb_options::UnsafeSnap;
use protobuf;
use byteorder::{BigEndian, ByteOrder};
use util::rocksdb;

use raftstore::Result;
use raftstore::Error;

pub struct Snapshot {
    db: Arc<DB>,
    snap: UnsafeSnap,
}

/// Because snap will be valid whenever db is valid, so it's safe to send
/// it around.
unsafe impl Send for Snapshot {}
unsafe impl Sync for Snapshot {}

#[derive(Debug, Clone)]
pub struct SyncSnapshot(Arc<Snapshot>);

impl Deref for SyncSnapshot {
    type Target = Snapshot;

    fn deref(&self) -> &Snapshot {
        &self.0
    }
}

impl SyncSnapshot {
    pub fn new(db: Arc<DB>) -> SyncSnapshot {
        SyncSnapshot(Arc::new(Snapshot::new(db)))
    }

    pub fn clone(&self) -> SyncSnapshot {
        SyncSnapshot(self.0.clone())
    }
}

impl Snapshot {
    pub fn new(db: Arc<DB>) -> Snapshot {
        unsafe {
            Snapshot {
                snap: db.unsafe_snap(),
                db: db,
            }
        }
    }

    pub fn into_sync(self) -> SyncSnapshot {
        SyncSnapshot(Arc::new(self))
    }

    pub fn cf_names(&self) -> Vec<&str> {
        self.db.cf_names()
    }

    pub fn cf_handle(&self, cf: &str) -> Result<&CFHandle> {
        rocksdb::get_cf_handle(&self.db, cf).map_err(Error::from)
    }

    pub fn get_db(&self) -> Arc<DB> {
        self.db.clone()
    }
}

impl Debug for Snapshot {
    fn fmt(&self, fmt: &mut Formatter) -> fmt::Result {
        write!(fmt, "Engine Snapshot Impl")
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        unsafe {
            self.db.release_snap(&self.snap);
        }
    }
}

// TODO: refactor this trait into rocksdb trait.
pub trait Peekable {
    fn get_value(&self, key: &[u8]) -> Result<Option<DBVector>>;
    fn get_value_cf(&self, cf: &str, key: &[u8]) -> Result<Option<DBVector>>;

    fn get_msg<M>(&self, key: &[u8]) -> Result<Option<M>>
    where
        M: protobuf::Message + protobuf::MessageStatic,
    {
        let value = try!(self.get_value(key));

        if value.is_none() {
            return Ok(None);
        }

        let mut m = M::new();
        try!(m.merge_from_bytes(&value.unwrap()));
        Ok(Some(m))
    }

    fn get_msg_cf<M>(&self, cf: &str, key: &[u8]) -> Result<Option<M>>
    where
        M: protobuf::Message + protobuf::MessageStatic,
    {
        let value = try!(self.get_value_cf(cf, key));

        if value.is_none() {
            return Ok(None);
        }

        let mut m = M::new();
        try!(m.merge_from_bytes(&value.unwrap()));
        Ok(Some(m))
    }

    fn get_u64(&self, key: &[u8]) -> Result<Option<u64>> {
        let value = try!(self.get_value(key));

        if value.is_none() {
            return Ok(None);
        }

        let value = value.unwrap();
        if value.len() != 8 {
            return Err(box_err!("need 8 bytes, but only got {}", value.len()));
        }

        let n = BigEndian::read_u64(&value);
        Ok(Some(n))
    }

    fn get_i64(&self, key: &[u8]) -> Result<Option<i64>> {
        let r = try!(self.get_u64(key));
        match r {
            None => Ok(None),
            Some(n) => Ok(Some(n as i64)),
        }
    }
}

#[derive(Clone, PartialEq)]
enum SeekMode {
    TotalOrder,
    Prefix,
}

pub struct IterOption {
    upper_bound: Option<Vec<u8>>,
    prefix_same_as_start: bool,
    fill_cache: bool,
    seek_mode: SeekMode,
}

impl IterOption {
    pub fn new(upper_bound: Option<Vec<u8>>, fill_cache: bool) -> IterOption {
        IterOption {
            upper_bound: upper_bound,
            prefix_same_as_start: false,
            fill_cache: fill_cache,
            seek_mode: SeekMode::TotalOrder,
        }
    }

    #[inline]
    pub fn use_prefix_seek(mut self) -> IterOption {
        self.seek_mode = SeekMode::Prefix;
        self
    }

    #[inline]
    pub fn total_order_seek_used(&self) -> bool {
        self.seek_mode == SeekMode::TotalOrder
    }

    #[inline]
    pub fn upper_bound(&self) -> Option<&[u8]> {
        self.upper_bound.as_ref().map(|v| v.as_slice())
    }

    #[inline]
    pub fn set_upper_bound(mut self, bound: Vec<u8>) -> IterOption {
        self.upper_bound = Some(bound);
        self
    }

    #[inline]
    pub fn set_prefix_same_as_start(mut self, enable: bool) -> IterOption {
        self.prefix_same_as_start = enable;
        self
    }

    pub fn build_read_opts(&self) -> ReadOptions {
        let mut opts = ReadOptions::new();
        opts.fill_cache(self.fill_cache);
        if self.total_order_seek_used() {
            opts.set_total_order_seek(true);
        } else if self.prefix_same_as_start {
            opts.set_prefix_same_as_start(true);
        }
        if let Some(ref key) = self.upper_bound {
            opts.set_iterate_upper_bound(key);
        }
        opts
    }
}

impl Default for IterOption {
    fn default() -> IterOption {
        IterOption {
            upper_bound: None,
            prefix_same_as_start: false,
            fill_cache: true,
            seek_mode: SeekMode::TotalOrder,
        }
    }
}

// TODO: refactor this trait into rocksdb trait.
pub trait Iterable {
    fn new_iterator(&self, iter_opt: IterOption) -> DBIterator;
    fn new_iterator_cf(&self, &str, iter_opt: IterOption) -> Result<DBIterator>;

    // scan scans database using an iterator in range [start_key, end_key), calls function f for
    // each iteration, if f returns false, terminates this scan.
    fn scan<F>(&self, start_key: &[u8], end_key: &[u8], fill_cache: bool, f: &mut F) -> Result<()>
    where
        F: FnMut(&[u8], &[u8]) -> Result<bool>,
    {
        let iter_opt = IterOption::new(Some(end_key.to_vec()), fill_cache);
        scan_impl(self.new_iterator(iter_opt), start_key, f)
    }

    // like `scan`, only on a specific column family.
    fn scan_cf<F>(
        &self,
        cf: &str,
        start_key: &[u8],
        end_key: &[u8],
        fill_cache: bool,
        f: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&[u8], &[u8]) -> Result<bool>,
    {
        let iter_opt = IterOption::new(Some(end_key.to_vec()), fill_cache);
        scan_impl(try!(self.new_iterator_cf(cf, iter_opt)), start_key, f)
    }

    // Seek the first key >= given key, if no found, return None.
    fn seek(&self, key: &[u8]) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut iter = self.new_iterator(IterOption::default());
        iter.seek(key.into());
        Ok(iter.kv())
    }

    // Seek the first key >= given key, if no found, return None.
    fn seek_cf(&self, cf: &str, key: &[u8]) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut iter = try!(self.new_iterator_cf(cf, IterOption::default()));
        iter.seek(key.into());
        Ok(iter.kv())
    }
}

fn scan_impl<F>(mut it: DBIterator, start_key: &[u8], f: &mut F) -> Result<()>
where
    F: FnMut(&[u8], &[u8]) -> Result<bool>,
{
    it.seek(start_key.into());
    while it.valid() {
        let r = try!(f(it.key(), it.value()));

        if !r || !it.next() {
            break;
        }
    }

    Ok(())
}

impl Peekable for DB {
    fn get_value(&self, key: &[u8]) -> Result<Option<DBVector>> {
        let v = try!(self.get(key));
        Ok(v)
    }

    fn get_value_cf(&self, cf: &str, key: &[u8]) -> Result<Option<DBVector>> {
        let handle = try!(rocksdb::get_cf_handle(self, cf));
        let v = try!(self.get_cf(handle, key));
        Ok(v)
    }
}

impl Iterable for DB {
    fn new_iterator(&self, iter_opt: IterOption) -> DBIterator {
        self.iter_opt(iter_opt.build_read_opts())
    }

    fn new_iterator_cf(&self, cf: &str, iter_opt: IterOption) -> Result<DBIterator> {
        let handle = try!(rocksdb::get_cf_handle(self, cf));
        let readopts = iter_opt.build_read_opts();
        Ok(DBIterator::new_cf(self, handle, readopts))
    }
}

impl Peekable for Snapshot {
    fn get_value(&self, key: &[u8]) -> Result<Option<DBVector>> {
        let mut opt = ReadOptions::new();
        unsafe {
            opt.set_snapshot(&self.snap);
        }
        let v = try!(self.db.get_opt(key, &opt));
        Ok(v)
    }

    fn get_value_cf(&self, cf: &str, key: &[u8]) -> Result<Option<DBVector>> {
        let handle = try!(rocksdb::get_cf_handle(&self.db, cf));
        let mut opt = ReadOptions::new();
        unsafe {
            opt.set_snapshot(&self.snap);
        }
        let v = try!(self.db.get_cf_opt(handle, key, &opt));
        Ok(v)
    }
}

impl Iterable for Snapshot {
    fn new_iterator(&self, iter_opt: IterOption) -> DBIterator {
        let mut opt = iter_opt.build_read_opts();
        unsafe {
            opt.set_snapshot(&self.snap);
        }
        DBIterator::new(&self.db, opt)
    }

    fn new_iterator_cf(&self, cf: &str, iter_opt: IterOption) -> Result<DBIterator> {
        let handle = try!(rocksdb::get_cf_handle(&self.db, cf));
        let mut opt = iter_opt.build_read_opts();
        unsafe {
            opt.set_snapshot(&self.snap);
        }
        Ok(DBIterator::new_cf(&self.db, handle, opt))
    }
}

pub trait Mutable: Writable {
    fn put_msg<M: protobuf::Message>(&self, key: &[u8], m: &M) -> Result<()> {
        let value = try!(m.write_to_bytes());
        try!(self.put(key, &value));
        Ok(())
    }

    fn put_msg_cf<M: protobuf::Message>(&self, cf: &CFHandle, key: &[u8], m: &M) -> Result<()> {
        let value = try!(m.write_to_bytes());
        try!(self.put_cf(cf, key, &value));
        Ok(())
    }

    fn put_u64(&self, key: &[u8], n: u64) -> Result<()> {
        let mut value = vec![0; 8];
        BigEndian::write_u64(&mut value, n);
        try!(self.put(key, &value));
        Ok(())
    }

    fn put_i64(&self, key: &[u8], n: i64) -> Result<()> {
        self.put_u64(key, n as u64)
    }

    fn del(&self, key: &[u8]) -> Result<()> {
        try!(self.delete(key));
        Ok(())
    }
}

impl Mutable for DB {}
impl Mutable for WriteBatch {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use tempdir::TempDir;
    use rocksdb::Writable;
    use super::*;
    use kvproto::metapb::Region;

    #[test]
    fn test_base() {
        let path = TempDir::new("var").unwrap();
        let cf = "cf";
        let engine = Arc::new(
            rocksdb::new_engine(path.path().to_str().unwrap(), &[cf]).unwrap(),
        );

        let mut r = Region::new();
        r.set_id(10);

        let key = b"key";
        let handle = rocksdb::get_cf_handle(&engine, cf).unwrap();
        engine.put_msg(key, &r).unwrap();
        engine.put_msg_cf(handle, key, &r).unwrap();

        let snap = Snapshot::new(engine.clone());

        let mut r1: Region = engine.get_msg(key).unwrap().unwrap();
        assert_eq!(r, r1);
        let r1_cf: Region = engine.get_msg_cf(cf, key).unwrap().unwrap();
        assert_eq!(r, r1_cf);

        let mut r2: Region = snap.get_msg(key).unwrap().unwrap();
        assert_eq!(r, r2);
        let r2_cf: Region = snap.get_msg_cf(cf, key).unwrap().unwrap();
        assert_eq!(r, r2_cf);

        r.set_id(11);
        engine.put_msg(key, &r).unwrap();
        r1 = engine.get_msg(key).unwrap().unwrap();
        r2 = snap.get_msg(key).unwrap().unwrap();
        assert_ne!(r1, r2);

        let b: Option<Region> = engine.get_msg(b"missing_key").unwrap();
        assert!(b.is_none());

        engine.put_i64(key, -1).unwrap();
        assert_eq!(engine.get_i64(key).unwrap(), Some(-1));
        assert!(engine.get_i64(b"missing_key").unwrap().is_none());

        let snap = Snapshot::new(engine.clone());
        assert_eq!(snap.get_i64(key).unwrap(), Some(-1));
        assert!(snap.get_i64(b"missing_key").unwrap().is_none());

        engine.put_u64(key, 1).unwrap();
        assert_eq!(engine.get_u64(key).unwrap(), Some(1));
        assert_eq!(snap.get_i64(key).unwrap(), Some(-1));
    }

    #[test]
    fn test_peekable() {
        let path = TempDir::new("var").unwrap();
        let cf = "cf";
        let engine = rocksdb::new_engine(path.path().to_str().unwrap(), &[cf]).unwrap();

        engine.put(b"k1", b"v1").unwrap();
        let handle = engine.cf_handle("cf").unwrap();
        engine.put_cf(handle, b"k1", b"v2").unwrap();

        assert_eq!(&*engine.get_value(b"k1").unwrap().unwrap(), b"v1");
        assert!(engine.get_value_cf("foo", b"k1").is_err());
        assert_eq!(&*engine.get_value_cf(cf, b"k1").unwrap().unwrap(), b"v2");
    }

    #[test]
    fn test_scan() {
        let path = TempDir::new("var").unwrap();
        let cf = "cf";
        let engine = Arc::new(
            rocksdb::new_engine(path.path().to_str().unwrap(), &[cf]).unwrap(),
        );
        let handle = engine.cf_handle(cf).unwrap();

        engine.put(b"a1", b"v1").unwrap();
        engine.put(b"a2", b"v2").unwrap();
        engine.put_cf(handle, b"a1", b"v1").unwrap();
        engine.put_cf(handle, b"a2", b"v22").unwrap();

        let mut data = vec![];
        engine
            .scan(b"", &[0xFF, 0xFF], false, &mut |key, value| {
                data.push((key.to_vec(), value.to_vec()));
                Ok(true)
            })
            .unwrap();
        assert_eq!(
            data,
            vec![
                (b"a1".to_vec(), b"v1".to_vec()),
                (b"a2".to_vec(), b"v2".to_vec()),
            ]
        );
        data.clear();

        engine
            .scan_cf(cf, b"", &[0xFF, 0xFF], false, &mut |key, value| {
                data.push((key.to_vec(), value.to_vec()));
                Ok(true)
            })
            .unwrap();
        assert_eq!(
            data,
            vec![
                (b"a1".to_vec(), b"v1".to_vec()),
                (b"a2".to_vec(), b"v22".to_vec()),
            ]
        );
        data.clear();

        let pair = engine.seek(b"a1").unwrap().unwrap();
        assert_eq!(pair, (b"a1".to_vec(), b"v1".to_vec()));
        assert!(engine.seek(b"a3").unwrap().is_none());
        let pair_cf = engine.seek_cf(cf, b"a1").unwrap().unwrap();
        assert_eq!(pair_cf, (b"a1".to_vec(), b"v1".to_vec()));
        assert!(engine.seek_cf(cf, b"a3").unwrap().is_none());

        let mut index = 0;
        engine
            .scan(b"", &[0xFF, 0xFF], false, &mut |key, value| {
                data.push((key.to_vec(), value.to_vec()));
                index += 1;
                Ok(index != 1)
            })
            .unwrap();

        assert_eq!(data.len(), 1);

        let snap = Snapshot::new(engine.clone());

        engine.put(b"a3", b"v3").unwrap();
        assert!(engine.seek(b"a3").unwrap().is_some());

        let pair = snap.seek(b"a1").unwrap().unwrap();
        assert_eq!(pair, (b"a1".to_vec(), b"v1".to_vec()));
        assert!(snap.seek(b"a3").unwrap().is_none());

        data.clear();

        snap.scan(b"", &[0xFF, 0xFF], false, &mut |key, value| {
            data.push((key.to_vec(), value.to_vec()));
            Ok(true)
        }).unwrap();

        assert_eq!(data.len(), 2);
    }
}
