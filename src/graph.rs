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

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use crate::btree::BTree;
use crate::error::{Error, Result};
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
pub struct Db {
    tree: BTree,
    /// Symbol caches, cleared on rollback since symbols are transactional.
    sym_ids: HashMap<(Sym, String), u32>,
    sym_names: HashMap<(Sym, u32), String>,
    indexed: Option<HashSet<u32>>,
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
        let wal = open(Path::new(&wal_path))?;

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
        Ok(Db { tree, sym_ids: HashMap::new(), sym_names: HashMap::new(), indexed: None })
    }

    /// Atomically and durably applies every change since the last commit.
    pub fn commit(&mut self) -> Result<()> {
        self.tree.pager_mut().commit()
    }

    /// Discards every change since the last commit.
    pub fn rollback(&mut self) {
        self.tree.pager_mut().rollback();
        self.sym_ids.clear();
        self.sym_names.clear();
        self.indexed = None;
    }

    /// Runs `f`, then commits if it returns `Ok` or rolls back if it returns
    /// `Err`. Uncommitted changes made before the call are included.
    pub fn transaction<T, E: From<Error>>(
        &mut self,
        f: impl FnOnce(&mut Db) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        match f(self) {
            Ok(v) => {
                self.commit()?;
                Ok(v)
            }
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
        let id = self.counter(NEXT_NODE)?;
        self.set_counter(NEXT_NODE, id + 1)?;
        self.tree.put(&Key::new(NODE).u64(id), &[])?;
        self.bump(NODE_COUNT, 1)?;
        for label in labels {
            self.add_label(id, label)?;
        }
        Ok(id)
    }

    pub fn node_exists(&mut self, id: NodeId) -> Result<bool> {
        self.tree.contains(&Key::new(NODE).u64(id))
    }

    /// Deletes a node with its labels, properties, and every edge touching
    /// it. Returns whether the node existed.
    pub fn delete_node(&mut self, id: NodeId) -> Result<bool> {
        if !self.node_exists(id)? {
            return Ok(false);
        }
        for (src, t, dst) in self.edge_ids(id, Direction::Both)? {
            self.remove_edge_ids(src, t, dst)?;
        }
        for k in self.scan_keys(&Key::new(NODE_LABEL).u64(id))? {
            let label = be_u32(&k, 9)?;
            self.tree.delete(&k)?;
            self.tree.delete(&Key::new(LABEL_INDEX).u32(label).u64(id))?;
        }
        for (k, v) in self.scan(&Key::new(NODE_PROP).u64(id))? {
            let key = be_u32(&k, 9)?;
            self.unindex(key, &v, id)?;
            self.tree.delete(&k)?;
        }
        self.tree.delete(&Key::new(NODE).u64(id))?;
        self.bump(NODE_COUNT, -1)?;
        Ok(true)
    }

    pub fn node_count(&mut self) -> Result<u64> {
        self.counter(NODE_COUNT)
    }

    // ---- labels ----

    /// Adds `label` to the node. Returns false if it already had it.
    pub fn add_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        self.require_node(id)?;
        let l = self.intern(Sym::Label, label)?;
        let key = Key::new(NODE_LABEL).u64(id).u32(l);
        if self.tree.contains(&key)? {
            return Ok(false);
        }
        self.tree.put(&key, &[])?;
        self.tree.put(&Key::new(LABEL_INDEX).u32(l).u64(id), &[])?;
        Ok(true)
    }

    /// Removes `label` from the node. Returns whether it had it.
    pub fn remove_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        let Some(l) = self.symbol(Sym::Label, label)? else {
            return Ok(false);
        };
        let found = self.tree.delete(&Key::new(NODE_LABEL).u64(id).u32(l))?;
        self.tree.delete(&Key::new(LABEL_INDEX).u32(l).u64(id))?;
        Ok(found)
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
        self.scan_keys(&Key::new(LABEL_INDEX).u32(l))?.iter().map(|k| be_u64(k, 5)).collect()
    }

    // ---- node properties ----

    pub fn set_node_prop(&mut self, id: NodeId, key: &str, value: impl Into<Value>) -> Result<()> {
        self.require_node(id)?;
        let k = self.intern(Sym::PropKey, key)?;
        let enc = value.into().encode();
        let pk = Key::new(NODE_PROP).u64(id).u32(k);
        if self.is_indexed(k)? {
            if let Some(old) = self.tree.get(&pk)? {
                self.unindex(k, &old, id)?;
            }
            self.tree.put(&Key::new(PROP_INDEX).u32(k).bytes(&enc).u64(id), &[])?;
        }
        self.tree.put(&pk, &enc)
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
        let Some(k) = self.symbol(Sym::PropKey, key)? else {
            return Ok(None);
        };
        let pk = Key::new(NODE_PROP).u64(id).u32(k);
        let Some(old) = self.tree.get(&pk)? else {
            return Ok(None);
        };
        self.unindex(k, &old, id)?;
        self.tree.delete(&pk)?;
        Value::decode_all(&old).map(Some)
    }

    // ---- indexes ----

    /// Indexes node property `key` so [`Db::find_nodes`] doesn't scan every
    /// node. Existing values are indexed immediately. Indexed values must fit
    /// in a key (about 1 KiB at the default page size).
    pub fn create_index(&mut self, key: &str) -> Result<()> {
        let k = self.intern(Sym::PropKey, key)?;
        if self.is_indexed(k)? {
            return Ok(());
        }
        let entries: Vec<(NodeId, Vec<u8>)> = self
            .scan(&[NODE_PROP])?
            .into_iter()
            .filter(|(pk, _)| be_u32(pk, 9).is_ok_and(|pk_key| pk_key == k))
            .map(|(pk, v)| be_u64(&pk, 1).map(|id| (id, v)))
            .collect::<Result<_>>()?;
        for (id, v) in entries {
            self.tree.put(&Key::new(PROP_INDEX).u32(k).bytes(&v).u64(id), &[])?;
        }
        self.tree.put(&Key::new(INDEXED).u32(k), &[])?;
        self.indexed.get_or_insert_with(HashSet::new).insert(k);
        Ok(())
    }

    pub fn drop_index(&mut self, key: &str) -> Result<bool> {
        let Some(k) = self.symbol(Sym::PropKey, key)? else {
            return Ok(false);
        };
        if !self.is_indexed(k)? {
            return Ok(false);
        }
        for k in self.scan_keys(&Key::new(PROP_INDEX).u32(k))? {
            self.tree.delete(&k)?;
        }
        self.tree.delete(&Key::new(INDEXED).u32(k))?;
        self.indexed.as_mut().unwrap().remove(&k);
        Ok(true)
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
        self.require_node(src)?;
        self.require_node(dst)?;
        let t = self.intern(Sym::EdgeType, edge_type)?;
        let out = Key::new(OUT).u64(src).u32(t).u64(dst);
        if self.tree.contains(&out)? {
            return Ok(false);
        }
        self.tree.put(&out, &[])?;
        self.tree.put(&Key::new(IN).u64(dst).u32(t).u64(src), &[])?;
        self.bump(EDGE_COUNT, 1)?;
        Ok(true)
    }

    /// Removes the edge and its properties. Returns whether it existed.
    pub fn remove_edge(&mut self, src: NodeId, edge_type: &str, dst: NodeId) -> Result<bool> {
        match self.symbol(Sym::EdgeType, edge_type)? {
            Some(t) => self.remove_edge_ids(src, t, dst),
            None => Ok(false),
        }
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
        if dir != Direction::In {
            n += self.scan_keys(&Key::new(OUT).u64(id).u32(t))?.len() as u64;
        }
        if dir != Direction::Out {
            n += self.scan_keys(&Key::new(IN).u64(id).u32(t))?.len() as u64;
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
        let t = self.symbol(Sym::EdgeType, edge_type)?.ok_or(Error::EdgeNotFound)?;
        if !self.tree.contains(&Key::new(OUT).u64(src).u32(t).u64(dst))? {
            return Err(Error::EdgeNotFound);
        }
        let k = self.intern(Sym::PropKey, key)?;
        self.tree.put(&Key::new(EDGE_PROP).u64(src).u32(t).u64(dst).u32(k), &value.into().encode())
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
        let (Some(t), Some(k)) = (self.symbol(Sym::EdgeType, edge_type)?, self.symbol(Sym::PropKey, key)?) else {
            return Ok(None);
        };
        let pk = Key::new(EDGE_PROP).u64(src).u32(t).u64(dst).u32(k);
        let old = self.tree.get(&pk)?;
        if old.is_some() {
            self.tree.delete(&pk)?;
        }
        old.map(|v| Value::decode_all(&v)).transpose()
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
        let mut seen = HashSet::from([start]);
        let mut out = vec![(start, 0)];
        let mut i = 0;
        while i < out.len() {
            let (node, depth) = out[i];
            i += 1;
            if depth == max_depth {
                continue;
            }
            for next in self.step(node, t, dir)? {
                if seen.insert(next) {
                    out.push((next, depth + 1));
                }
            }
        }
        Ok(out)
    }

    /// A shortest path from `from` to `to` over edges of `edge_type` (any
    /// type if `None`), as the list of nodes along it, or `None` if `to` is
    /// unreachable.
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
        let mut parent = HashMap::from([(from, from)]);
        let mut queue = VecDeque::from([from]);
        while let Some(node) = queue.pop_front() {
            for next in self.step(node, t, dir)? {
                if parent.contains_key(&next) {
                    continue;
                }
                parent.insert(next, node);
                if next == to {
                    let mut path = vec![to];
                    let mut cur = to;
                    while cur != from {
                        cur = parent[&cur];
                        path.push(cur);
                    }
                    path.reverse();
                    return Ok(Some(path));
                }
                queue.push_back(next);
            }
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
        for (tag, wanted) in [(OUT, dir != Direction::In), (IN, dir != Direction::Out)] {
            if !wanted {
                continue;
            }
            let prefix = match t {
                Some(t) => Key::new(tag).u64(id).u32(t),
                None => Key::new(tag).u64(id),
            };
            for k in self.scan_keys(&prefix)? {
                out.push(be_u64(&k, 13)?);
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    // ---- internals ----

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

    fn counter(&mut self, name: &[u8]) -> Result<u64> {
        match self.tree.get(&Key::new(META).bytes(name))? {
            Some(v) => Ok(u64::from_be_bytes(v.try_into().map_err(|_| Error::Corrupt("bad counter"))?)),
            None => Ok(0),
        }
    }

    fn set_counter(&mut self, name: &[u8], v: u64) -> Result<()> {
        self.tree.put(&Key::new(META).bytes(name), &v.to_be_bytes())
    }

    fn bump(&mut self, name: &[u8], delta: i64) -> Result<()> {
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

fn lock(file: &File) -> Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(Error::Locked),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}
