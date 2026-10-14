//! Pager: maps page ids to fixed-size pages of a single file, cached in a
//! bounded buffer pool with clock eviction.
//!
//! Page 0 holds the file header. Every page reserves its trailing 4 bytes for
//! a CRC-32 of the rest of the page, written on writeback and verified on
//! read, so torn or bit-rotted pages are reported instead of silently trusted.
//!
//! Callers pin a page with [`Pager::get`] or [`Pager::allocate`], access it
//! through the returned [`PageRef`], and unpin it with [`Pager::release`].
//! Pinned pages are never evicted.
//!
//! TODO(wal): dirty pages are written back in place, both on eviction and on
//! `flush`, so a crash mid-write can leave the file half-updated. The WAL
//! will take over that path and give `flush` commit semantics.

use std::collections::HashMap;
use std::fs::File;
use std::io;

use crate::crc32::crc32;
use crate::error::{Error, Result};

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
    pub page_size: u32,
    /// Buffer pool capacity, in pages.
    pub cache_pages: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { page_size: 4096, cache_pages: 4096 }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub hits: u64,
    pub reads: u64,
    pub writes: u64,
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

struct Header {
    page_size: u32,
    page_count: u64,
    freelist_head: PageId,
    root: PageId,
}

impl Header {
    fn encode(&self, buf: &mut [u8]) {
        buf[0..16].copy_from_slice(MAGIC);
        buf[16..20].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf[20..24].copy_from_slice(&self.page_size.to_le_bytes());
        buf[24..32].copy_from_slice(&self.page_count.to_le_bytes());
        buf[32..40].copy_from_slice(&self.freelist_head.to_le_bytes());
        buf[40..48].copy_from_slice(&self.root.to_le_bytes());
    }

    fn decode(buf: &[u8; HEADER_SIZE]) -> Result<Header> {
        if &buf[0..16] != MAGIC {
            return Err(Error::Corrupt("bad magic"));
        }
        let version = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        let h = Header {
            page_size: u32::from_le_bytes(buf[20..24].try_into().unwrap()),
            page_count: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            freelist_head: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
            root: u64::from_le_bytes(buf[40..48].try_into().unwrap()),
        };
        validate_page_size(h.page_size)?;
        if h.page_count == 0 || h.freelist_head >= h.page_count || h.root >= h.page_count {
            return Err(Error::Corrupt("header page ids out of range"));
        }
        Ok(h)
    }
}

fn validate_page_size(page_size: u32) -> Result<()> {
    if !(MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(Error::InvalidPageSize(page_size));
    }
    Ok(())
}

pub struct Pager {
    file: File,
    page_size: usize,
    page_count: u64,
    freelist_head: PageId,
    root: PageId,
    frames: Vec<Frame>,
    slab: Vec<u8>,
    map: HashMap<PageId, u32>,
    hand: usize,
    stats: Stats,
}

impl Pager {
    /// Initializes a new database in `file`, which must be empty.
    pub fn create(file: File, opts: &Options) -> Result<Pager> {
        validate_page_size(opts.page_size)?;
        let mut p = Pager::init(file, opts.page_size as usize, opts.cache_pages);
        p.write_header()?;
        Ok(p)
    }

    /// Opens an existing database in `file`. The page size comes from the file.
    pub fn open(file: File, opts: &Options) -> Result<Pager> {
        let mut probe = [0u8; HEADER_SIZE];
        read_at(&file, &mut probe, 0)?;
        let h = Header::decode(&probe)?;

        let mut p = Pager::init(file, h.page_size as usize, opts.cache_pages);
        let mut page0 = vec![0u8; p.page_size];
        read_page(&p.file, 0, &mut page0)?;
        p.page_count = h.page_count;
        p.freelist_head = h.freelist_head;
        p.root = h.root;
        Ok(p)
    }

    fn init(file: File, page_size: usize, cache_pages: usize) -> Pager {
        let n = cache_pages.max(MIN_CACHE_PAGES);
        Pager {
            file,
            page_size,
            page_count: 1,
            freelist_head: NULL_PAGE,
            root: NULL_PAGE,
            frames: (0..n).map(|_| Frame::default()).collect(),
            slab: vec![0u8; n * page_size],
            map: HashMap::with_capacity(n),
            hand: 0,
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
        self.page_count
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Root page of the main B+tree. Owned by the layer above; persisted here.
    pub fn root(&self) -> PageId {
        self.root
    }

    pub fn set_root(&mut self, root: PageId) {
        self.root = root;
    }

    /// Pins the page in the pool. Pair with [`Pager::release`].
    pub fn get(&mut self, id: PageId) -> Result<PageRef> {
        if id == NULL_PAGE || id >= self.page_count {
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
        read_page(&self.file, id, &mut self.slab[range])?;
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
        if self.freelist_head != NULL_PAGE {
            let r = self.get(self.freelist_head)?;
            let next = u64::from_le_bytes(self.page(r)[0..8].try_into().unwrap());
            if next >= self.page_count {
                self.release(r);
                return Err(Error::Corrupt("freelist link out of range"));
            }
            self.freelist_head = next;
            self.page_mut(r).fill(0);
            return Ok(r);
        }
        let i = self.evict()?;
        let id = self.page_count;
        self.page_count += 1;
        let range = self.frame_range(i);
        self.slab[range].fill(0);
        self.install(i, id, true);
        Ok(PageRef(i))
    }

    /// Returns a page to the freelist. The caller must not hold a pin on it.
    pub fn free(&mut self, id: PageId) -> Result<()> {
        let r = self.get(id)?;
        debug_assert_eq!(self.frames[r.0 as usize].pins, 1, "freeing a pinned page");
        let head = self.freelist_head;
        let page = self.page_mut(r);
        page.fill(0);
        page[0..8].copy_from_slice(&head.to_le_bytes());
        self.freelist_head = id;
        self.release(r);
        Ok(())
    }

    /// Writes every dirty page and the header, then fsyncs.
    pub fn flush(&mut self) -> Result<()> {
        for i in 0..self.frames.len() {
            let f = &self.frames[i];
            if f.id != NULL_PAGE && f.dirty {
                self.write_frame(i as u32)?;
                self.frames[i].dirty = false;
            }
        }
        self.write_header()?;
        self.file.sync_data()?;
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

    /// Finds an unpinned frame with the clock algorithm, writing it back if dirty.
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
        self.stats.writes += 1;
        write_at(&self.file, &self.slab[range], id * self.page_size as u64)?;
        Ok(())
    }

    fn write_header(&mut self) -> Result<()> {
        let mut buf = vec![0u8; self.page_size];
        Header {
            page_size: self.page_size as u32,
            page_count: self.page_count,
            freelist_head: self.freelist_head,
            root: self.root,
        }
        .encode(&mut buf[..HEADER_SIZE]);
        seal(&mut buf);
        self.stats.writes += 1;
        write_at(&self.file, &buf, 0)?;
        Ok(())
    }
}

/// Reads page `id` into `buf` (one full page) and verifies its checksum.
fn read_page(file: &File, id: PageId, buf: &mut [u8]) -> Result<()> {
    read_at(file, buf, id * buf.len() as u64)?;
    let (body, sum) = buf.split_at(buf.len() - CHECKSUM_SIZE);
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

fn read_at(file: &File, buf: &mut [u8], offset: u64) -> Result<()> {
    match read_exact_at(file, buf, offset) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(Error::Corrupt("file truncated")),
        Err(e) => Err(e.into()),
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, offset)?;
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs::OpenOptions;

    fn small() -> Options {
        Options { page_size: 512, cache_pages: 8 }
    }

    fn open_rw(path: &std::path::Path) -> File {
        OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).unwrap()
    }

    #[test]
    fn pages_survive_eviction_and_reopen() {
        let dir = TempDir::new();
        let path = dir.path().join("t.db");
        let n_pages = 100u64;
        {
            let mut p = Pager::create(open_rw(&path), &small()).unwrap();
            for i in 0..n_pages {
                let r = p.allocate().unwrap();
                p.page_mut(r).fill(i as u8);
                p.release(r);
            }
            p.flush().unwrap();
            assert!(p.stats().writes > n_pages, "evictions should force writeback");
        }
        let mut p = Pager::open(open_rw(&path), &small()).unwrap();
        assert_eq!(p.page_count(), n_pages + 1);
        for id in 1..=n_pages {
            let r = p.get(id).unwrap();
            assert!(p.page(r).iter().all(|&b| b == (id - 1) as u8));
            p.release(r);
        }
    }

    #[test]
    fn freed_pages_are_reused() {
        let dir = TempDir::new();
        let mut p = Pager::create(open_rw(&dir.path().join("t.db")), &small()).unwrap();
        let ids: Vec<PageId> = (0..3)
            .map(|_| {
                let r = p.allocate().unwrap();
                p.release(r);
                p.id(r)
            })
            .collect();
        p.free(ids[0]).unwrap();
        p.free(ids[2]).unwrap();
        let a = p.allocate().unwrap();
        let b = p.allocate().unwrap();
        assert_eq!(p.id(a), ids[2]);
        assert_eq!(p.id(b), ids[0]);
        assert_eq!(p.page_count(), 4);
    }

    #[test]
    fn checksum_mismatch_is_corruption() {
        let dir = TempDir::new();
        let path = dir.path().join("t.db");
        let id = {
            let mut p = Pager::create(open_rw(&path), &small()).unwrap();
            let r = p.allocate().unwrap();
            p.page_mut(r).fill(0xab);
            p.release(r);
            p.flush().unwrap();
            p.id(r)
        };
        write_at(&open_rw(&path), &[0], id * 512 + 100).unwrap();
        let mut p = Pager::open(open_rw(&path), &small()).unwrap();
        assert!(matches!(p.get(id), Err(Error::Corrupt(_))));
    }

    #[test]
    fn pool_exhaustion_is_an_error() {
        let dir = TempDir::new();
        let mut p = Pager::create(open_rw(&dir.path().join("t.db")), &small()).unwrap();
        for _ in 0..MIN_CACHE_PAGES {
            p.allocate().unwrap();
        }
        assert!(matches!(p.allocate(), Err(Error::PoolExhausted)));
    }
}
