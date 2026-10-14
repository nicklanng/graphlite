//! The graph API: nodes with labels and properties, typed directed edges
//! with properties, property indexes, traversals, and transactions.
//!
//! Everything lives in one B+tree under tagged keys. Integers are
//! big-endian so byte order is numeric order, and every adjacency list is a
//! single range scan:
//!
//! ```text
//! N | node                         node exists
//! L | node | label                 node's labels
//! l | label | node                 label index
//! P | node | key          → value  node property
//! E | src | type | dst             out-edge
//! e | dst | type | src             in-edge
//! Q | src | type | dst | key → value  edge property
//! X | key | value | node           property index entry
//! I | key                          property `key` is indexed
//! Y | kind | name         → id     symbol table (labels, edge types, keys)
//! y | kind | id           → name
//! M | name                → u64    counters
//! ```
//!
//! Labels, edge types and property keys are interned as u32 symbols.
//!
//! TODO: block-compressed adjacency lists; streaming iterators for very
//! high-degree nodes; a query language.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use crate::btree::BTree;
use crate::error::{Error, Result};
use crate::hash::{IdMap, IdSet};
use crate::pager::{NULL_PAGE, Options, Pager, Stats};
use crate::value::Value;

pub type NodeId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Edges leaving the node.
    Out,
    /// Edges entering the node.
    In,
    /// Both, ignoring direction.
    Both,
}

/// A directed, typed edge.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Edge {
    pub src: NodeId,
    pub edge_type: String,
    pub dst: NodeId,
}

const NODE: u8 = b'N';
const NODE_LABEL: u8 = b'L';
const LABEL_INDEX: u8 = b'l';
const NODE_PROP: u8 = b'P';
const OUT: u8 = b'E';
const IN: u8 = b'e';
const EDGE_PROP: u8 = b'Q';
const PROP_INDEX: u8 = b'X';
const INDEXED: u8 = b'I';
const SYM_BY_NAME: u8 = b'Y';
const SYM_BY_ID: u8 = b'y';
const META: u8 = b'M';

const NEXT_NODE: &[u8] = b"next_node";
const NEXT_SYMBOL: &[u8] = b"next_symbol";
const NODE_COUNT: &[u8] = b"nodes";
const EDGE_COUNT: &[u8] = b"edges";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Sym {
    Label = 0,
    EdgeType = 1,
    PropKey = 2,
}

/// Builds a tagged key.
struct Key(Vec<u8>);

impl Key {
    fn new(tag: u8) -> Key {
        let mut v = Vec::with_capacity(32);
        v.push(tag);
        Key(v)
    }

    fn u32(mut self, v: u32) -> Key {
        self.0.extend(v.to_be_bytes());
        self
    }

    fn u64(mut self, v: u64) -> Key {
        self.0.extend(v.to_be_bytes());
        self
    }

    fn bytes(mut self, b: &[u8]) -> Key {
        self.0.extend(b);
        self
    }
}

impl std::ops::Deref for Key {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

fn be_u32(b: &[u8], off: usize) -> Result<u32> {
    b.get(off..off + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap())).ok_or(Error::Corrupt("short key"))
}

fn be_u64(b: &[u8], off: usize) -> Result<u64> {
    b.get(off..off + 8).map(|s| u64::from_be_bytes(s.try_into().unwrap())).ok_or(Error::Corrupt("short key"))
}

/// An open database file.
///
/// One handle per file: the file is locked while open. All methods take
/// `&mut self`; share a `Db` across threads behind a `Mutex`.
///
/// Changes form an implicit transaction that becomes durable and visible
/// after a crash only on [`Db::commit`]. [`Db::rollback`] discards it, and so
/// does dropping the `Db`.
///
/// A write that fails after changing anything (an I/O error halfway through
/// [`Db::delete_node`], say) may leave the transaction half-applied. Further
/// writes and commits then return [`Error::RollbackRequired`] until
/// [`Db::rollback`]. Errors raised before anything changed, such as
/// [`Error::NodeNotFound`], leave the transaction usable.
pub struct Db {
    tree: BTree,
    /// Symbol caches, cleared on rollback since symbols are transactional.
    sym_ids: HashMap<(Sym, String), u32>,
    sym_names: HashMap<(Sym, u32), String>,
    indexed: Option<HashSet<u32>>,
    /// Counter values, written back to the tree at commit; cleared on rollback.
    counters: HashMap<&'static [u8], u64>,
    counters_dirty: bool,
    /// A write or commit failed partway; only a rollback is accepted.
    poisoned: bool,
}

impl Db {
    /// Opens the database at `path`, creating it if missing. The WAL lives
    /// next to it at `<path>-wal`.
    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        Db::open_with(path, &Options::default())
    }

    pub fn open_with(path: impl AsRef<Path>, opts: &Options) -> Result<Db> {
        let path = path.as_ref();
        let open = |p: &Path| OpenOptions::new().read(true).write(true).create(true).truncate(false).open(p);
        let file = open(path)?;
        lock(&file)?;
        let mut wal_path = OsString::from(path.as_os_str());
        wal_path.push("-wal");
        let wal_path = Path::new(&wal_path);
        let wal_existed = wal_path.try_exists()?;
        let wal = open(wal_path)?;
        if file.metadata()?.len() == 0 || !wal_existed {
            // Make the new directory entries durable before any commit relies on them.
            sync_dir(path)?;
        }

        let pager = if file.metadata()?.len() == 0 {
            Pager::create(file, wal, opts)?
        } else {
            Pager::open(file, wal, opts)?
        };
        let tree = if pager.root() == NULL_PAGE {
            // A new file, or one whose creation crashed before its first commit.
            let mut tree = BTree::create(pager)?;
            tree.pager_mut().commit()?;
            tree.pager_mut().checkpoint()?;
            tree
        } else {
            BTree::open(pager)?
        };
        Ok(Db {
            tree,
            sym_ids: HashMap::new(),
            sym_names: HashMap::new(),
            indexed: None,
            counters: HashMap::new(),
            counters_dirty: false,
            poisoned: false,
        })
    }

    /// Atomically and durably applies every change since the last commit.
    ///
    /// If this fails, the transaction must be rolled back. The commit may
    /// still have reached the disk, so a crash afterwards can reveal it.
    pub fn commit(&mut self) -> Result<()> {
        if self.poisoned {
            return Err(Error::RollbackRequired);
        }
        let res = self.flush_counters().and_then(|()| self.tree.pager_mut().commit());
        self.poisoned = res.is_err();
        res
    }

    fn flush_counters(&mut self) -> Result<()> {
        if self.counters_dirty {
            for (name, v) in &self.counters {
                self.tree.put(&Key::new(META).bytes(name), &v.to_be_bytes())?;
            }
            self.counters_dirty = false;
        }
        Ok(())
    }

    /// Discards every change since the last commit.
    pub fn rollback(&mut self) {
        self.tree.pager_mut().rollback();
        self.sym_ids.clear();
        self.sym_names.clear();
        self.indexed = None;
        self.counters.clear();
        self.counters_dirty = false;
        self.poisoned = false;
    }

    /// Runs `f`, then commits if it returns `Ok` or rolls back if it returns
    /// `Err` or the commit fails. Uncommitted changes made before the call
    /// are included.
    pub fn transaction<T, E: From<Error>>(
        &mut self,
        f: impl FnOnce(&mut Db) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        match f(self) {
            Ok(v) => match self.commit() {
                Ok(()) => Ok(v),
                Err(e) => {
                    self.rollback();
                    Err(e.into())
                }
            },
            Err(e) => {
                self.rollback();
                Err(e)
            }
        }
    }

    /// Discards uncommitted changes, moves the WAL into the main file, and
    /// closes the database, reporting any error that dropping would hide.
    pub fn close(mut self) -> Result<()> {
        self.rollback();
        self.tree.pager_mut().checkpoint()
    }

    pub fn stats(&self) -> Stats {
        self.tree.pager().stats()
    }

    /// Logical size of the database, in bytes.
    pub fn size(&self) -> u64 {
        let p = self.tree.pager();
        p.page_count() * p.page_size() as u64
    }

    // ---- nodes ----

    /// Creates a node with the given labels and returns its id. Ids are
    /// dense and never reused.
    pub fn create_node(&mut self, labels: &[&str]) -> Result<NodeId> {
        self.write(|db| {
            let id = db.counter(NEXT_NODE)?;
            db.tree.put(&Key::new(NODE).u64(id), &[])?;
            db.set_counter(NEXT_NODE, id + 1)?;
            db.bump(NODE_COUNT, 1)?;
            for label in labels {
                db.add_label(id, label)?;
            }
            Ok(id)
        })
    }

    pub fn node_exists(&mut self, id: NodeId) -> Result<bool> {
        self.tree.contains(&Key::new(NODE).u64(id))
    }

    /// Deletes a node with its labels, properties, and every edge touching
    /// it. Returns whether the node existed.
    pub fn delete_node(&mut self, id: NodeId) -> Result<bool> {
        self.write(|db| {
            if !db.node_exists(id)? {
                return Ok(false);
            }
            for (src, t, dst) in db.edge_ids(id, Direction::Both)? {
                db.remove_edge_ids(src, t, dst)?;
            }
            for k in db.scan_keys(&Key::new(NODE_LABEL).u64(id))? {
                let label = be_u32(&k, 9)?;
                db.tree.delete(&k)?;
                db.tree.delete(&Key::new(LABEL_INDEX).u32(label).u64(id))?;
            }
            for (k, v) in db.scan(&Key::new(NODE_PROP).u64(id))? {
                let key = be_u32(&k, 9)?;
                db.unindex(key, &v, id)?;
                db.tree.delete(&k)?;
            }
            db.tree.delete(&Key::new(NODE).u64(id))?;
            db.bump(NODE_COUNT, -1)?;
            Ok(true)
        })
    }

    pub fn node_count(&mut self) -> Result<u64> {
        self.counter(NODE_COUNT)
    }

    // ---- labels ----

    /// Adds `label` to the node. Returns false if it already had it.
    pub fn add_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        self.write(|db| {
            db.require_node(id)?;
            let l = db.intern(Sym::Label, label)?;
            if !db.tree.put_if_absent(&Key::new(NODE_LABEL).u64(id).u32(l), &[])? {
                return Ok(false);
            }
            db.tree.put(&Key::new(LABEL_INDEX).u32(l).u64(id), &[])?;
            Ok(true)
        })
    }

    /// Removes `label` from the node. Returns whether it had it.
    pub fn remove_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        self.write(|db| {
            let Some(l) = db.symbol(Sym::Label, label)? else {
                return Ok(false);
            };
            let found = db.tree.delete(&Key::new(NODE_LABEL).u64(id).u32(l))?;
            db.tree.delete(&Key::new(LABEL_INDEX).u32(l).u64(id))?;
            Ok(found)
        })
    }

    pub fn has_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        match self.symbol(Sym::Label, label)? {
            Some(l) => self.tree.contains(&Key::new(NODE_LABEL).u64(id).u32(l)),
            None => Ok(false),
        }
    }

    pub fn labels(&mut self, id: NodeId) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for k in self.scan_keys(&Key::new(NODE_LABEL).u64(id))? {
            out.push(self.symbol_name(Sym::Label, be_u32(&k, 9)?)?);
        }
        Ok(out)
    }

    /// Nodes carrying `label`, in id order.
    pub fn nodes_with_label(&mut self, label: &str) -> Result<Vec<NodeId>> {
        let Some(l) = self.symbol(Sym::Label, label)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        self.for_each_key(&Key::new(LABEL_INDEX).u32(l), |k| {
            out.push(be_u64(k, 5)?);
            Ok(())
        })?;
        Ok(out)
    }

    // ---- node properties ----

    pub fn set_node_prop(&mut self, id: NodeId, key: &str, value: impl Into<Value>) -> Result<()> {
        self.write(|db| {
            db.require_node(id)?;
            let k = db.intern(Sym::PropKey, key)?;
            let enc = value.into().encode();
            let pk = Key::new(NODE_PROP).u64(id).u32(k);
            if db.is_indexed(k)? {
                let ik = Key::new(PROP_INDEX).u32(k).bytes(&enc).u64(id);
                if ik.len() > db.tree.max_key_len() {
                    return Err(Error::TooLarge);
                }
                if let Some(old) = db.tree.get(&pk)? {
                    db.unindex(k, &old, id)?;
                }
                db.tree.put(&ik, &[])?;
            }
            db.tree.put(&pk, &enc)
        })
    }

    pub fn node_prop(&mut self, id: NodeId, key: &str) -> Result<Option<Value>> {
        let Some(k) = self.symbol(Sym::PropKey, key)? else {
            return Ok(None);
        };
        self.tree.get(&Key::new(NODE_PROP).u64(id).u32(k))?.map(|v| Value::decode_all(&v)).transpose()
    }

    /// All properties of the node, ordered by when each key was first used.
    pub fn node_props(&mut self, id: NodeId) -> Result<Vec<(String, Value)>> {
        let mut out = Vec::new();
        for (k, v) in self.scan(&Key::new(NODE_PROP).u64(id))? {
            out.push((self.symbol_name(Sym::PropKey, be_u32(&k, 9)?)?, Value::decode_all(&v)?));
        }
        Ok(out)
    }

    /// Removes a property, returning its old value.
    pub fn remove_node_prop(&mut self, id: NodeId, key: &str) -> Result<Option<Value>> {
        self.write(|db| {
            let Some(k) = db.symbol(Sym::PropKey, key)? else {
                return Ok(None);
            };
            let pk = Key::new(NODE_PROP).u64(id).u32(k);
            let Some(old) = db.tree.get(&pk)? else {
                return Ok(None);
            };
            db.unindex(k, &old, id)?;
            db.tree.delete(&pk)?;
            Value::decode_all(&old).map(Some)
        })
    }

    // ---- indexes ----

    /// Indexes node property `key` so [`Db::find_nodes`] doesn't scan every
    /// node. Existing values are indexed immediately. Indexed values must fit
    /// in a key (about 1 KiB at the default page size).
    pub fn create_index(&mut self, key: &str) -> Result<()> {
        self.write(|db| {
            let k = db.intern(Sym::PropKey, key)?;
            if db.is_indexed(k)? {
                return Ok(());
            }
            // Build every entry first, so a value too large to index fails
            // the call before anything is written.
            let max_key = db.tree.max_key_len();
            let mut entries = Vec::new();
            let mut c = db.tree.seek(&[NODE_PROP])?;
            while let Some((pk, v)) = c.next()? {
                if pk.first() != Some(&NODE_PROP) {
                    break;
                }
                if be_u32(pk, 9)? == k {
                    let ik = Key::new(PROP_INDEX).u32(k).bytes(v).u64(be_u64(pk, 1)?);
                    if ik.len() > max_key {
                        return Err(Error::TooLarge);
                    }
                    entries.push(ik);
                }
            }
            drop(c);
            for ik in entries {
                db.tree.put(&ik, &[])?;
            }
            db.tree.put(&Key::new(INDEXED).u32(k), &[])?;
            db.indexed.get_or_insert_with(HashSet::new).insert(k);
            Ok(())
        })
    }

    pub fn drop_index(&mut self, key: &str) -> Result<bool> {
        self.write(|db| {
            let Some(k) = db.symbol(Sym::PropKey, key)? else {
                return Ok(false);
            };
            if !db.is_indexed(k)? {
                return Ok(false);
            }
            for k in db.scan_keys(&Key::new(PROP_INDEX).u32(k))? {
                db.tree.delete(&k)?;
            }
            db.tree.delete(&Key::new(INDEXED).u32(k))?;
            db.indexed.as_mut().unwrap().remove(&k);
            Ok(true)
        })
    }

    /// Nodes whose property `key` equals `value`, in id order. Uses the
    /// index on `key` if there is one, otherwise scans every node.
    pub fn find_nodes(&mut self, key: &str, value: impl Into<Value>) -> Result<Vec<NodeId>> {
        let Some(k) = self.symbol(Sym::PropKey, key)? else {
            return Ok(Vec::new());
        };
        let enc = value.into().encode();
        if self.is_indexed(k)? {
            let prefix = Key::new(PROP_INDEX).u32(k).bytes(&enc);
            return self.scan_keys(&prefix)?.iter().map(|ik| be_u64(ik, prefix.len())).collect();
        }
        let mut out = Vec::new();
        for nk in self.scan_keys(&[NODE])? {
            let id = be_u64(&nk, 1)?;
            if self.tree.get(&Key::new(NODE_PROP).u64(id).u32(k))?.is_some_and(|v| v == enc) {
                out.push(id);
            }
        }
        Ok(out)
    }

    // ---- edges ----

    /// Adds the edge `src -[edge_type]-> dst`. Returns false if it already
    /// existed; there is at most one edge per (src, type, dst).
    pub fn add_edge(&mut self, src: NodeId, edge_type: &str, dst: NodeId) -> Result<bool> {
        self.write(|db| {
            db.require_node(src)?;
            if dst != src {
                db.require_node(dst)?;
            }
            let t = db.intern(Sym::EdgeType, edge_type)?;
            if !db.tree.put_if_absent(&Key::new(OUT).u64(src).u32(t).u64(dst), &[])? {
                return Ok(false);
            }
            db.tree.put(&Key::new(IN).u64(dst).u32(t).u64(src), &[])?;
            db.bump(EDGE_COUNT, 1)?;
            Ok(true)
        })
    }

    /// Removes the edge and its properties. Returns whether it existed.
    pub fn remove_edge(&mut self, src: NodeId, edge_type: &str, dst: NodeId) -> Result<bool> {
        self.write(|db| {
            match db.symbol(Sym::EdgeType, edge_type)? {
                Some(t) => db.remove_edge_ids(src, t, dst),
                None => Ok(false),
            }
        })
    }

    fn remove_edge_ids(&mut self, src: NodeId, t: u32, dst: NodeId) -> Result<bool> {
        if !self.tree.delete(&Key::new(OUT).u64(src).u32(t).u64(dst))? {
            return Ok(false);
        }
        self.tree.delete(&Key::new(IN).u64(dst).u32(t).u64(src))?;
        for k in self.scan_keys(&Key::new(EDGE_PROP).u64(src).u32(t).u64(dst))? {
            self.tree.delete(&k)?;
        }
        self.bump(EDGE_COUNT, -1)?;
        Ok(true)
    }

    pub fn has_edge(&mut self, src: NodeId, edge_type: &str, dst: NodeId) -> Result<bool> {
        match self.symbol(Sym::EdgeType, edge_type)? {
            Some(t) => self.tree.contains(&Key::new(OUT).u64(src).u32(t).u64(dst)),
            None => Ok(false),
        }
    }

    pub fn edge_count(&mut self) -> Result<u64> {
        self.counter(EDGE_COUNT)
    }

    /// Neighbors of `id` over edges of `edge_type`, in id order. With
    /// [`Direction::Both`], a node linked both ways appears once.
    pub fn neighbors(&mut self, id: NodeId, edge_type: &str, dir: Direction) -> Result<Vec<NodeId>> {
        match self.symbol(Sym::EdgeType, edge_type)? {
            Some(t) => self.step(id, Some(t), dir),
            None => Ok(Vec::new()),
        }
    }

    /// Number of `edge_type` edges at `id` in direction `dir`. With
    /// [`Direction::Both`], a self-loop counts twice.
    pub fn degree(&mut self, id: NodeId, edge_type: &str, dir: Direction) -> Result<u64> {
        let Some(t) = self.symbol(Sym::EdgeType, edge_type)? else {
            return Ok(0);
        };
        let mut n = 0;
        for (tag, wanted) in [(OUT, dir != Direction::In), (IN, dir != Direction::Out)] {
            if wanted {
                self.for_each_key(&Key::new(tag).u64(id).u32(t), |_| {
                    n += 1;
                    Ok(())
                })?;
            }
        }
        Ok(n)
    }

    /// Every edge at `id` in direction `dir`, of any type.
    pub fn edges(&mut self, id: NodeId, dir: Direction) -> Result<Vec<Edge>> {
        let mut out = Vec::new();
        for (src, t, dst) in self.edge_ids(id, dir)? {
            out.push(Edge { src, edge_type: self.symbol_name(Sym::EdgeType, t)?, dst });
        }
        Ok(out)
    }

    /// (src, type, dst) of every edge at `id`; a self-loop is listed once.
    fn edge_ids(&mut self, id: NodeId, dir: Direction) -> Result<Vec<(NodeId, u32, NodeId)>> {
        let mut out = Vec::new();
        if dir != Direction::In {
            for k in self.scan_keys(&Key::new(OUT).u64(id))? {
                out.push((id, be_u32(&k, 9)?, be_u64(&k, 13)?));
            }
        }
        if dir != Direction::Out {
            for k in self.scan_keys(&Key::new(IN).u64(id))? {
                let src = be_u64(&k, 13)?;
                if dir == Direction::In || src != id {
                    out.push((src, be_u32(&k, 9)?, id));
                }
            }
        }
        Ok(out)
    }

    // ---- edge properties ----

    pub fn set_edge_prop(
        &mut self,
        src: NodeId,
        edge_type: &str,
        dst: NodeId,
        key: &str,
        value: impl Into<Value>,
    ) -> Result<()> {
        self.write(|db| {
            let t = db.symbol(Sym::EdgeType, edge_type)?.ok_or(Error::EdgeNotFound)?;
            if !db.tree.contains(&Key::new(OUT).u64(src).u32(t).u64(dst))? {
                return Err(Error::EdgeNotFound);
            }
            let k = db.intern(Sym::PropKey, key)?;
            db.tree.put(&Key::new(EDGE_PROP).u64(src).u32(t).u64(dst).u32(k), &value.into().encode())
        })
    }

    pub fn edge_prop(&mut self, src: NodeId, edge_type: &str, dst: NodeId, key: &str) -> Result<Option<Value>> {
        let (Some(t), Some(k)) = (self.symbol(Sym::EdgeType, edge_type)?, self.symbol(Sym::PropKey, key)?) else {
            return Ok(None);
        };
        let pk = Key::new(EDGE_PROP).u64(src).u32(t).u64(dst).u32(k);
        self.tree.get(&pk)?.map(|v| Value::decode_all(&v)).transpose()
    }

    pub fn edge_props(&mut self, src: NodeId, edge_type: &str, dst: NodeId) -> Result<Vec<(String, Value)>> {
        let Some(t) = self.symbol(Sym::EdgeType, edge_type)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for (k, v) in self.scan(&Key::new(EDGE_PROP).u64(src).u32(t).u64(dst))? {
            out.push((self.symbol_name(Sym::PropKey, be_u32(&k, 21)?)?, Value::decode_all(&v)?));
        }
        Ok(out)
    }

    pub fn remove_edge_prop(&mut self, src: NodeId, edge_type: &str, dst: NodeId, key: &str) -> Result<Option<Value>> {
        self.write(|db| {
            let (Some(t), Some(k)) = (db.symbol(Sym::EdgeType, edge_type)?, db.symbol(Sym::PropKey, key)?) else {
                return Ok(None);
            };
            let pk = Key::new(EDGE_PROP).u64(src).u32(t).u64(dst).u32(k);
            let old = db.tree.get(&pk)?;
            if old.is_some() {
                db.tree.delete(&pk)?;
            }
            old.map(|v| Value::decode_all(&v)).transpose()
        })
    }

    // ---- traversal ----

    /// Breadth-first traversal from `start` over edges of `edge_type` (any
    /// type if `None`), up to `max_depth` hops. Returns each reached node
    /// with its distance, in visiting order, starting with `(start, 0)`.
    pub fn traverse(
        &mut self,
        start: NodeId,
        edge_type: Option<&str>,
        dir: Direction,
        max_depth: u32,
    ) -> Result<Vec<(NodeId, u32)>> {
        self.require_node(start)?;
        let Some(t) = self.edge_filter(edge_type)? else {
            return Ok(vec![(start, 0)]);
        };
        let mut seen = IdSet::default();
        seen.insert(start);
        let mut out = vec![(start, 0)];
        let mut next = Vec::new();
        let mut i = 0;
        while i < out.len() {
            let (node, depth) = out[i];
            i += 1;
            if depth == max_depth {
                continue;
            }
            next.clear();
            self.step_into(node, t, dir, &mut next)?;
            for &n in &next {
                if seen.insert(n) {
                    out.push((n, depth + 1));
                }
            }
        }
        Ok(out)
    }

    /// A shortest path from `from` to `to` over edges of `edge_type` (any
    /// type if `None`), as the list of nodes along it, or `None` if `to` is
    /// unreachable.
    ///
    /// Searches from both ends, always expanding the smaller frontier by one
    /// level, so it visits far fewer nodes than a one-sided search.
    pub fn shortest_path(
        &mut self,
        from: NodeId,
        to: NodeId,
        edge_type: Option<&str>,
        dir: Direction,
    ) -> Result<Option<Vec<NodeId>>> {
        self.require_node(from)?;
        self.require_node(to)?;
        if from == to {
            return Ok(Some(vec![from]));
        }
        let Some(t) = self.edge_filter(edge_type)? else {
            return Ok(None);
        };
        let back_dir = match dir {
            Direction::Out => Direction::In,
            Direction::In => Direction::Out,
            Direction::Both => Direction::Both,
        };
        // Each side maps visited nodes to (parent toward its root, depth).
        let mut sides = [IdMap::default(), IdMap::default()];
        sides[0].insert(from, (from, 0u32));
        sides[1].insert(to, (to, 0u32));
        let mut frontiers = [vec![from], vec![to]];
        let mut next = Vec::new();
        let mut neighbors = Vec::new();
        while !frontiers[0].is_empty() && !frontiers[1].is_empty() {
            let side = usize::from(frontiers[1].len() < frontiers[0].len());
            let d = if side == 0 { dir } else { back_dir };
            // The best meeting found while expanding this level: (length, u, v)
            // for an edge from u on this side to v already seen by the other.
            let mut best: Option<(u32, NodeId, NodeId)> = None;
            next.clear();
            for &u in &frontiers[side] {
                let du = sides[side][&u].1;
                neighbors.clear();
                self.step_into(u, t, d, &mut neighbors)?;
                for &v in &neighbors {
                    if let Some(&(_, dv)) = sides[1 - side].get(&v)
                        && best.is_none_or(|(len, _, _)| du + 1 + dv < len)
                    {
                        best = Some((du + 1 + dv, u, v));
                    }
                    if let std::collections::hash_map::Entry::Vacant(e) = sides[side].entry(v) {
                        e.insert((u, du + 1));
                        next.push(v);
                    }
                }
            }
            if let Some((_, u, v)) = best {
                // Walk u back to this side's root and v back to the other's.
                let chain = |map: &IdMap<(NodeId, u32)>, mut n: NodeId| {
                    let mut c = vec![n];
                    while map[&n].0 != n {
                        n = map[&n].0;
                        c.push(n);
                    }
                    c
                };
                let (mut path, tail) = if side == 0 {
                    (chain(&sides[0], u), chain(&sides[1], v))
                } else {
                    (chain(&sides[0], v), chain(&sides[1], u))
                };
                path.reverse();
                path.extend(tail);
                return Ok(Some(path));
            }
            std::mem::swap(&mut frontiers[side], &mut next);
        }
        Ok(None)
    }

    /// Resolves an optional edge type filter. `None` means no edge can match.
    fn edge_filter(&mut self, edge_type: Option<&str>) -> Result<Option<Option<u32>>> {
        match edge_type {
            None => Ok(Some(None)),
            Some(name) => Ok(self.symbol(Sym::EdgeType, name)?.map(Some)),
        }
    }

    /// Distinct neighbors of `id` over type `t` (any if `None`), in id order.
    fn step(&mut self, id: NodeId, t: Option<u32>, dir: Direction) -> Result<Vec<NodeId>> {
        let mut out = Vec::new();
        self.step_into(id, t, dir, &mut out)?;
        if dir == Direction::Both || t.is_none() {
            out.sort_unstable();
            out.dedup();
        }
        Ok(out)
    }

    /// Appends the neighbors of `id` over type `t` (any if `None`) to `out`,
    /// unsorted and possibly repeated.
    fn step_into(&mut self, id: NodeId, t: Option<u32>, dir: Direction, out: &mut Vec<NodeId>) -> Result<()> {
        for (tag, wanted) in [(OUT, dir != Direction::In), (IN, dir != Direction::Out)] {
            if !wanted {
                continue;
            }
            let mut prefix = [0u8; 13];
            prefix[0] = tag;
            prefix[1..9].copy_from_slice(&id.to_be_bytes());
            let len = match t {
                Some(t) => {
                    prefix[9..13].copy_from_slice(&t.to_be_bytes());
                    13
                }
                None => 9,
            };
            self.for_each_key(&prefix[..len], |k| {
                out.push(be_u64(k, 13)?);
                Ok(())
            })?;
        }
        Ok(())
    }

    // ---- internals ----

    /// Runs a write. If it fails after changing the tree, the transaction
    /// may be half-applied, so it is poisoned until rolled back.
    fn write<T>(&mut self, f: impl FnOnce(&mut Db) -> Result<T>) -> Result<T> {
        if self.poisoned {
            return Err(Error::RollbackRequired);
        }
        let writes = self.tree.writes();
        let res = f(self);
        if res.is_err() && self.tree.writes() != writes {
            self.poisoned = true;
        }
        res
    }

    fn require_node(&mut self, id: NodeId) -> Result<()> {
        if self.node_exists(id)? { Ok(()) } else { Err(Error::NodeNotFound(id)) }
    }

    /// Every (key, value) whose key starts with `prefix`.
    fn scan(&mut self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut c = self.tree.seek(prefix)?;
        let mut out = Vec::new();
        while let Some((k, v)) = c.next()? {
            if !k.starts_with(prefix) {
                break;
            }
            out.push((k.to_vec(), v.to_vec()));
        }
        Ok(out)
    }

    /// Calls `f` on every key starting with `prefix`, without copying keys.
    fn for_each_key(&mut self, prefix: &[u8], mut f: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let mut c = self.tree.seek(prefix)?;
        while let Some((k, _)) = c.next()? {
            if !k.starts_with(prefix) {
                break;
            }
            f(k)?;
        }
        Ok(())
    }

    fn scan_keys(&mut self, prefix: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut c = self.tree.seek(prefix)?;
        let mut out = Vec::new();
        while let Some((k, _)) = c.next()? {
            if !k.starts_with(prefix) {
                break;
            }
            out.push(k.to_vec());
        }
        Ok(out)
    }

    fn counter(&mut self, name: &'static [u8]) -> Result<u64> {
        if let Some(&v) = self.counters.get(name) {
            return Ok(v);
        }
        let v = match self.tree.get(&Key::new(META).bytes(name))? {
            Some(v) => u64::from_be_bytes(v.try_into().map_err(|_| Error::Corrupt("bad counter"))?),
            None => 0,
        };
        self.counters.insert(name, v);
        Ok(v)
    }

    fn set_counter(&mut self, name: &'static [u8], v: u64) -> Result<()> {
        self.counters.insert(name, v);
        self.counters_dirty = true;
        Ok(())
    }

    fn bump(&mut self, name: &'static [u8], delta: i64) -> Result<()> {
        let v = self.counter(name)?;
        self.set_counter(name, v.wrapping_add_signed(delta))
    }

    fn symbol(&mut self, kind: Sym, name: &str) -> Result<Option<u32>> {
        if let Some(&id) = self.sym_ids.get(&(kind, name.to_owned())) {
            return Ok(Some(id));
        }
        let key = Key::new(SYM_BY_NAME).bytes(&[kind as u8]).bytes(name.as_bytes());
        if key.len() > self.tree.max_key_len() {
            return Ok(None);
        }
        let Some(v) = self.tree.get(&key)? else {
            return Ok(None);
        };
        let id = u32::from_be_bytes(v.try_into().map_err(|_| Error::Corrupt("bad symbol id"))?);
        self.sym_ids.insert((kind, name.to_owned()), id);
        Ok(Some(id))
    }

    fn intern(&mut self, kind: Sym, name: &str) -> Result<u32> {
        if let Some(id) = self.symbol(kind, name)? {
            return Ok(id);
        }
        let id = u32::try_from(self.counter(NEXT_SYMBOL)?).map_err(|_| Error::TooLarge)?;
        self.tree.put(&Key::new(SYM_BY_NAME).bytes(&[kind as u8]).bytes(name.as_bytes()), &id.to_be_bytes())?;
        self.tree.put(&Key::new(SYM_BY_ID).bytes(&[kind as u8]).u32(id), name.as_bytes())?;
        self.set_counter(NEXT_SYMBOL, u64::from(id) + 1)?;
        self.sym_ids.insert((kind, name.to_owned()), id);
        Ok(id)
    }

    fn symbol_name(&mut self, kind: Sym, id: u32) -> Result<String> {
        if let Some(name) = self.sym_names.get(&(kind, id)) {
            return Ok(name.clone());
        }
        let v = self
            .tree
            .get(&Key::new(SYM_BY_ID).bytes(&[kind as u8]).u32(id))?
            .ok_or(Error::Corrupt("dangling symbol"))?;
        let name = String::from_utf8(v).map_err(|_| Error::Corrupt("bad symbol name"))?;
        self.sym_names.insert((kind, id), name.clone());
        Ok(name)
    }

    fn is_indexed(&mut self, key: u32) -> Result<bool> {
        if self.indexed.is_none() {
            let keys = self.scan_keys(&[INDEXED])?;
            self.indexed = Some(keys.iter().map(|k| be_u32(k, 1)).collect::<Result<_>>()?);
        }
        Ok(self.indexed.as_ref().unwrap().contains(&key))
    }

    /// Removes the index entry for node `id` having encoded `value` under `key`, if indexed.
    fn unindex(&mut self, key: u32, value: &[u8], id: NodeId) -> Result<()> {
        if self.is_indexed(key)? {
            self.tree.delete(&Key::new(PROP_INDEX).u32(key).bytes(value).u64(id))?;
        }
        Ok(())
    }
}

/// Fsyncs the directory holding `path`, making newly created files in it durable.
#[cfg(unix)]
fn sync_dir(path: &Path) -> Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Windows has no directory fsync to call.
#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> Result<()> {
    Ok(())
}

fn lock(file: &File) -> Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(Error::Locked),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn a_write_failing_partway_requires_rollback() {
        let dir = TempDir::new();
        let mut db = Db::open(dir.path().join("t.db")).unwrap();
        let a = db.create_node(&["A"]).unwrap();
        db.commit().unwrap();
        // No public call fails halfway on demand, so fail one by hand.
        let res: Result<()> = db.write(|db| {
            db.tree.delete(&Key::new(NODE).u64(a))?;
            Err(Error::Corrupt("injected"))
        });
        assert!(res.is_err());
        assert!(matches!(db.create_node(&[]), Err(Error::RollbackRequired)));
        assert!(matches!(db.commit(), Err(Error::RollbackRequired)));
        db.rollback();
        assert!(db.node_exists(a).unwrap());
        db.create_node(&[]).unwrap();
        db.commit().unwrap();
        assert_eq!(db.node_count().unwrap(), 2);
    }
}
