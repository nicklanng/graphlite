//! B+tree over pager pages, with variable-length byte keys and values.
//!
//! Slotted page layout (integers little-endian):
//!
//! ```text
//! 0    kind                 u8   (1 = leaf, 2 = internal)
//! 2    cell count           u16
//! 4    cell content start   u16  (cells grow down from the page end)
//! 8    link                 u64  (leaf: right sibling; internal: rightmost child)
//! 16   slot array           u16 per cell: cell offsets, in key order
//!
//! leaf cell:      klen u16 | vlen u16 | key | value
//! internal cell:  klen u16 | child u64 | key        (child holds keys < key)
//! ```
//!
//! Keys compare bytewise, so callers encode composite keys big-endian.
//! The root page id never changes: a root split moves the old root's
//! contents to a fresh page.
//!
//! Values too large to fit inline (keys + values beyond about a quarter page)
//! go to a chain of overflow pages; the leaf cell then stores vlen = 0xFFFF
//! and a 16-byte stub: total length u64 | first page u64. Each overflow page
//! is: next page u64 | data.
//!
//! TODO: deletes don't rebalance (underfull and empty leaves are tolerated
//! and skipped by cursors); keys are capped at about a quarter page; no
//! prefix compression yet.

use std::cmp::Ordering;

use crate::error::{Error, Result};
use crate::pager::{NULL_PAGE, PageId, PageRef, Pager};

const KIND_LEAF: u8 = 1;
const KIND_INTERNAL: u8 = 2;

const HEADER_SIZE: usize = 16;
const SLOT_SIZE: usize = 2;
const LEAF_OVERHEAD: usize = 4;
const INTERNAL_OVERHEAD: usize = 10;
const OVERFLOW_MARK: usize = 0xFFFF;
const STUB_SIZE: usize = 16;
/// Deeper than any real tree: every internal node has at least two children.
/// A descent this long means a cycle in a damaged file.
const MAX_HEIGHT: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Leaf,
    Internal,
}

/// Read-only view of a node page.
#[derive(Clone, Copy)]
struct Node<'a>(&'a [u8]);

impl<'a> Node<'a> {
    fn kind(self) -> Result<Kind> {
        match self.0[0] {
            KIND_LEAF => Ok(Kind::Leaf),
            KIND_INTERNAL => Ok(Kind::Internal),
            _ => Err(Error::Corrupt("unknown node kind")),
        }
    }

    fn is_leaf(self) -> bool {
        self.0[0] == KIND_LEAF
    }

    fn u16_at(self, off: usize) -> usize {
        u16::from_le_bytes([self.0[off], self.0[off + 1]]) as usize
    }

    fn u64_at(self, off: usize) -> u64 {
        u64::from_le_bytes(self.0[off..off + 8].try_into().unwrap())
    }

    fn count(self) -> usize {
        self.u16_at(2)
    }

    fn content_start(self) -> usize {
        self.u16_at(4)
    }

    fn link(self) -> PageId {
        self.u64_at(8)
    }

    fn slot(self, i: usize) -> usize {
        self.u16_at(HEADER_SIZE + i * SLOT_SIZE)
    }

    fn key(self, i: usize) -> &'a [u8] {
        let off = self.slot(i);
        let klen = self.u16_at(off);
        let start = off + if self.is_leaf() { LEAF_OVERHEAD } else { INTERNAL_OVERHEAD };
        &self.0[start..start + klen]
    }

    /// The value bytes as stored in the cell: the value, or an overflow stub.
    fn stored(self, i: usize) -> &'a [u8] {
        debug_assert!(self.is_leaf());
        let off = self.slot(i);
        let (klen, vlen) = (self.u16_at(off), self.u16_at(off + 2));
        let len = if vlen == OVERFLOW_MARK { STUB_SIZE } else { vlen };
        let start = off + LEAF_OVERHEAD + klen;
        &self.0[start..start + len]
    }

    fn is_overflow(self, i: usize) -> bool {
        self.u16_at(self.slot(i) + 2) == OVERFLOW_MARK
    }

    /// Total length and first page of an overflowed value.
    fn overflow(self, i: usize) -> Option<(usize, PageId)> {
        if !self.is_overflow(i) {
            return None;
        }
        let stub = self.stored(i);
        let len = u64::from_le_bytes(stub[0..8].try_into().unwrap()) as usize;
        Some((len, u64::from_le_bytes(stub[8..16].try_into().unwrap())))
    }

    /// Child pointer `i`; `i == count` is the rightmost child.
    fn child(self, i: usize) -> PageId {
        debug_assert!(!self.is_leaf());
        if i == self.count() { self.link() } else { self.u64_at(self.slot(i) + 2) }
    }

    fn cell_size(self, i: usize) -> usize {
        let off = self.slot(i);
        let klen = self.u16_at(off);
        if self.is_leaf() {
            LEAF_OVERHEAD + klen + self.stored(i).len()
        } else {
            INTERNAL_OVERHEAD + klen
        }
    }

    /// Unallocated bytes between the slot array and the cell content.
    fn gap(self) -> usize {
        self.content_start() - (HEADER_SIZE + self.count() * SLOT_SIZE)
    }

    /// Bytes reclaimable by compaction, including the gap.
    fn free_space(self) -> usize {
        let used: usize = (0..self.count()).map(|i| self.cell_size(i) + SLOT_SIZE).sum();
        self.0.len() - HEADER_SIZE - used
    }

    /// Index of the first key >= `k`, and whether it equals `k`.
    fn lower_bound(self, k: &[u8]) -> (usize, bool) {
        let (mut lo, mut hi) = (0, self.count());
        let mut found = false;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match compare(self.key(mid), k) {
                Ordering::Less => lo = mid + 1,
                Ordering::Equal => {
                    // Keys are unique: this is the lower bound.
                    hi = mid;
                    lo = mid;
                    found = true;
                }
                Ordering::Greater => hi = mid,
            }
        }
        (lo, found)
    }

    /// Index of the child subtree that may contain `k`: the first i with k < key(i).
    fn child_index(self, k: &[u8]) -> usize {
        let (mut lo, mut hi) = (0, self.count());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if compare(k, self.key(mid)) == Ordering::Less { hi = mid } else { lo = mid + 1 }
        }
        lo
    }
}

/// Bytewise key order. Compares 8 bytes at a time, which beats a `memcmp`
/// call for the short keys that dominate (edge keys are 21 bytes).
#[inline]
fn compare(a: &[u8], b: &[u8]) -> Ordering {
    let n = a.len().min(b.len());
    let (wa, _) = a[..n].as_chunks::<8>();
    let (wb, _) = b[..n].as_chunks::<8>();
    for (x, y) in wa.iter().zip(wb) {
        let (x, y) = (u64::from_be_bytes(*x), u64::from_be_bytes(*y));
        if x != y {
            return x.cmp(&y);
        }
    }
    let done = wa.len() * 8;
    for (x, y) in a[done..n].iter().zip(&b[done..n]) {
        if x != y {
            return x.cmp(y);
        }
    }
    a.len().cmp(&b.len())
}

/// Checks a node page loaded from disk, so the accessors above can't index
/// out of bounds and splits can't overflow a page, even on a damaged file
/// whose checksums are intact. Cells must fit in the page, each no larger
/// than `max_cell`.
fn check_node(page: &[u8], max_cell: usize) -> Result<()> {
    let n = Node(page);
    let (overhead, vlen_mask) = match n.kind()? {
        Kind::Leaf => (LEAF_OVERHEAD, 0xFFFF),
        Kind::Internal => (INTERNAL_OVERHEAD, 0),
    };
    let start = n.content_start();
    let slots_end = HEADER_SIZE + n.count() * SLOT_SIZE;
    if slots_end > start || start > page.len() {
        return Err(Error::Corrupt("malformed tree page"));
    }
    // One pass without early exits: this runs on every cache miss.
    let last = page.len() - LEAF_OVERHEAD;
    let mut ok = true;
    let mut used = slots_end;
    for s in page[HEADER_SIZE..slots_end].chunks_exact(SLOT_SIZE) {
        let off = usize::from(u16::from_le_bytes([s[0], s[1]]));
        // Clamped so the reads stay in bounds; `ok` records the real verdict.
        let at = off.min(last);
        let klen = usize::from(u16::from_le_bytes([page[at], page[at + 1]]));
        let vlen = match usize::from(u16::from_le_bytes([page[at + 2], page[at + 3]])) & vlen_mask {
            OVERFLOW_MARK => STUB_SIZE,
            v => v,
        };
        let size = overhead + klen + vlen;
        ok &= off >= start && size <= max_cell && off + size <= page.len();
        used += size;
    }
    if !ok || used > page.len() {
        return Err(Error::Corrupt("malformed tree page"));
    }
    Ok(())
}

/// Pins node page `id`, checking its structure the first time it's loaded.
fn get_node(pager: &mut Pager, id: PageId, max_cell: usize) -> Result<PageRef> {
    let r = pager.get(id)?;
    if !pager.is_checked(r) {
        if let Err(e) = check_node(pager.page(r), max_cell) {
            pager.release(r);
            return Err(e);
        }
        pager.set_checked(r);
    }
    Ok(r)
}

/// Mutable view of a node page.
struct NodeMut<'a>(&'a mut [u8]);

impl<'a> NodeMut<'a> {
    fn init(buf: &'a mut [u8], kind: Kind) -> NodeMut<'a> {
        buf[..HEADER_SIZE].fill(0);
        buf[0] = match kind {
            Kind::Leaf => KIND_LEAF,
            Kind::Internal => KIND_INTERNAL,
        };
        let len = buf.len();
        let mut n = NodeMut(buf);
        n.set_u16(4, len);
        n
    }

    fn r(&self) -> Node<'_> {
        Node(self.0)
    }

    fn set_u16(&mut self, off: usize, v: usize) {
        self.0[off..off + 2].copy_from_slice(&(v as u16).to_le_bytes());
    }

    fn set_count(&mut self, c: usize) {
        self.set_u16(2, c);
    }

    fn set_link(&mut self, id: PageId) {
        self.0[8..16].copy_from_slice(&id.to_le_bytes());
    }

    fn set_slot(&mut self, i: usize, off: usize) {
        self.set_u16(HEADER_SIZE + i * SLOT_SIZE, off);
    }

    fn set_child(&mut self, i: usize, id: PageId) {
        if i == self.r().count() {
            self.set_link(id);
        } else {
            let off = self.r().slot(i) + 2;
            self.0[off..off + 8].copy_from_slice(&id.to_le_bytes());
        }
    }

    /// Ensures a cell of `size` bytes fits, compacting if that is enough.
    fn make_room(&mut self, size: usize, scratch: &mut [u8]) -> bool {
        if self.r().gap() >= size + SLOT_SIZE {
            return true;
        }
        if self.r().free_space() >= size + SLOT_SIZE {
            self.compact(scratch);
            return true;
        }
        false
    }

    /// Reserves `size` bytes for a new cell at slot `i`, returning its offset.
    fn reserve(&mut self, i: usize, size: usize) -> usize {
        let n = self.r();
        let count = n.count();
        assert!(n.gap() >= size + SLOT_SIZE, "reserve without room");
        let off = n.content_start() - size;
        let slots = HEADER_SIZE + i * SLOT_SIZE..HEADER_SIZE + count * SLOT_SIZE;
        self.0.copy_within(slots.clone(), slots.start + SLOT_SIZE);
        self.set_slot(i, off);
        self.set_count(count + 1);
        self.set_u16(4, off);
        off
    }

    /// Writes a leaf cell; with `overflow`, `value` is a 16-byte stub.
    fn put_leaf_cell(&mut self, i: usize, key: &[u8], value: &[u8], overflow: bool) {
        debug_assert!(!overflow || value.len() == STUB_SIZE);
        let off = self.reserve(i, LEAF_OVERHEAD + key.len() + value.len());
        self.set_u16(off, key.len());
        self.set_u16(off + 2, if overflow { OVERFLOW_MARK } else { value.len() });
        let k = off + LEAF_OVERHEAD;
        self.0[k..k + key.len()].copy_from_slice(key);
        self.0[k + key.len()..k + key.len() + value.len()].copy_from_slice(value);
    }

    fn put_internal_cell(&mut self, i: usize, key: &[u8], child: PageId) {
        let off = self.reserve(i, INTERNAL_OVERHEAD + key.len());
        self.set_u16(off, key.len());
        self.0[off + 2..off + 10].copy_from_slice(&child.to_le_bytes());
        self.0[off + INTERNAL_OVERHEAD..off + INTERNAL_OVERHEAD + key.len()].copy_from_slice(key);
    }

    /// Drops cell `i`. Its bytes stay allocated until the next compaction.
    fn remove(&mut self, i: usize) {
        let count = self.r().count();
        self.0.copy_within(
            HEADER_SIZE + (i + 1) * SLOT_SIZE..HEADER_SIZE + count * SLOT_SIZE,
            HEADER_SIZE + i * SLOT_SIZE,
        );
        self.set_count(count - 1);
    }

    /// Rewrites cells contiguously at the page end, reclaiming removed cells.
    fn compact(&mut self, scratch: &mut [u8]) {
        let len = self.0.len();
        scratch[..len].copy_from_slice(self.0);
        let old = Node(&scratch[..len]);
        let mut end = len;
        for i in 0..old.count() {
            let size = old.cell_size(i);
            let src = old.slot(i);
            end -= size;
            self.0[end..end + size].copy_from_slice(&old.0[src..src + size]);
            self.set_slot(i, end);
        }
        self.set_u16(4, end);
    }
}

/// A cell staged during a split.
struct Cell<'a> {
    key: &'a [u8],
    value: &'a [u8],
    overflow: bool,
    child: PageId,
}

/// Result of splitting a node: everything >= `key` moved to page `right`.
struct Split {
    key: Vec<u8>,
    right: PageId,
}

/// Picks the split index so both halves hold about the same number of bytes.
/// Leaves keep `cells[..m]` and move `cells[m..]`; internal nodes push
/// `cells[m]` up to the parent.
fn split_point(cells: &[Cell], kind: Kind) -> usize {
    let size = |c: &Cell| match kind {
        Kind::Leaf => LEAF_OVERHEAD + c.key.len() + c.value.len() + SLOT_SIZE,
        Kind::Internal => INTERNAL_OVERHEAD + c.key.len() + SLOT_SIZE,
    };
    let total: usize = cells.iter().map(size).sum();
    let mut acc = 0;
    let mut m = cells.len() / 2;
    for (i, c) in cells.iter().enumerate() {
        acc += size(c);
        if acc * 2 >= total {
            m = if kind == Kind::Leaf { i + 1 } else { i };
            break;
        }
    }
    match kind {
        Kind::Leaf => m.clamp(1, cells.len() - 1),
        Kind::Internal => m.clamp(1, cells.len() - 2),
    }
}

pub struct BTree {
    pager: Pager,
    root: PageId,
    /// One page of scratch space for compaction and splits.
    scratch: Vec<u8>,
    /// Largest cell (header + key + value) a page may hold; at least four
    /// fit per page, which keeps splits balanced.
    max_cell: usize,
    /// Overflow chain of a value replaced during an insert, freed afterwards.
    orphan: Option<PageId>,
    /// Whether the last insert found its key already present.
    existed: bool,
    /// Counts calls that may have modified the tree, successful or not.
    writes: u64,
}

/// One insert, threaded down the tree.
struct Insert<'a> {
    key: &'a [u8],
    /// The value, or an overflow stub.
    value: &'a [u8],
    overflow: bool,
    /// Replace an existing value; otherwise leave it untouched.
    replace: bool,
}

impl BTree {
    /// Creates an empty tree, recording its root in the pager header.
    pub fn create(mut pager: Pager) -> Result<BTree> {
        let r = pager.allocate()?;
        NodeMut::init(pager.page_mut(r), Kind::Leaf);
        let root = pager.id(r);
        pager.release(r);
        pager.set_root(root);
        Ok(BTree::with_root(pager, root))
    }

    pub fn open(pager: Pager) -> Result<BTree> {
        let root = pager.root();
        if root == NULL_PAGE {
            return Err(Error::Corrupt("missing tree root"));
        }
        Ok(BTree::with_root(pager, root))
    }

    fn with_root(pager: Pager, root: PageId) -> BTree {
        let usable = pager.usable_size();
        BTree {
            scratch: vec![0u8; usable],
            max_cell: (usable - HEADER_SIZE) / 4 - SLOT_SIZE,
            orphan: None,
            existed: false,
            writes: 0,
            pager,
            root,
        }
    }

    pub fn pager(&self) -> &Pager {
        &self.pager
    }

    pub fn pager_mut(&mut self) -> &mut Pager {
        &mut self.pager
    }

    /// Increases on every put or delete that got past argument checks, so a
    /// caller can tell whether a failed operation may have changed the tree.
    pub fn writes(&self) -> u64 {
        self.writes
    }

    /// Largest `key.len() + value.len()` stored inline in a leaf.
    fn max_inline(&self) -> usize {
        self.max_cell - INTERNAL_OVERHEAD
    }

    /// Longest key accepted by [`BTree::put`].
    pub fn max_key_len(&self) -> usize {
        self.max_inline() - STUB_SIZE
    }

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let r = self.find_leaf(key)?;
        let n = Node(self.pager.page(r));
        let (i, found) = n.lower_bound(key);
        let out = if !found {
            Ok(None)
        } else if let Some((len, first)) = n.overflow(i) {
            let mut v = Vec::new();
            read_overflow(&mut self.pager, len, first, &mut v).map(|()| Some(v))
        } else {
            Ok(Some(n.stored(i).to_vec()))
        };
        self.pager.release(r);
        out
    }

    pub fn contains(&mut self, key: &[u8]) -> Result<bool> {
        let r = self.find_leaf(key)?;
        let found = Node(self.pager.page(r)).lower_bound(key).1;
        self.pager.release(r);
        Ok(found)
    }

    /// Inserts or replaces `key`. Values of any size are accepted; keys are
    /// limited to [`BTree::max_key_len`].
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.upsert(key, value, true).map(|_| ())
    }

    /// Inserts `key` unless it is already present, in a single descent.
    /// Returns whether it was inserted.
    pub fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> Result<bool> {
        self.upsert(key, value, false)
    }

    /// Returns whether `key` was absent before the call.
    fn upsert(&mut self, key: &[u8], value: &[u8], replace: bool) -> Result<bool> {
        if key.len() > self.max_key_len() {
            return Err(Error::TooLarge);
        }
        let inline = key.len() + value.len() <= self.max_inline();
        if !inline && !replace && self.contains(key)? {
            return Ok(false); // don't write an overflow chain we would discard
        }
        self.writes += 1;
        let stub;
        let (stored, overflow) = if inline {
            (value, false)
        } else {
            let first = self.write_overflow(value)?;
            stub = [(value.len() as u64).to_le_bytes(), first.to_le_bytes()].concat();
            (&stub[..], true)
        };
        self.existed = false;
        self.orphan = None;
        let split = self.insert(self.root, &Insert { key, value: stored, overflow, replace }, 0);
        // Taken even on error: a chain left here would be freed by a later
        // insert, possibly after a rollback made it live again.
        let orphan = self.orphan.take();
        let split = split?;
        if let Some(first) = orphan {
            self.free_overflow(first)?;
        }
        let absent = !self.existed;
        let Some(split) = split else {
            return Ok(absent);
        };
        // Keep the root id stable: move its contents to a new left child.
        let left = self.pager.allocate()?;
        let root = match get_node(&mut self.pager, self.root, self.max_cell) {
            Ok(r) => r,
            Err(e) => {
                self.pager.release(left);
                return Err(e);
            }
        };
        self.scratch.copy_from_slice(self.pager.page(root));
        self.pager.page_mut(left).copy_from_slice(&self.scratch);
        let left_id = self.pager.id(left);
        let mut n = NodeMut::init(self.pager.page_mut(root), Kind::Internal);
        n.put_internal_cell(0, &split.key, left_id);
        n.set_link(split.right);
        self.pager.release(root);
        self.pager.release(left);
        Ok(absent)
    }

    /// Removes `key`, returning whether it was present.
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> {
        self.writes += 1;
        let r = self.find_leaf(key)?;
        let n = Node(self.pager.page(r));
        let (i, found) = n.lower_bound(key);
        let mut chain = None;
        if found {
            chain = n.overflow(i).map(|(_, first)| first);
            NodeMut(self.pager.page_mut(r)).remove(i);
        }
        self.pager.release(r);
        if let Some(first) = chain {
            self.free_overflow(first)?;
        }
        Ok(found)
    }

    /// Returns a cursor at the first key >= `key`. The tree can't be modified
    /// while the cursor is alive.
    pub fn seek(&mut self, key: &[u8]) -> Result<Cursor<'_>> {
        let r = self.find_leaf(key)?;
        let (idx, _) = Node(self.pager.page(r)).lower_bound(key);
        Ok(Cursor { pager: &mut self.pager, page: Some(r), idx, buf: Vec::new(), max_cell: self.max_cell, hops: 0 })
    }

    /// Writes `value` to a new overflow chain, returning its first page.
    fn write_overflow(&mut self, value: &[u8]) -> Result<PageId> {
        let cap = self.pager.usable_size() - 8;
        let mut next = NULL_PAGE;
        // Back to front, so each page can link to the one written before it.
        for chunk in value.chunks(cap).rev() {
            let r = self.pager.allocate()?;
            let page = self.pager.page_mut(r);
            page[0..8].copy_from_slice(&next.to_le_bytes());
            page[8..8 + chunk.len()].copy_from_slice(chunk);
            next = self.pager.id(r);
            self.pager.release(r);
        }
        Ok(next)
    }

    fn free_overflow(&mut self, mut id: PageId) -> Result<()> {
        let mut left = self.pager.page_count();
        while id != NULL_PAGE {
            if left == 0 {
                return Err(Error::Corrupt("overflow chain cycle"));
            }
            left -= 1;
            let r = self.pager.get(id)?;
            let next = u64::from_le_bytes(self.pager.page(r)[0..8].try_into().unwrap());
            self.pager.release(r);
            self.pager.free(id)?;
            id = next;
        }
        Ok(())
    }

    /// Descends to the leaf that may contain `key` and returns it pinned.
    fn find_leaf(&mut self, key: &[u8]) -> Result<PageRef> {
        let mut id = self.root;
        for _ in 0..MAX_HEIGHT {
            let r = get_node(&mut self.pager, id, self.max_cell)?;
            let n = Node(self.pager.page(r));
            match n.kind() {
                Ok(Kind::Leaf) => return Ok(r),
                Ok(Kind::Internal) => {
                    id = n.child(n.child_index(key));
                    self.pager.release(r);
                }
                Err(e) => {
                    self.pager.release(r);
                    return Err(e);
                }
            }
        }
        Err(Error::Corrupt("tree too deep"))
    }

    fn insert(&mut self, id: PageId, ins: &Insert, depth: usize) -> Result<Option<Split>> {
        if depth == MAX_HEIGHT {
            return Err(Error::Corrupt("tree too deep"));
        }
        let r = get_node(&mut self.pager, id, self.max_cell)?;
        let n = Node(self.pager.page(r));
        match n.kind() {
            Ok(Kind::Leaf) => {
                let res = self.insert_into_leaf(r, ins);
                self.pager.release(r);
                res
            }
            Ok(Kind::Internal) => {
                let ci = n.child_index(ins.key);
                let child = n.child(ci);
                self.pager.release(r);
                let Some(split) = self.insert(child, ins, depth + 1)? else {
                    return Ok(None);
                };
                let r = get_node(&mut self.pager, id, self.max_cell)?;
                let res = self.insert_into_internal(r, ci, child, split);
                self.pager.release(r);
                res
            }
            Err(e) => {
                self.pager.release(r);
                Err(e)
            }
        }
    }

    fn insert_into_leaf(&mut self, r: PageRef, ins: &Insert) -> Result<Option<Split>> {
        let Insert { key, value, overflow, .. } = *ins;
        let n = Node(self.pager.page(r));
        let (pos, found) = n.lower_bound(key);
        if found {
            self.existed = true;
            if !ins.replace {
                return Ok(None);
            }
            self.orphan = n.overflow(pos).map(|(_, first)| first);
        }
        let mut n = NodeMut(self.pager.page_mut(r));
        if found {
            n.remove(pos);
        }
        if n.make_room(LEAF_OVERHEAD + key.len() + value.len(), &mut self.scratch) {
            n.put_leaf_cell(pos, key, value, overflow);
            return Ok(None);
        }

        self.scratch.copy_from_slice(self.pager.page(r));
        let old = Node(&self.scratch);
        let mut cells: Vec<Cell> = (0..old.count())
            .map(|i| Cell { key: old.key(i), value: old.stored(i), overflow: old.is_overflow(i), child: NULL_PAGE })
            .collect();
        cells.insert(pos, Cell { key, value, overflow, child: NULL_PAGE });
        let m = split_point(&cells, Kind::Leaf);
        let sep = cells[m].key.to_vec();

        let right = self.pager.allocate()?;
        let right_id = self.pager.id(right);
        let mut rn = NodeMut::init(self.pager.page_mut(right), Kind::Leaf);
        rn.set_link(old.link());
        for (i, c) in cells[m..].iter().enumerate() {
            rn.put_leaf_cell(i, c.key, c.value, c.overflow);
        }
        self.pager.release(right);

        let mut ln = NodeMut::init(self.pager.page_mut(r), Kind::Leaf);
        ln.set_link(right_id);
        for (i, c) in cells[..m].iter().enumerate() {
            ln.put_leaf_cell(i, c.key, c.value, c.overflow);
        }
        Ok(Some(Split { key: sep, right: right_id }))
    }

    /// Records that child `ci` (page `left`) split, with keys >= `split.key`
    /// now in `split.right`.
    fn insert_into_internal(&mut self, r: PageRef, ci: usize, left: PageId, split: Split) -> Result<Option<Split>> {
        let mut n = NodeMut(self.pager.page_mut(r));
        if n.make_room(INTERNAL_OVERHEAD + split.key.len(), &mut self.scratch) {
            n.put_internal_cell(ci, &split.key, left);
            n.set_child(ci + 1, split.right);
            return Ok(None);
        }

        self.scratch.copy_from_slice(self.pager.page(r));
        let old = Node(&self.scratch);
        let mut cells: Vec<Cell> =
            (0..old.count()).map(|i| Cell { key: old.key(i), value: &[], overflow: false, child: old.child(i) }).collect();
        let mut rightmost = old.link();
        cells.insert(ci, Cell { key: &split.key, value: &[], overflow: false, child: left });
        // The pointer that used to lead to `left` now leads to `split.right`.
        match cells.get_mut(ci + 1) {
            Some(c) => c.child = split.right,
            None => rightmost = split.right,
        }
        let m = split_point(&cells, Kind::Internal);
        let up = cells[m].key.to_vec();

        let right = self.pager.allocate()?;
        let right_id = self.pager.id(right);
        let mut rn = NodeMut::init(self.pager.page_mut(right), Kind::Internal);
        rn.set_link(rightmost);
        for (i, c) in cells[m + 1..].iter().enumerate() {
            rn.put_internal_cell(i, c.key, c.child);
        }
        self.pager.release(right);

        let mut ln = NodeMut::init(self.pager.page_mut(r), Kind::Internal);
        ln.set_link(cells[m].child);
        for (i, c) in cells[..m].iter().enumerate() {
            ln.put_internal_cell(i, c.key, c.child);
        }
        Ok(Some(Split { key: up, right: right_id }))
    }
}

/// Forward iterator over a tree in key order, holding a pin on the current leaf.
pub struct Cursor<'t> {
    pager: &'t mut Pager,
    page: Option<PageRef>,
    idx: usize,
    /// Holds the current value when it lives in overflow pages.
    buf: Vec<u8>,
    max_cell: usize,
    /// Leaves stepped to; more than the file has pages means a sibling cycle.
    hops: u64,
}

impl Cursor<'_> {
    /// Returns the next entry. The slices borrow the page cache and live
    /// until the next call.
    #[allow(clippy::should_implement_trait)] // lending: items borrow the cursor
    pub fn next(&mut self) -> Result<Option<(&[u8], &[u8])>> {
        while let Some(r) = self.page {
            let count = Node(self.pager.page(r)).count();
            if self.idx < count {
                let i = self.idx;
                self.idx += 1;
                if let Some((len, first)) = Node(self.pager.page(r)).overflow(i) {
                    read_overflow(self.pager, len, first, &mut self.buf)?;
                    return Ok(Some((Node(self.pager.page(r)).key(i), &self.buf)));
                }
                let n = Node(self.pager.page(r));
                return Ok(Some((n.key(i), n.stored(i))));
            }
            let sibling = Node(self.pager.page(r)).link();
            self.pager.release(r);
            self.page = None;
            self.idx = 0;
            if sibling != NULL_PAGE {
                self.hops += 1;
                if self.hops > self.pager.page_count() {
                    return Err(Error::Corrupt("leaf sibling cycle"));
                }
                let r = get_node(self.pager, sibling, self.max_cell)?;
                self.page = Some(r);
                if !Node(self.pager.page(r)).is_leaf() {
                    return Err(Error::Corrupt("leaf sibling is not a leaf"));
                }
            }
        }
        Ok(None)
    }
}

/// Reads an overflowed value of `len` bytes starting at page `id` into `out`.
fn read_overflow(pager: &mut Pager, len: usize, mut id: PageId, out: &mut Vec<u8>) -> Result<()> {
    let cap = (pager.usable_size() - 8) as u64;
    if len as u64 > pager.page_count().saturating_mul(cap) {
        return Err(Error::Corrupt("overflow length exceeds file"));
    }
    out.clear();
    out.reserve(len);
    while out.len() < len {
        if id == NULL_PAGE {
            return Err(Error::Corrupt("overflow chain too short"));
        }
        let r = pager.get(id)?;
        let page = pager.page(r);
        let take = (len - out.len()).min(page.len() - 8);
        out.extend_from_slice(&page[8..8 + take]);
        id = u64::from_le_bytes(page[0..8].try_into().unwrap());
        pager.release(r);
    }
    Ok(())
}

impl Drop for Cursor<'_> {
    fn drop(&mut self) {
        if let Some(r) = self.page.take() {
            self.pager.release(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pager::Options;
    use crate::testutil::{Rng, TempDir};
    use std::collections::BTreeMap;
    use std::fs::OpenOptions;
    use std::path::Path;

    fn open(dir: &Path, create: bool) -> BTree {
        let file = |name: &str| {
            OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(name)).unwrap()
        };
        let opts = Options { page_size: 512, cache_pages: 16, wal_autocheckpoint: 64 };
        if create {
            BTree::create(Pager::create(file("t.db"), file("t.db-wal"), &opts).unwrap()).unwrap()
        } else {
            BTree::open(Pager::open(file("t.db"), file("t.db-wal"), &opts).unwrap()).unwrap()
        }
    }

    fn scan_all(t: &mut BTree) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut c = t.seek(&[]).unwrap();
        let mut out = Vec::new();
        while let Some((k, v)) = c.next().unwrap() {
            out.push((k.to_vec(), v.to_vec()));
        }
        out
    }

    fn random_bytes(rng: &mut Rng, max_len: usize) -> Vec<u8> {
        let len = rng.below(max_len as u64 + 1) as usize;
        // Small alphabet so random keys collide and exercise replacement.
        (0..len).map(|_| b'a' + rng.below(4) as u8).collect()
    }

    #[test]
    fn matches_reference_model() {
        let dir = TempDir::new();
        let mut t = open(dir.path(), true);
        let mut model = BTreeMap::new();
        let mut rng = Rng::new(7);

        for step in 0..20_000 {
            let key = random_bytes(&mut rng, 12);
            match rng.below(10) {
                0..=6 => {
                    // Mostly small values, some spilling into overflow pages.
                    let max = if rng.below(20) == 0 { 2_000 } else { 40 };
                    let value = random_bytes(&mut rng, max);
                    t.put(&key, &value).unwrap();
                    model.insert(key, value);
                }
                7..=8 => assert_eq!(t.delete(&key).unwrap(), model.remove(&key).is_some()),
                _ => assert_eq!(t.get(&key).unwrap(), model.get(&key).cloned()),
            }
            if step % 1_000 == 0 {
                t.pager_mut().commit().unwrap();
            }
            if step % 5_000 == 0 {
                let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                assert_eq!(scan_all(&mut t), expected);
            }
        }

        t.pager_mut().commit().unwrap();
        drop(t);
        let mut t = open(dir.path(), false);
        let expected: Vec<_> = model.into_iter().collect();
        assert_eq!(scan_all(&mut t), expected);
    }

    #[test]
    fn compare_matches_slice_order() {
        let mut rng = Rng::new(3);
        for _ in 0..20_000 {
            let a = random_bytes(&mut rng, 24);
            let mut b = a.clone();
            match rng.below(3) {
                0 => b = random_bytes(&mut rng, 24),
                1 if !b.is_empty() => {
                    let i = rng.below(b.len() as u64) as usize;
                    b[i] = b[i].wrapping_add(rng.below(3) as u8);
                }
                _ => b.truncate(rng.below(b.len() as u64 + 1) as usize),
            }
            assert_eq!(compare(&a, &b), a.cmp(&b), "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn put_if_absent_keeps_existing_values() {
        let dir = TempDir::new();
        let mut t = open(dir.path(), true);
        for i in 0u32..2_000 {
            assert!(t.put_if_absent(&i.to_be_bytes(), b"first").unwrap());
        }
        for i in 0u32..2_000 {
            assert!(!t.put_if_absent(&i.to_be_bytes(), b"second").unwrap());
        }
        assert!(!t.put_if_absent(&7u32.to_be_bytes(), &vec![0u8; 10_000]).unwrap());
        let pages = t.pager().page_count();
        assert!(scan_all(&mut t).iter().all(|(_, v)| v == b"first"));
        assert!(t.put_if_absent(b"big", &vec![9u8; 10_000]).unwrap());
        assert_eq!(t.get(b"big").unwrap().unwrap(), vec![9u8; 10_000]);
        assert!(t.pager().page_count() > pages);
    }

    #[test]
    fn sequential_inserts_and_seek() {
        let dir = TempDir::new();
        let mut t = open(dir.path(), true);
        for i in 0u32..10_000 {
            t.put(&i.to_be_bytes(), &(i * 2).to_le_bytes()).unwrap();
        }
        let mut c = t.seek(&5_000u32.to_be_bytes()).unwrap();
        for i in 5_000u32..10_000 {
            let (k, v) = c.next().unwrap().unwrap();
            assert_eq!(k, i.to_be_bytes());
            assert_eq!(v, (i * 2).to_le_bytes());
        }
        assert!(c.next().unwrap().is_none());
    }

    #[test]
    fn oversized_keys_are_rejected() {
        let dir = TempDir::new();
        let mut t = open(dir.path(), true);
        let max = t.max_key_len();
        assert!(matches!(t.put(&vec![2u8; max + 1], &[]), Err(Error::TooLarge)));
        // Max-size keys with large values still split cleanly.
        for i in 0..50u8 {
            let mut k = vec![i; max - 1];
            k.push(0);
            t.put(&k, &vec![i; 1_000]).unwrap();
        }
        let all = scan_all(&mut t);
        assert_eq!(all.len(), 50);
        assert!(all.iter().enumerate().all(|(i, (_, v))| *v == vec![i as u8; 1_000]));
    }

    /// Damage that checksums can't catch, written through the pager so the
    /// pages are sealed: it must surface as `Corrupt`, not a panic or a hang.
    #[test]
    fn malformed_pages_are_corruption() {
        let fill = |t: &mut BTree| {
            for i in 0u32..2_000 {
                t.put(&i.to_be_bytes(), b"value").unwrap();
            }
        };
        let key = 0u32.to_be_bytes();

        // A leaf slot pointing past the end of the page.
        let dir = TempDir::new();
        let mut t = open(dir.path(), true);
        fill(&mut t);
        let r = t.find_leaf(&key).unwrap();
        NodeMut(t.pager.page_mut(r)).set_slot(0, 0xFFF0);
        t.pager.release(r);
        t.pager_mut().commit().unwrap();
        drop(t);
        let mut t = open(dir.path(), false);
        assert!(matches!(t.get(&key), Err(Error::Corrupt(_))));
        assert!(matches!(t.seek(&[]), Err(Error::Corrupt(_))));

        // The root's rightmost child pointing back at the root.
        let dir = TempDir::new();
        let mut t = open(dir.path(), true);
        fill(&mut t);
        let r = t.pager.get(t.root).unwrap();
        assert!(!Node(t.pager.page(r)).is_leaf());
        NodeMut(t.pager.page_mut(r)).set_link(t.root);
        t.pager.release(r);
        t.pager_mut().commit().unwrap();
        drop(t);
        let mut t = open(dir.path(), false);
        let last = u32::MAX.to_be_bytes();
        assert!(matches!(t.get(&last), Err(Error::Corrupt(_))));
        assert!(matches!(t.put(&last, b"v"), Err(Error::Corrupt(_))));
    }

    #[test]
    fn overflow_values_roundtrip_and_free_their_pages() {
        let dir = TempDir::new();
        let mut t = open(dir.path(), true);
        let cap = t.pager().usable_size() - 8;
        for len in [cap - 1, cap, cap + 1, 3 * cap, 100_000] {
            let v: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            t.put(b"big", &v).unwrap();
            assert_eq!(t.get(b"big").unwrap().unwrap(), v);
        }
        t.put(b"big", &vec![1u8; 100_000]).unwrap();
        let pages = t.pager().page_count();
        for _ in 0..5 {
            t.put(b"big", &vec![2u8; 100_000]).unwrap();
        }
        assert_eq!(t.pager().page_count(), pages, "replaced chains should be reused");
        t.put(b"big", b"small").unwrap();
        assert_eq!(t.get(b"big").unwrap().unwrap(), b"small");
        assert!(t.delete(b"big").unwrap());
        t.put(b"again", &vec![3u8; 100_000]).unwrap();
        assert_eq!(t.pager().page_count(), pages);
    }
}
