//! Pager: maps page ids to fixed-size pages, cached in a bounded buffer pool
//! with clock eviction, with transactions on top of the write-ahead log.
//!
//! Page 0 holds the file header. Every page reserves its trailing 4 bytes for
//! a CRC-32 of the rest of the page, verified on every read, so torn or
//! bit-rotted pages are reported instead of silently trusted.
//!
//! Callers pin a page with [`Pager::get`] or [`Pager::allocate`], access it
//! through the returned [`PageRef`], and unpin it with [`Pager::release`].
//! Pinned pages are never evicted.
//!
//! Writes never touch the main file directly. Dirty pages go to the WAL,
//! either when evicted or at [`Pager::commit`], which ends with the header
//! page as a commit frame. [`Pager::rollback`] drops everything since the
//! last commit. Once the WAL grows past `wal_autocheckpoint` frames, a
//! checkpoint copies committed pages into the main file.

use std::collections::HashMap;
use std::fs::File;

use crate::crc32::crc32;
use crate::error::{Error, Result};
use crate::io::{read_at, write_at};
use crate::wal::Wal;

pub type PageId = u64;

/// Page 0 is the header, so 0 doubles as the "no page" sentinel in links.
pub const NULL_PAGE: PageId = 0;

pub const CHECKSUM_SIZE: usize = 4;
pub const MIN_PAGE_SIZE: u32 = 512;
/// Page-local offsets are u16, which caps the page size.
pub const MAX_PAGE_SIZE: u32 = 65536;
const MIN_CACHE_PAGES: usize = 8;

const MAGIC: &[u8; 16] = b"graphlite fmt 1\0";
const FORMAT_VERSION: u32 = 1;
const HEADER_SIZE: usize = 48;

#[derive(Clone, Debug)]
pub struct Options {
    /// Page size for new databases; existing files keep theirs.
    pub page_size: u32,
    /// Buffer pool capacity, in pages.
    pub cache_pages: usize,
    /// Checkpoint after a commit leaves at least this many frames in the WAL.
    pub wal_autocheckpoint: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options { page_size: 4096, cache_pages: 4096, wal_autocheckpoint: 1000 }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Page requests served from the cache.
    pub hits: u64,
    /// Pages read from the main file or the WAL.
    pub reads: u64,
    /// Pages written to the WAL.
    pub wal_writes: u64,
    /// Commits that wrote at least one page.
    pub commits: u64,
    pub checkpoints: u64,
}

/// Handle to a pinned page. Valid until passed to [`Pager::release`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRef(u32);

#[derive(Default)]
struct Frame {
    id: PageId,
    pins: u32,
    dirty: bool,
    referenced: bool,
}

/// Database state recorded in the header page.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Meta {
    page_count: u64,
    freelist_head: PageId,
    root: PageId,
}

impl Meta {
    fn encode(&self, page_size: usize, buf: &mut [u8]) {
        buf[0..16].copy_from_slice(MAGIC);
        buf[16..20].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf[20..24].copy_from_slice(&(page_size as u32).to_le_bytes());
        buf[24..32].copy_from_slice(&self.page_count.to_le_bytes());
        buf[32..40].copy_from_slice(&self.freelist_head.to_le_bytes());
        buf[40..48].copy_from_slice(&self.root.to_le_bytes());
    }

    fn decode(buf: &[u8]) -> Result<Meta> {
        let page_size = probe_page_size(buf)?;
        if buf.len() != page_size as usize - CHECKSUM_SIZE {
            return Err(Error::Corrupt("header page size mismatch"));
        }
        let m = Meta {
            page_count: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            freelist_head: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
            root: u64::from_le_bytes(buf[40..48].try_into().unwrap()),
        };
        if m.page_count == 0 || m.freelist_head >= m.page_count || m.root >= m.page_count {
            return Err(Error::Corrupt("header page ids out of range"));
        }
        Ok(m)
    }
}

/// Validates magic and version and returns the page size from a header.
fn probe_page_size(buf: &[u8]) -> Result<u32> {
    if &buf[0..16] != MAGIC {
        return Err(Error::Corrupt("bad magic"));
    }
    let version = u32::from_le_bytes(buf[16..20].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(Error::UnsupportedVersion(version));
    }
    let page_size = u32::from_le_bytes(buf[20..24].try_into().unwrap());
    validate_page_size(page_size)?;
    Ok(page_size)
}

fn validate_page_size(page_size: u32) -> Result<()> {
    if !(MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(Error::InvalidPageSize(page_size));
    }
    Ok(())
}

pub struct Pager {
    file: File,
    wal: Wal,
    page_size: usize,
    meta: Meta,
    /// `meta` as of the last commit, restored on rollback.
    committed: Meta,
    frames: Vec<Frame>,
    slab: Vec<u8>,
    map: HashMap<PageId, u32>,
    hand: usize,
    autocheckpoint: u64,
    stats: Stats,
}

impl Pager {
    /// Initializes a new database in `file`, which must be empty. Any
    /// contents of `wal_file` are discarded.
    pub fn create(file: File, wal_file: File, opts: &Options) -> Result<Pager> {
        validate_page_size(opts.page_size)?;
        let page_size = opts.page_size as usize;
        let meta = Meta { page_count: 1, freelist_head: NULL_PAGE, root: NULL_PAGE };
        let mut header = vec![0u8; page_size];
        meta.encode(page_size, &mut header);
        seal(&mut header);
        write_at(&file, &header, 0)?;
        file.sync_all()?;
        let wal = Wal::open(wal_file, page_size, true)?;
        Ok(Pager::init(file, wal, page_size, meta, opts))
    }

    /// Opens an existing database, recovering committed transactions from
    /// the WAL. The page size comes from the file.
    pub fn open(file: File, wal_file: File, opts: &Options) -> Result<Pager> {
        let mut probe = [0u8; HEADER_SIZE];
        read_at(&file, &mut probe, 0)?;
        let page_size = probe_page_size(&probe)? as usize;
        let wal = Wal::open(wal_file, page_size, false)?;

        let placeholder = Meta { page_count: 1, freelist_head: NULL_PAGE, root: NULL_PAGE };
        let mut p = Pager::init(file, wal, page_size, placeholder, opts);
        p.checkpoint()?;
        let mut header = vec![0u8; page_size];
        read_page(&p.file, 0, &mut header)?;
        p.meta = Meta::decode(&header[..page_size - CHECKSUM_SIZE])?;
        p.committed = p.meta;
        Ok(p)
    }

    fn init(file: File, wal: Wal, page_size: usize, meta: Meta, opts: &Options) -> Pager {
        let n = opts.cache_pages.max(MIN_CACHE_PAGES);
        Pager {
            file,
            wal,
            page_size,
            meta,
            committed: meta,
            frames: (0..n).map(|_| Frame::default()).collect(),
            slab: vec![0u8; n * page_size],
            map: HashMap::with_capacity(n),
            hand: 0,
            autocheckpoint: opts.wal_autocheckpoint.max(1),
            stats: Stats::default(),
        }
    }

    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Bytes per page available to callers; the trailing checksum is reserved.
    pub fn usable_size(&self) -> usize {
        self.page_size - CHECKSUM_SIZE
    }

    pub fn page_count(&self) -> u64 {
        self.meta.page_count
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Root page of the main B+tree. Owned by the layer above; persisted here.
    pub fn root(&self) -> PageId {
        self.meta.root
    }

    pub fn set_root(&mut self, root: PageId) {
        self.meta.root = root;
    }

    /// Pins the page in the pool. Pair with [`Pager::release`].
    pub fn get(&mut self, id: PageId) -> Result<PageRef> {
        if id == NULL_PAGE || id >= self.meta.page_count {
            return Err(Error::Corrupt("page id out of range"));
        }
        if let Some(&i) = self.map.get(&id) {
            let f = &mut self.frames[i as usize];
            f.pins += 1;
            f.referenced = true;
            self.stats.hits += 1;
            return Ok(PageRef(i));
        }
        let i = self.evict()?;
        let range = self.frame_range(i);
        self.stats.reads += 1;
        let buf = &mut self.slab[range];
        match self.wal.lookup(id) {
            Some(n) => {
                self.wal.read_page(n, buf)?;
                verify(buf)?;
            }
            None => read_page(&self.file, id, buf)?,
        }
        self.install(i, id, false);
        Ok(PageRef(i))
    }

    pub fn release(&mut self, r: PageRef) {
        let f = &mut self.frames[r.0 as usize];
        debug_assert!(f.pins > 0, "release of unpinned page");
        f.pins -= 1;
    }

    pub fn id(&self, r: PageRef) -> PageId {
        self.frames[r.0 as usize].id
    }

    pub fn page(&self, r: PageRef) -> &[u8] {
        let range = self.frame_range(r.0);
        &self.slab[range][..self.usable_size()]
    }

    /// Mutable access to a pinned page; marks it dirty.
    pub fn page_mut(&mut self, r: PageRef) -> &mut [u8] {
        self.frames[r.0 as usize].dirty = true;
        let range = self.frame_range(r.0);
        let usable = self.usable_size();
        &mut self.slab[range][..usable]
    }

    /// Returns a zeroed, pinned, dirty page, reusing a freed page when one exists.
    pub fn allocate(&mut self) -> Result<PageRef> {
        if self.meta.freelist_head != NULL_PAGE {
            let r = self.get(self.meta.freelist_head)?;
            let next = u64::from_le_bytes(self.page(r)[0..8].try_into().unwrap());
            if next >= self.meta.page_count {
                self.release(r);
                return Err(Error::Corrupt("freelist link out of range"));
            }
            self.meta.freelist_head = next;
            self.page_mut(r).fill(0);
            return Ok(r);
        }
        let i = self.evict()?;
        let id = self.meta.page_count;
        self.meta.page_count += 1;
        let range = self.frame_range(i);
        self.slab[range].fill(0);
        self.install(i, id, true);
        Ok(PageRef(i))
    }

    /// Returns a page to the freelist. The caller must not hold a pin on it.
    pub fn free(&mut self, id: PageId) -> Result<()> {
        let r = self.get(id)?;
        debug_assert_eq!(self.frames[r.0 as usize].pins, 1, "freeing a pinned page");
        let head = self.meta.freelist_head;
        let page = self.page_mut(r);
        page.fill(0);
        page[0..8].copy_from_slice(&head.to_le_bytes());
        self.meta.freelist_head = id;
        self.release(r);
        Ok(())
    }

    /// Whether anything changed since the last commit.
    pub fn has_changes(&self) -> bool {
        self.meta != self.committed || self.wal.has_pending() || self.frames.iter().any(|f| f.dirty)
    }

    /// Atomically and durably applies every change since the last commit.
    pub fn commit(&mut self) -> Result<()> {
        if !self.has_changes() {
            return Ok(());
        }
        for i in 0..self.frames.len() {
            if self.frames[i].id != NULL_PAGE && self.frames[i].dirty {
                self.write_frame(i as u32)?;
            }
        }
        let mut header = vec![0u8; self.page_size];
        self.meta.encode(self.page_size, &mut header);
        seal(&mut header);
        self.wal.write(NULL_PAGE, &header, true)?;
        self.stats.wal_writes += 1;
        self.wal.commit()?;
        self.committed = self.meta;
        self.stats.commits += 1;
        if self.wal.len() >= self.autocheckpoint {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Discards every change since the last commit. No page may be pinned.
    pub fn rollback(&mut self) {
        debug_assert!(self.frames.iter().all(|f| f.pins == 0), "rollback with pinned pages");
        // Cached pages may hold uncommitted contents, clean or not.
        for f in &mut self.frames {
            *f = Frame::default();
        }
        self.map.clear();
        self.wal.rollback();
        self.meta = self.committed;
    }

    /// Copies committed pages from the WAL into the main file and empties the
    /// WAL. Does nothing while a transaction has written to the WAL.
    pub fn checkpoint(&mut self) -> Result<()> {
        if self.wal.has_pending() || !self.wal.has_committed() {
            return Ok(());
        }
        let mut buf = vec![0u8; self.page_size];
        let mut pages: Vec<(PageId, u64)> = self.wal.committed_pages().collect();
        pages.sort_unstable();
        for (id, n) in pages {
            self.wal.read_page(n, &mut buf)?;
            verify(&buf)?;
            write_at(&self.file, &buf, id * self.page_size as u64)?;
        }
        self.file.sync_data()?;
        self.wal.reset()?;
        self.stats.checkpoints += 1;
        Ok(())
    }

    fn frame_range(&self, i: u32) -> std::ops::Range<usize> {
        let start = i as usize * self.page_size;
        start..start + self.page_size
    }

    fn install(&mut self, i: u32, id: PageId, dirty: bool) {
        self.frames[i as usize] = Frame { id, pins: 1, dirty, referenced: true };
        self.map.insert(id, i);
    }

    /// Finds an unpinned frame with the clock algorithm, spilling it to the
    /// WAL if dirty.
    fn evict(&mut self) -> Result<u32> {
        let n = self.frames.len();
        for _ in 0..2 * n {
            let i = self.hand;
            self.hand = (self.hand + 1) % n;
            let f = &mut self.frames[i];
            if f.pins > 0 {
                continue;
            }
            if f.referenced {
                f.referenced = false;
                continue;
            }
            if f.id != NULL_PAGE {
                if f.dirty {
                    self.write_frame(i as u32)?;
                }
                let id = std::mem::take(&mut self.frames[i]).id;
                self.map.remove(&id);
            }
            return Ok(i as u32);
        }
        Err(Error::PoolExhausted)
    }

    fn write_frame(&mut self, i: u32) -> Result<()> {
        let id = self.frames[i as usize].id;
        let range = self.frame_range(i);
        seal(&mut self.slab[range.clone()]);
        self.wal.write(id, &self.slab[range], false)?;
        self.frames[i as usize].dirty = false;
        self.stats.wal_writes += 1;
        Ok(())
    }
}

impl Drop for Pager {
    /// Uncommitted changes are discarded; committed ones are checkpointed so
    /// the WAL is empty after a clean close.
    fn drop(&mut self) {
        if std::thread::panicking() || self.frames.iter().any(|f| f.pins > 0) {
            return;
        }
        self.rollback();
        let _ = self.checkpoint();
    }
}

/// Reads page `id` of the main file into `buf` (one full page) and verifies it.
fn read_page(file: &File, id: PageId, buf: &mut [u8]) -> Result<()> {
    read_at(file, buf, id * buf.len() as u64)?;
    verify(buf)
}

fn verify(page: &[u8]) -> Result<()> {
    let (body, sum) = page.split_at(page.len() - CHECKSUM_SIZE);
    if crc32(body) != u32::from_le_bytes(sum.try_into().unwrap()) {
        return Err(Error::Corrupt("page checksum mismatch"));
    }
    Ok(())
}

/// Stores the checksum of the page body in its trailing bytes.
fn seal(page: &mut [u8]) {
    let (body, sum) = page.split_at_mut(page.len() - CHECKSUM_SIZE);
    sum.copy_from_slice(&crc32(body).to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs::OpenOptions;
    use std::path::Path;

    fn small() -> Options {
        Options { page_size: 512, cache_pages: 8, wal_autocheckpoint: u64::MAX }
    }

    fn files(dir: &Path) -> (File, File) {
        let open = |name: &str| {
            OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(name)).unwrap()
        };
        (open("t.db"), open("t.db-wal"))
    }

    fn create(dir: &Path, opts: &Options) -> Pager {
        let (f, w) = files(dir);
        Pager::create(f, w, opts).unwrap()
    }

    fn open(dir: &Path, opts: &Options) -> Pager {
        let (f, w) = files(dir);
        Pager::open(f, w, opts).unwrap()
    }

    /// Allocates `n` pages filled with `fill`, returning their ids.
    fn fill_pages(p: &mut Pager, n: u64, fill: u8) -> Vec<PageId> {
        (0..n)
            .map(|_| {
                let r = p.allocate().unwrap();
                p.page_mut(r).fill(fill);
                p.release(r);
                p.id(r)
            })
            .collect()
    }

    fn assert_filled(p: &mut Pager, ids: &[PageId], fill: u8) {
        for &id in ids {
            let r = p.get(id).unwrap();
            assert!(p.page(r).iter().all(|&b| b == fill), "page {id}");
            p.release(r);
        }
    }

    #[test]
    fn committed_pages_survive_eviction_and_reopen() {
        let dir = TempDir::new();
        let ids = {
            let mut p = create(dir.path(), &small());
            let ids = fill_pages(&mut p, 100, 7);
            p.commit().unwrap();
            assert!(p.stats().wal_writes > 100, "evictions should spill to the wal");
            ids
        };
        let mut p = open(dir.path(), &small());
        assert_eq!(p.page_count(), 101);
        assert_filled(&mut p, &ids, 7);
    }

    #[test]
    fn committed_frames_are_recovered_from_the_wal() {
        let dir = TempDir::new();
        let ids = {
            let mut p = create(dir.path(), &small());
            let ids = fill_pages(&mut p, 20, 3);
            p.commit().unwrap();
            std::mem::forget(p); // crash: no rollback, no checkpoint
            ids
        };
        assert!(std::fs::metadata(dir.path().join("t.db-wal")).unwrap().len() > 0);
        let mut p = open(dir.path(), &small());
        assert_eq!(p.stats().checkpoints, 1);
        assert_filled(&mut p, &ids, 3);
    }

    #[test]
    fn uncommitted_changes_are_lost_on_crash() {
        let dir = TempDir::new();
        let (kept, lost) = {
            let mut p = create(dir.path(), &small());
            let kept = fill_pages(&mut p, 5, 1);
            p.commit().unwrap();
            // Overwrite the kept pages and add more, spilling some to the wal.
            for &id in &kept {
                let r = p.get(id).unwrap();
                p.page_mut(r).fill(9);
                p.release(r);
            }
            let lost = fill_pages(&mut p, 30, 2);
            std::mem::forget(p);
            (kept, lost)
        };
        let mut p = open(dir.path(), &small());
        assert_eq!(p.page_count(), 6);
        assert_filled(&mut p, &kept, 1);
        assert!(p.get(lost[0]).is_err());
    }

    #[test]
    fn rollback_restores_last_commit() {
        let dir = TempDir::new();
        let mut p = create(dir.path(), &small());
        let ids = fill_pages(&mut p, 5, 1);
        p.commit().unwrap();
        for &id in &ids {
            let r = p.get(id).unwrap();
            p.page_mut(r).fill(5);
            p.release(r);
        }
        fill_pages(&mut p, 30, 2);
        p.free(ids[0]).unwrap();
        p.rollback();
        assert_eq!(p.page_count(), 6);
        assert_filled(&mut p, &ids, 1);
        // The rolled-back frames are overwritten by the next transaction.
        let more = fill_pages(&mut p, 3, 4);
        p.commit().unwrap();
        drop(p);
        let mut p = open(dir.path(), &small());
        assert_filled(&mut p, &ids, 1);
        assert_filled(&mut p, &more, 4);
    }

    #[test]
    fn autocheckpoint_empties_the_wal() {
        let dir = TempDir::new();
        let opts = Options { wal_autocheckpoint: 10, ..small() };
        let mut p = create(dir.path(), &opts);
        let ids = fill_pages(&mut p, 20, 8);
        p.commit().unwrap();
        assert_eq!(p.stats().checkpoints, 1);
        assert_eq!(p.wal.len(), 0);
        assert_filled(&mut p, &ids, 8);
    }

    #[test]
    fn freed_pages_are_reused() {
        let dir = TempDir::new();
        let mut p = create(dir.path(), &small());
        let ids = fill_pages(&mut p, 3, 0);
        p.free(ids[0]).unwrap();
        p.free(ids[2]).unwrap();
        let a = p.allocate().unwrap();
        let b = p.allocate().unwrap();
        assert_eq!(p.id(a), ids[2]);
        assert_eq!(p.id(b), ids[0]);
        assert_eq!(p.page_count(), 4);
        p.release(a);
        p.release(b);
    }

    #[test]
    fn checksum_mismatch_is_corruption() {
        let dir = TempDir::new();
        let id = {
            let mut p = create(dir.path(), &small());
            let id = fill_pages(&mut p, 1, 0xab)[0];
            p.commit().unwrap();
            id
        };
        let (f, _) = files(dir.path());
        write_at(&f, &[0], id * 512 + 100).unwrap();
        let mut p = open(dir.path(), &small());
        assert!(matches!(p.get(id), Err(Error::Corrupt(_))));
    }

    #[test]
    fn pool_exhaustion_is_an_error() {
        let dir = TempDir::new();
        let mut p = create(dir.path(), &small());
        let pinned: Vec<PageRef> = (0..MIN_CACHE_PAGES).map(|_| p.allocate().unwrap()).collect();
        assert!(matches!(p.allocate(), Err(Error::PoolExhausted)));
        for r in pinned {
            p.release(r);
        }
    }
}
