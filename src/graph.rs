//! Graph layer: nodes and directed, labeled edges over the B+tree.
//!
//! Each edge is stored twice, as value-less keys, so both directions are a
//! range scan (integers big-endian, so byte order is numeric order):
//!
//! ```text
//! out:  'o' | pred u32 | src u64 | dst u64
//! in:   'i' | pred u32 | dst u64 | src u64
//! ```
//!
//! Scanning the 13-byte prefix (tag, pred, node) yields that node's
//! neighbors in id order: its posting list. Node ids are dense u64s handed
//! out from a counter key.
//!
//! TODO: block-compressed posting lists, node/edge properties, label index,
//! scans across all predicates.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use crate::btree::{BTree, Cursor};
use crate::error::{Error, Result};
use crate::pager::{Options, Pager, Stats};

pub type NodeId = u64;
pub type Predicate = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
}

const TAG_OUT: u8 = b'o';
const TAG_IN: u8 = b'i';
const PREFIX_LEN: usize = 13;
const EDGE_KEY_LEN: usize = 21;
const NEXT_NODE_KEY: &[u8] = b"m:next_node";

fn edge_key(tag: u8, pred: Predicate, a: NodeId, b: NodeId) -> [u8; EDGE_KEY_LEN] {
    let mut k = [0u8; EDGE_KEY_LEN];
    k[0] = tag;
    k[1..5].copy_from_slice(&pred.to_be_bytes());
    k[5..13].copy_from_slice(&a.to_be_bytes());
    k[13..21].copy_from_slice(&b.to_be_bytes());
    k
}

/// An open database file. Single process, single handle: the file is locked
/// while open.
///
/// Changes reach disk durably on [`Db::commit`]. Until the WAL lands, pages
/// evicted from the cache are written in place before commit, so a crash can
/// leave a partially applied state.
pub struct Db {
    tree: BTree,
}

impl Db {
    /// Opens the database at `path`, creating it if missing.
    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        Db::open_with(path, &Options::default())
    }

    pub fn open_with(path: impl AsRef<Path>, opts: &Options) -> Result<Db> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        lock(&file)?;
        let tree = if file.metadata()?.len() == 0 {
            let mut tree = BTree::create(Pager::create(file, opts)?)?;
            tree.pager_mut().flush()?;
            tree
        } else {
            BTree::open(Pager::open(file, opts)?)?
        };
        Ok(Db { tree })
    }

    /// Writes all changes to disk and fsyncs.
    pub fn commit(&mut self) -> Result<()> {
        self.tree.pager_mut().flush()
    }

    pub fn stats(&self) -> Stats {
        self.tree.pager().stats()
    }

    /// Size of the database file, in bytes.
    pub fn file_size(&self) -> u64 {
        let p = self.tree.pager();
        p.page_count() * p.page_size() as u64
    }

    pub fn create_node(&mut self) -> Result<NodeId> {
        let id = match self.tree.get(NEXT_NODE_KEY)? {
            Some(v) => u64::from_be_bytes(v.try_into().map_err(|_| Error::Corrupt("bad node counter"))?),
            None => 0,
        };
        self.tree.put(NEXT_NODE_KEY, &(id + 1).to_be_bytes())?;
        Ok(id)
    }

    /// Adds the edge `src -pred-> dst`. Adding an existing edge is a no-op.
    pub fn add_edge(&mut self, src: NodeId, pred: Predicate, dst: NodeId) -> Result<()> {
        self.tree.put(&edge_key(TAG_OUT, pred, src, dst), &[])?;
        self.tree.put(&edge_key(TAG_IN, pred, dst, src), &[])
    }

    /// Removes the edge, returning whether it existed.
    pub fn remove_edge(&mut self, src: NodeId, pred: Predicate, dst: NodeId) -> Result<bool> {
        let existed = self.tree.delete(&edge_key(TAG_OUT, pred, src, dst))?;
        self.tree.delete(&edge_key(TAG_IN, pred, dst, src))?;
        Ok(existed)
    }

    pub fn has_edge(&mut self, src: NodeId, pred: Predicate, dst: NodeId) -> Result<bool> {
        self.tree.contains(&edge_key(TAG_OUT, pred, src, dst))
    }

    /// Neighbors of `node` over `pred` in `dir`, in ascending id order.
    pub fn neighbors(&mut self, node: NodeId, pred: Predicate, dir: Direction) -> Result<Neighbors<'_>> {
        let tag = if dir == Direction::Out { TAG_OUT } else { TAG_IN };
        let key = edge_key(tag, pred, node, 0);
        let mut prefix = [0u8; PREFIX_LEN];
        prefix.copy_from_slice(&key[..PREFIX_LEN]);
        Ok(Neighbors { cursor: Some(self.tree.seek(&prefix)?), prefix })
    }
}

fn lock(file: &File) -> Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(Error::Locked),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Iterator over a node's neighbors; see [`Db::neighbors`].
pub struct Neighbors<'db> {
    cursor: Option<Cursor<'db>>,
    prefix: [u8; PREFIX_LEN],
}

impl Iterator for Neighbors<'_> {
    type Item = Result<NodeId>;

    fn next(&mut self) -> Option<Result<NodeId>> {
        let cursor = self.cursor.as_mut()?;
        let item = match cursor.next() {
            Ok(Some((k, _))) if k.starts_with(&self.prefix) => match k.len() {
                EDGE_KEY_LEN => Some(Ok(u64::from_be_bytes(k[PREFIX_LEN..].try_into().unwrap()))),
                _ => Some(Err(Error::Corrupt("bad edge key length"))),
            },
            Ok(_) => None,
            Err(e) => Some(Err(e)),
        };
        if !matches!(item, Some(Ok(_))) {
            self.cursor = None; // done: drop the cursor and its pin
        }
        item
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    const KNOWS: Predicate = 1;
    const LIKES: Predicate = 2;

    fn collect(db: &mut Db, node: NodeId, pred: Predicate, dir: Direction) -> Vec<NodeId> {
        db.neighbors(node, pred, dir).unwrap().map(|n| n.unwrap()).collect()
    }

    #[test]
    fn edges_both_directions_and_persistence() {
        let dir = TempDir::new();
        let path = dir.path().join("g.db");
        {
            let mut db = Db::open(&path).unwrap();
            let ids: Vec<NodeId> = (0..5).map(|_| db.create_node().unwrap()).collect();
            assert_eq!(ids, [0, 1, 2, 3, 4]);
            db.add_edge(0, KNOWS, 3).unwrap();
            db.add_edge(0, KNOWS, 1).unwrap();
            db.add_edge(0, KNOWS, 1).unwrap(); // duplicate
            db.add_edge(2, KNOWS, 1).unwrap();
            db.add_edge(0, LIKES, 4).unwrap();
            db.commit().unwrap();
        }
        let mut db = Db::open(&path).unwrap();
        assert_eq!(collect(&mut db, 0, KNOWS, Direction::Out), [1, 3]);
        assert_eq!(collect(&mut db, 1, KNOWS, Direction::In), [0, 2]);
        assert_eq!(collect(&mut db, 0, LIKES, Direction::Out), [4]);
        assert_eq!(collect(&mut db, 4, KNOWS, Direction::Out), []);
        assert!(db.has_edge(2, KNOWS, 1).unwrap());

        assert!(db.remove_edge(0, KNOWS, 1).unwrap());
        assert!(!db.remove_edge(0, KNOWS, 1).unwrap());
        assert_eq!(collect(&mut db, 1, KNOWS, Direction::In), [2]);
        assert_eq!(db.create_node().unwrap(), 5);
    }

    #[test]
    fn high_degree_node_spans_many_leaves() {
        let dir = TempDir::new();
        let opts = Options { page_size: 512, cache_pages: 16 };
        let mut db = Db::open_with(dir.path().join("g.db"), &opts).unwrap();
        for dst in (0..5_000).rev() {
            db.add_edge(42, KNOWS, dst).unwrap();
            db.add_edge(43, KNOWS, dst).unwrap();
        }
        let out = collect(&mut db, 42, KNOWS, Direction::Out);
        assert_eq!(out, (0..5_000).collect::<Vec<_>>());
        assert_eq!(collect(&mut db, 7, KNOWS, Direction::In), [42, 43]);
    }

    #[test]
    fn second_handle_is_locked_out() {
        let dir = TempDir::new();
        let path = dir.path().join("g.db");
        let _db = Db::open(&path).unwrap();
        assert!(matches!(Db::open(&path), Err(Error::Locked)));
    }
}
