//! Write-ahead log: the `-wal` file next to the database.
//!
//! Modified pages are appended here as frames instead of being written over
//! the main file. A transaction commits by appending the header page (page 0)
//! as a frame flagged `commit` and fsyncing. Recovery replays frames up to the
//! last valid commit frame; anything after it is an aborted transaction.
//!
//! A checkpoint copies the latest committed version of each page into the
//! main file, fsyncs it, and resets the log. Checkpointing is idempotent, so
//! a crash halfway through is repaired by checkpointing again on open.
//!
//! ```text
//! header (32 bytes):  magic [16] | page_size u32 | salt u32 | crc u32 | pad
//! frame:              page_id u64 | commit u32 | salt u32 | crc u32 | pad u32 | page
//! ```
//!
//! Pages arrive sealed with their own trailing CRC-32C, so the frame crc only
//! covers the first 16 header bytes plus that page checksum; recovery checks
//! both. The salt changes on every reset, so frames left over from an earlier
//! generation are never mistaken for current ones.

use std::fs::File;

use crate::crc32::{Hasher, crc32};
use crate::error::{Error, Result};
use crate::hash::IdMap;
use crate::io::{read_at, write_at};
use crate::pager::PageId;

const MAGIC: &[u8; 16] = b"graphlite wal 2\0";
const HEADER_SIZE: u64 = 32;
const FRAME_HEADER_SIZE: usize = 24;

pub struct Wal {
    file: File,
    page_size: usize,
    salt: u32,
    /// Frames `[0, committed_frames)` are durable.
    committed_frames: u64,
    /// Frames `[committed_frames, frames)` belong to the open transaction.
    frames: u64,
    /// Latest committed frame for each page.
    committed: IdMap<u64>,
    /// Latest frame for each page written by the open transaction.
    pending: IdMap<u64>,
    /// Reused buffer for one frame (header + page).
    buf: Vec<u8>,
}

impl Wal {
    /// Opens the log, recovering every committed frame. With `reset`, any
    /// existing contents are discarded instead.
    pub fn open(file: File, page_size: usize, reset: bool) -> Result<Wal> {
        let mut wal = Wal {
            file,
            page_size,
            salt: 0,
            committed_frames: 0,
            frames: 0,
            committed: IdMap::default(),
            pending: IdMap::default(),
            buf: vec![0u8; FRAME_HEADER_SIZE + page_size],
        };
        match wal.read_header()? {
            Some(salt) if !reset => {
                wal.salt = salt;
                wal.recover()?;
            }
            prev => {
                wal.salt = prev.unwrap_or_else(seed_salt);
                wal.reset()?;
            }
        }
        Ok(wal)
    }

    /// Returns the salt from a valid header, or `None` for an empty or torn one.
    fn read_header(&mut self) -> Result<Option<u32>> {
        if self.file.metadata()?.len() < HEADER_SIZE {
            return Ok(None);
        }
        let mut h = [0u8; HEADER_SIZE as usize];
        read_at(&self.file, &mut h, 0)?;
        let crc = u32::from_le_bytes(h[24..28].try_into().unwrap());
        if &h[0..16] != MAGIC || crc32(&h[..24]) != crc {
            // A torn header can only come from a crash during reset, which
            // happens after a completed checkpoint: nothing here is needed.
            return Ok(None);
        }
        let page_size = u32::from_le_bytes(h[16..20].try_into().unwrap()) as usize;
        if page_size != self.page_size {
            return Err(Error::Corrupt("wal page size does not match database"));
        }
        Ok(Some(u32::from_le_bytes(h[20..24].try_into().unwrap())))
    }

    fn recover(&mut self) -> Result<()> {
        let len = self.file.metadata()?.len();
        let frame_size = self.frame_size();
        let mut n = 0u64;
        while HEADER_SIZE + (n + 1) * frame_size <= len {
            let off = self.frame_offset(n);
            read_at(&self.file, &mut self.buf, off)?;
            let h = &self.buf[..FRAME_HEADER_SIZE];
            let salt = u32::from_le_bytes(h[12..16].try_into().unwrap());
            let crc = u32::from_le_bytes(h[16..20].try_into().unwrap());
            if salt != self.salt || crc != frame_crc(&self.buf) || !page_intact(&self.buf[FRAME_HEADER_SIZE..]) {
                break;
            }
            let id = u64::from_le_bytes(h[0..8].try_into().unwrap());
            self.pending.insert(id, n);
            n += 1;
            if u32::from_le_bytes(h[8..12].try_into().unwrap()) != 0 {
                self.committed.extend(self.pending.drain());
                self.committed_frames = n;
            }
        }
        self.pending.clear();
        self.frames = self.committed_frames;
        Ok(())
    }

    fn frame_size(&self) -> u64 {
        (FRAME_HEADER_SIZE + self.page_size) as u64
    }

    fn frame_offset(&self, n: u64) -> u64 {
        HEADER_SIZE + n * self.frame_size()
    }

    /// Frames in the log, committed or not.
    pub fn len(&self) -> u64 {
        self.frames
    }

    /// Whether the open transaction has written frames.
    pub fn has_pending(&self) -> bool {
        self.frames > self.committed_frames
    }

    pub fn has_committed(&self) -> bool {
        !self.committed.is_empty()
    }

    /// The frame holding the newest version of page `id`, if the log has one.
    pub fn lookup(&self, id: PageId) -> Option<u64> {
        self.pending.get(&id).or_else(|| self.committed.get(&id)).copied()
    }

    /// Reads the page stored in frame `n` into `buf`.
    pub fn read_page(&self, n: u64, buf: &mut [u8]) -> Result<()> {
        read_at(&self.file, buf, self.frame_offset(n) + FRAME_HEADER_SIZE as u64)
    }

    /// Writes `page`, which must carry its checksum in its last 4 bytes, as
    /// the newest version of `id` in the open transaction.
    /// A page already written by this transaction is overwritten in place,
    /// since no committed state depends on that frame.
    pub fn write(&mut self, id: PageId, page: &[u8], commit: bool) -> Result<()> {
        let n = match self.pending.get(&id) {
            Some(&n) if !commit => n,
            _ => {
                let n = self.frames;
                self.frames += 1;
                self.pending.insert(id, n);
                n
            }
        };
        let h = &mut self.buf[..FRAME_HEADER_SIZE];
        h[0..8].copy_from_slice(&id.to_le_bytes());
        h[8..12].copy_from_slice(&u32::from(commit).to_le_bytes());
        h[12..16].copy_from_slice(&self.salt.to_le_bytes());
        h[20..24].fill(0);
        self.buf[FRAME_HEADER_SIZE..].copy_from_slice(page);
        let crc = frame_crc(&self.buf);
        self.buf[16..20].copy_from_slice(&crc.to_le_bytes());
        let off = self.frame_offset(n);
        write_at(&self.file, &self.buf, off)?;
        Ok(())
    }

    /// Makes the open transaction durable. Its last frame must be a commit frame.
    pub fn commit(&mut self) -> Result<()> {
        self.file.sync_data()?;
        self.committed.extend(self.pending.drain());
        self.committed_frames = self.frames;
        Ok(())
    }

    /// Forgets the open transaction's frames; later writes overwrite them.
    pub fn rollback(&mut self) {
        self.pending.clear();
        self.frames = self.committed_frames;
    }

    /// Committed pages and the frame holding each one's newest version.
    pub fn committed_pages(&self) -> impl Iterator<Item = (PageId, u64)> + '_ {
        self.committed.iter().map(|(&id, &n)| (id, n))
    }

    /// Empties the log under a new salt. Only valid once every committed page
    /// is durable in the main file.
    pub fn reset(&mut self) -> Result<()> {
        debug_assert!(self.pending.is_empty(), "reset with an open transaction");
        self.salt = self.salt.wrapping_add(1);
        let mut h = [0u8; HEADER_SIZE as usize];
        h[0..16].copy_from_slice(MAGIC);
        h[16..20].copy_from_slice(&(self.page_size as u32).to_le_bytes());
        h[20..24].copy_from_slice(&self.salt.to_le_bytes());
        let crc = crc32(&h[..24]);
        h[24..28].copy_from_slice(&crc.to_le_bytes());
        write_at(&self.file, &h, 0)?;
        self.file.set_len(HEADER_SIZE)?;
        self.file.sync_data()?;
        self.committed.clear();
        self.committed_frames = 0;
        self.frames = 0;
        Ok(())
    }
}

fn frame_crc(frame: &[u8]) -> u32 {
    let mut c = Hasher::new();
    c.update(&frame[..16]);
    c.update(&frame[frame.len() - 4..]);
    c.finish()
}

fn page_intact(page: &[u8]) -> bool {
    let (body, sum) = page.split_at(page.len() - 4);
    crc32(body) == u32::from_le_bytes(sum.try_into().unwrap())
}

fn seed_salt() -> u32 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (t.as_nanos() as u32) ^ std::process::id()
}
