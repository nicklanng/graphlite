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
//! TODO: deletes don't rebalance (underfull and empty leaves are tolerated
//! and skipped by cursors); no overflow pages, so a cell is capped at about a
//! quarter page; no prefix compression yet.

use std::cmp::Ordering;

use crate::error::{Error, Result};
use crate::pager::{NULL_PAGE, PageId, PageRef, Pager};

const KIND_LEAF: u8 = 1;
const KIND_INTERNAL: u8 = 2;

const HEADER_SIZE: usize = 16;
const SLOT_SIZE: usize = 2;
const LEAF_OVERHEAD: usize = 4;
const INTERNAL_OVERHEAD: usize = 10;

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

    fn value(self, i: usize) -> &'a [u8] {
        debug_assert!(self.is_leaf());
        let off = self.slot(i);
        let (klen, vlen) = (self.u16_at(off), self.u16_at(off + 2));
        let start = off + LEAF_OVERHEAD + klen;
        &self.0[start..start + vlen]
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
            LEAF_OVERHEAD + klen + self.u16_at(off + 2)
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
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.key(mid) < k { lo = mid + 1 } else { hi = mid }
        }
        (lo, lo < self.count() && self.key(lo) == k)
    }

    /// Index of the child subtree that may contain `k`: the first i with k < key(i).
    fn child_index(self, k: &[u8]) -> usize {
        let (mut lo, mut hi) = (0, self.count());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if k.cmp(self.key(mid)) == Ordering::Less { hi = mid } else { lo = mid + 1 }
        }
        lo
    }
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

    fn put_leaf_cell(&mut self, i: usize, key: &[u8], value: &[u8]) {
        let off = self.reserve(i, LEAF_OVERHEAD + key.len() + value.len());
        self.set_u16(off, key.len());
        self.set_u16(off + 2, value.len());
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

    /// Largest `key.len() + value.len()` accepted by [`BTree::put`].
    pub fn max_entry_len(&self) -> usize {
        self.max_cell - INTERNAL_OVERHEAD
    }

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let r = self.find_leaf(key)?;
        let n = Node(self.pager.page(r));
        let out = match n.lower_bound(key) {
            (i, true) => Some(n.value(i).to_vec()),
            _ => None,
        };
        self.pager.release(r);
        Ok(out)
    }

    pub fn contains(&mut self, key: &[u8]) -> Result<bool> {
        let r = self.find_leaf(key)?;
        let found = Node(self.pager.page(r)).lower_bound(key).1;
        self.pager.release(r);
        Ok(found)
    }

    /// Inserts or replaces `key`.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if key.len() + value.len() > self.max_entry_len() {
            return Err(Error::TooLarge);
        }
        let Some(split) = self.insert(self.root, key, value)? else {
            return Ok(());
        };
        // Keep the root id stable: move its contents to a new left child.
        let left = self.pager.allocate()?;
        let root = match self.pager.get(self.root) {
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
        Ok(())
    }

    /// Removes `key`, returning whether it was present.
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> {
        let r = self.find_leaf(key)?;
        let (i, found) = Node(self.pager.page(r)).lower_bound(key);
        if found {
            NodeMut(self.pager.page_mut(r)).remove(i);
        }
        self.pager.release(r);
        Ok(found)
    }

    /// Returns a cursor at the first key >= `key`. The tree can't be modified
    /// while the cursor is alive.
    pub fn seek(&mut self, key: &[u8]) -> Result<Cursor<'_>> {
        let r = self.find_leaf(key)?;
        let (idx, _) = Node(self.pager.page(r)).lower_bound(key);
        Ok(Cursor { pager: &mut self.pager, page: Some(r), idx })
    }

    /// Descends to the leaf that may contain `key` and returns it pinned.
    fn find_leaf(&mut self, key: &[u8]) -> Result<PageRef> {
        let mut id = self.root;
        loop {
            let r = self.pager.get(id)?;
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
    }

    fn insert(&mut self, id: PageId, key: &[u8], value: &[u8]) -> Result<Option<Split>> {
        let r = self.pager.get(id)?;
        let n = Node(self.pager.page(r));
        match n.kind() {
            Ok(Kind::Leaf) => {
                let res = self.insert_into_leaf(r, key, value);
                self.pager.release(r);
                res
            }
            Ok(Kind::Internal) => {
                let ci = n.child_index(key);
                let child = n.child(ci);
                self.pager.release(r);
                let Some(split) = self.insert(child, key, value)? else {
                    return Ok(None);
                };
                let r = self.pager.get(id)?;
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

    fn insert_into_leaf(&mut self, r: PageRef, key: &[u8], value: &[u8]) -> Result<Option<Split>> {
        let (pos, found) = Node(self.pager.page(r)).lower_bound(key);
        let mut n = NodeMut(self.pager.page_mut(r));
        if found {
            n.remove(pos);
        }
        if n.make_room(LEAF_OVERHEAD + key.len() + value.len(), &mut self.scratch) {
            n.put_leaf_cell(pos, key, value);
            return Ok(None);
        }

        self.scratch.copy_from_slice(self.pager.page(r));
        let old = Node(&self.scratch);
        let mut cells: Vec<Cell> =
            (0..old.count()).map(|i| Cell { key: old.key(i), value: old.value(i), child: NULL_PAGE }).collect();
        cells.insert(pos, Cell { key, value, child: NULL_PAGE });
        let m = split_point(&cells, Kind::Leaf);
        let sep = cells[m].key.to_vec();

        let right = self.pager.allocate()?;
        let right_id = self.pager.id(right);
        let mut rn = NodeMut::init(self.pager.page_mut(right), Kind::Leaf);
        rn.set_link(old.link());
        for (i, c) in cells[m..].iter().enumerate() {
            rn.put_leaf_cell(i, c.key, c.value);
        }
        self.pager.release(right);

        let mut ln = NodeMut::init(self.pager.page_mut(r), Kind::Leaf);
        ln.set_link(right_id);
        for (i, c) in cells[..m].iter().enumerate() {
            ln.put_leaf_cell(i, c.key, c.value);
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
            (0..old.count()).map(|i| Cell { key: old.key(i), value: &[], child: old.child(i) }).collect();
        let mut rightmost = old.link();
        cells.insert(ci, Cell { key: &split.key, value: &[], child: left });
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
                let n = Node(self.pager.page(r));
                return Ok(Some((n.key(i), n.value(i))));
            }
            let sibling = Node(self.pager.page(r)).link();
            self.pager.release(r);
            self.page = None;
            self.idx = 0;
            if sibling != NULL_PAGE {
                self.page = Some(self.pager.get(sibling)?);
            }
        }
        Ok(None)
    }
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

    fn open(path: &Path, create: bool) -> BTree {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).unwrap();
        let opts = Options { page_size: 512, cache_pages: 16 };
        if create {
            BTree::create(Pager::create(file, &opts).unwrap()).unwrap()
        } else {
            BTree::open(Pager::open(file, &opts).unwrap()).unwrap()
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
        let path = dir.path().join("t.db");
        let mut t = open(&path, true);
        let mut model = BTreeMap::new();
        let mut rng = Rng::new(7);

        for step in 0..20_000 {
            let key = random_bytes(&mut rng, 12);
            match rng.below(10) {
                0..=6 => {
                    let value = random_bytes(&mut rng, 40);
                    t.put(&key, &value).unwrap();
                    model.insert(key, value);
                }
                7..=8 => assert_eq!(t.delete(&key).unwrap(), model.remove(&key).is_some()),
                _ => assert_eq!(t.get(&key).unwrap(), model.get(&key).cloned()),
            }
            if step % 5_000 == 0 {
                let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                assert_eq!(scan_all(&mut t), expected);
            }
        }

        t.pager_mut().flush().unwrap();
        drop(t);
        let mut t = open(&path, false);
        let expected: Vec<_> = model.into_iter().collect();
        assert_eq!(scan_all(&mut t), expected);
    }

    #[test]
    fn sequential_inserts_and_seek() {
        let dir = TempDir::new();
        let mut t = open(&dir.path().join("t.db"), true);
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
    fn oversized_entries_are_rejected() {
        let dir = TempDir::new();
        let mut t = open(&dir.path().join("t.db"), true);
        let max = t.max_entry_len();
        t.put(&vec![1u8; max], &[]).unwrap();
        assert!(matches!(t.put(&vec![2u8; max + 1], &[]), Err(Error::TooLarge)));
        // Max-size keys still split cleanly.
        for i in 0..50u8 {
            let mut k = vec![i; max - 1];
            k.push(0);
            t.put(&k, &[]).unwrap();
        }
        assert_eq!(scan_all(&mut t).len(), 51);
    }
}
