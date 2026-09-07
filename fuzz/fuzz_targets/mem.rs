//! A `Vec`-backed medium, shared by both targets.
//!
//! Deliberately strict about the things the traits promise — a
//! mismatched buffer length and an out-of-range LBA are errors, not
//! truncations — because a fuzz target whose backend silently forgives
//! is a fuzz target that cannot find the crate forgetting to check.

// Shared by both targets, and each uses a different half of it: the read
// target never constructs a blank image, the round-trip target never
// wraps a fuzzer-supplied one.
#![allow(dead_code)]

use amiga_ffs::{BlockSink, BlockSource};

#[derive(Debug)]
pub enum MemError {
    BadBufferLen { got: usize, want: usize },
    OutOfRange { lba: u64, blocks: u64 },
}

impl core::fmt::Display for MemError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for MemError {}

pub struct MemDisk {
    bs: usize,
    data: Vec<u8>,
}

impl MemDisk {
    pub fn new(bs: usize, data: Vec<u8>) -> Self {
        Self { bs, data }
    }

    pub fn blank(bs: usize, nblocks: u64) -> Self {
        Self {
            bs,
            data: vec![0u8; bs * nblocks as usize],
        }
    }

    pub fn blocks(&self) -> u64 {
        self.data.len() as u64 / self.bs as u64
    }
}

impl BlockSource for MemDisk {
    type Error = MemError;

    fn block_size(&self) -> usize {
        self.bs
    }

    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), MemError> {
        if buf.len() != self.bs {
            return Err(MemError::BadBufferLen {
                got: buf.len(),
                want: self.bs,
            });
        }
        if lba >= self.blocks() {
            return Err(MemError::OutOfRange {
                lba,
                blocks: self.blocks(),
            });
        }
        let off = lba as usize * self.bs;
        buf.copy_from_slice(&self.data[off..off + self.bs]);
        Ok(())
    }

    fn block_count(&self) -> Option<u64> {
        Some(self.blocks())
    }
}

impl BlockSink for MemDisk {
    type Error = MemError;

    fn block_size(&self) -> usize {
        self.bs
    }

    fn write_block(&mut self, lba: u64, buf: &[u8]) -> Result<(), MemError> {
        if buf.len() != self.bs {
            return Err(MemError::BadBufferLen {
                got: buf.len(),
                want: self.bs,
            });
        }
        if lba >= self.blocks() {
            return Err(MemError::OutOfRange {
                lba,
                blocks: self.blocks(),
            });
        }
        let off = lba as usize * self.bs;
        self.data[off..off + self.bs].copy_from_slice(buf);
        Ok(())
    }

    fn block_count(&self) -> Option<u64> {
        Some(self.blocks())
    }
}
