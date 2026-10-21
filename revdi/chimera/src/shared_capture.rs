// SPDX-License-Identifier: GPL-2.0-or-later
//! GPU capture copy boundary; no capture client or Vulkan dependency is required here.

use crate::capture_pool::WriteLease;
use std::os::fd::BorrowedFd;
use std::time::Duration;

/// A leased compositor framebuffer. Its producer must honor the capture lease's read fence.
/// The caller keeps the lease and file descriptors alive through `copy_frame`, then releases
/// the lease before publishing the owned destination. A revoked lease invalidates the copy.
pub struct SharedFramebuffer<'a> {
    pub fd: BorrowedFd<'a>,
    pub producer_fence: Option<BorrowedFd<'a>>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub offset: u64,
    pub modifier: u64,
    pub format: u32,
    pub size: u64,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct SharedCopyStats {
    pub elapsed: Duration,
    pub bytes: u64,
    pub imports: u64,
}

/// Synchronous copy into an exclusively owned pool slot. Implementations wait for GPU reads
/// and writes to finish before returning, including failures, so neither source nor target
/// ownership can be released while the GPU accesses it. No CPU desktop copy is permitted.
pub trait SharedCaptureCopy: Send {
    fn copy_frame(
        &mut self,
        source: SharedFramebuffer<'_>,
        destination: &mut WriteLease,
    ) -> Result<SharedCopyStats, String>;
}
