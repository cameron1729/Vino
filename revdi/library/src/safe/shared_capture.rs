// SPDX-License-Identifier: LGPL-2.1-or-later
//! Experimental DMA-buf capture lease ABI. Consumers must wait for the producer
//! fence, finish copying into private storage, and successfully finish the lease
//! before publishing that private snapshot. Revocation invalidates the snapshot;
//! the kernel retains source protection until outstanding GPU imports retire.

use super::{Device, Error, Mode, BUFFER_ID};
use crate::ffi;
use core::ffi::{c_int, c_ulong, c_void};
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

const VERSION: u32 = 1;
const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const ACQUIRE: c_ulong = (3 << 30) | (80 << 16) | (b'd' as c_ulong) << 8 | 0x45;
const RELEASE: c_ulong = (3 << 30) | (16 << 16) | (b'd' as c_ulong) << 8 | 0x46;
const F_GETFD: c_int = 1;
const FD_CLOEXEC: c_int = 1;

extern "C" {
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
}

/// Matches tools/gpu/evdi-shared-capture.h, including explicit 64-bit reserved slots.
#[repr(C)]
#[derive(Clone, Copy)]
struct AcquireFrame {
    version: u32,
    flags: u32,
    dmabuf_fd: i32,
    ready_fence_fd: i32,
    width: u32,
    height: u32,
    pitch: u32,
    format: u32,
    offset: u64,
    modifier: u64,
    size: u64,
    token: u64,
    reserved: [u64; 2],
}

impl Default for AcquireFrame {
    fn default() -> Self {
        Self {
            version: VERSION,
            flags: 0,
            dmabuf_fd: -1,
            ready_fence_fd: -1,
            width: 0,
            height: 0,
            pitch: 0,
            format: 0,
            offset: 0,
            modifier: 0,
            size: 0,
            token: 0,
            reserved: [0; 2],
        }
    }
}

#[repr(C)]
struct ReleaseFrame {
    token: u64,
    flags: u32,
    reserved: u32,
}

/// Immutable layout returned by the experimental EVDI lease ABI. `size` describes
/// the complete DMA allocation, including any prefix before `offset`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SharedFrameInfo {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: u32,
    pub offset: u64,
    pub modifier: u64,
    pub size: u64,
    pub token: u64,
}

/// Owned DMA-buf and producer-fence descriptors, with exclusive access to the
/// associated Device until release. Lease validity is bounded to one second;
/// expiry/revocation makes [`Self::finish`] fail and the copied frame unusable.
/// After revocation, source protection may persist through GPU import teardown.
pub struct SharedFrameLease<'a> {
    device: &'a mut Device,
    dmabuf: OwnedFd,
    ready_fence: OwnedFd,
    info: SharedFrameInfo,
    active: bool,
}

impl SharedFrameLease<'_> {
    pub fn info(&self) -> SharedFrameInfo {
        self.info
    }
    pub fn dmabuf(&self) -> BorrowedFd<'_> {
        self.dmabuf.as_fd()
    }
    /// Always present, including a signaled sync_file when there were no writers.
    pub fn ready_fence(&self) -> BorrowedFd<'_> {
        self.ready_fence.as_fd()
    }

    /// Release only after all GPU reads/copies have completed. Any failure means
    /// the snapshot must be discarded and the shared capture session aborted.
    pub fn finish(mut self) -> Result<(), Error> {
        release(self.device, self.info.token)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for SharedFrameLease<'_> {
    fn drop(&mut self) {
        if self.active {
            // Recovery for early returns/panics. The normal path explicitly
            // finishes, so revocation cannot be mistaken for a valid snapshot.
            let _ = release(self.device, self.info.token);
            self.active = false;
        }
    }
}

impl Device {
    /// Select shared capture before CONNECT/mode selection, without registering
    /// a userspace framebuffer or allocating a full-screen RGB/XRGB vector.
    pub fn enable_shared_capture(&mut self) -> Result<(), Error> {
        if self.mode.is_some()
            || self.update_pending
            || self.pending_buffer.is_some()
            || self.buffer.is_some()
            || !self.external_buffers.is_empty()
        {
            return Err(Error::ExternalBuffer);
        }
        self.external_mode = true;
        Ok(())
    }

    /// Request the next shared framebuffer without invoking GRABPIX. EAGAIN from
    /// ACQUIRE retains the ready notification, allowing a later call to retry
    /// without issuing another compositor update request or losing its ACK.
    pub fn try_next_shared_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<SharedFrameLease<'_>>, Error> {
        if !self.external_mode || !self.external_buffers.is_empty() {
            return Err(Error::ExternalBuffer);
        }
        self.poll_events(Duration::ZERO);
        let Some(mode) = self.mode else {
            return Ok(None);
        };
        if mode.bits_per_pixel != 32 || mode.pixel_format != XRGB8888 {
            return Err(Error::ExternalBuffer);
        }
        if self.take_ready() {
            return self.acquire_shared(mode);
        }
        if !self.update_pending {
            // REQUEST_UPDATE carries an event cookie only; ACQUIRE consumes the
            // pending compositor frame directly, with no registered destination.
            let ready = unsafe { crate::request_update_checked(self.handle.as_ptr(), BUFFER_ID) }
                .map_err(Error::CaptureIoctl)?;
            self.update_pending = true;
            if ready {
                return self.acquire_shared(mode);
            }
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.poll_events(remaining);
            // Mode changes revoke the old update. Request a fresh one next time.
            if !self.update_pending {
                return Ok(None);
            }
            if self.take_ready() {
                return self.acquire_shared(self.mode.ok_or(Error::ExternalBuffer)?);
            }
        }
    }

    fn acquire_shared(&mut self, mode: Mode) -> Result<Option<SharedFrameLease<'_>>, Error> {
        let mut output = AcquireFrame::default();
        let result = unsafe {
            shared_ioctl(
                self.handle.as_ref().fd,
                ACQUIRE,
                (&mut output as *mut AcquireFrame).cast(),
            )
        };
        if let Err(errno) = result {
            // The kernel normally publishes no descriptors on failure. Still
            // close/release any returned resources defensively before retrying.
            cleanup_output(self, &mut output)?;
            if errno == ffi::EAGAIN {
                self.update_pending = true;
                *self
                    .state
                    .ready_buffer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(BUFFER_ID);
                return Ok(None);
            }
            return Err(Error::CaptureIoctl(errno));
        }
        let info = match validate_info(&output, mode) {
            Ok(info) => info,
            Err(error) => {
                cleanup_output(self, &mut output)?;
                return Err(error);
            }
        };
        let fds = match take_descriptors(&mut output) {
            Ok(fds) => fds,
            Err(error) => {
                cleanup_output(self, &mut output)?;
                return Err(error);
            }
        };
        self.update_pending = false;
        self.pending_buffer = None;
        Ok(Some(SharedFrameLease {
            device: self,
            dmabuf: fds.0,
            ready_fence: fds.1,
            info,
            active: true,
        }))
    }
}

fn validate_info(frame: &AcquireFrame, mode: Mode) -> Result<SharedFrameInfo, Error> {
    let row = u64::from(frame.width)
        .checked_mul(4)
        .ok_or(Error::ExternalBuffer)?;
    let last_row = u64::from(frame.height.checked_sub(1).ok_or(Error::ExternalBuffer)?)
        .checked_mul(u64::from(frame.pitch))
        .ok_or(Error::ExternalBuffer)?;
    let end = frame
        .offset
        .checked_add(last_row)
        .and_then(|n| n.checked_add(row))
        .ok_or(Error::ExternalBuffer)?;
    if frame.version != VERSION
        || frame.flags != 0
        || frame.reserved != [0; 2]
        || frame.token == 0
        || frame.width == 0
        || frame.height == 0
        || frame.width as usize != mode.width
        || frame.height as usize != mode.height
        || frame.format != XRGB8888
        || frame.modifier != 0
        || u64::from(frame.pitch) < row
        || frame.pitch % 4 != 0
        || frame.offset % 4 != 0
        || end > frame.size
        || frame.size > isize::MAX as u64
    {
        return Err(Error::ExternalBuffer);
    }
    Ok(SharedFrameInfo {
        width: frame.width as usize,
        height: frame.height as usize,
        stride: frame.pitch as usize,
        format: frame.format,
        offset: frame.offset,
        modifier: frame.modifier,
        size: frame.size,
        token: frame.token,
    })
}

fn take_descriptors(frame: &mut AcquireFrame) -> Result<(OwnedFd, OwnedFd), Error> {
    if frame.dmabuf_fd < 0 || frame.ready_fence_fd < 0 || frame.dmabuf_fd == frame.ready_fence_fd {
        return Err(Error::ExternalBuffer);
    }
    for fd in [frame.dmabuf_fd, frame.ready_fence_fd] {
        // Check validity and close-on-exec before constructing Rust's OwnedFd.
        let flags = unsafe { fcntl(fd, F_GETFD) };
        if flags < 0 || flags & FD_CLOEXEC == 0 {
            return Err(Error::ExternalBuffer);
        }
    }
    let dmabuf = std::mem::replace(&mut frame.dmabuf_fd, -1);
    let ready = std::mem::replace(&mut frame.ready_fence_fd, -1);
    // SAFETY: ACQUIRE transferred two distinct, open descriptors; their raw
    // fields are invalidated so error cleanup cannot close them a second time.
    Ok(unsafe { (OwnedFd::from_raw_fd(dmabuf), OwnedFd::from_raw_fd(ready)) })
}

fn cleanup_output(device: &mut Device, frame: &mut AcquireFrame) -> Result<(), Error> {
    // Keep fds owned until after release, deduplicating even malformed output.
    let mut descriptors = Vec::with_capacity(2);
    let raw = [
        std::mem::replace(&mut frame.dmabuf_fd, -1),
        std::mem::replace(&mut frame.ready_fence_fd, -1),
    ];
    for (index, fd) in raw.into_iter().enumerate() {
        if fd >= 0 && (index == 0 || fd != raw[0]) && unsafe { fcntl(fd, F_GETFD) } >= 0 {
            // SAFETY: still owned by this successful/partially returned output,
            // confirmed open and adopted once even when the kernel duplicated it.
            descriptors.push(unsafe { OwnedFd::from_raw_fd(fd) });
        }
    }
    let token = std::mem::replace(&mut frame.token, 0);
    let result = if token != 0 {
        release(device, token)
    } else {
        Ok(())
    };
    drop(descriptors);
    result
}

fn release(device: &mut Device, token: u64) -> Result<(), Error> {
    let mut request = ReleaseFrame {
        token,
        flags: 0,
        reserved: 0,
    };
    unsafe {
        shared_ioctl(
            device.handle.as_ref().fd,
            RELEASE,
            (&mut request as *mut ReleaseFrame).cast(),
        )
    }
    .map_err(Error::CaptureIoctl)
}

/// Unlike the generic DRM helper, EAGAIN must escape to the capture loop rather
/// than busy-spinning indefinitely while a previous producer fence is pending.
unsafe fn shared_ioctl(fd: c_int, request: c_ulong, argument: *mut c_void) -> Result<(), c_int> {
    #[cfg(test)]
    if let Some(result) = IOCTL_MOCK.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .map(|mock| mock(fd, request, argument))
    }) {
        return result;
    }
    loop {
        if ffi::ioctl(fd, request, argument) >= 0 {
            return Ok(());
        }
        let errno = ffi::errno();
        if errno != ffi::EINTR {
            return Err(errno);
        }
    }
}

#[cfg(test)]
type IoctlMock = Box<dyn FnMut(c_int, c_ulong, *mut c_void) -> Result<(), c_int>>;
#[cfg(test)]
thread_local! {
    static IOCTL_MOCK: std::cell::RefCell<Option<IoctlMock>> = std::cell::RefCell::new(None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safe::EventState;
    use std::{
        cell::{Cell, RefCell},
        io::{ErrorKind, Read},
        os::{fd::IntoRawFd, unix::net::UnixStream},
        ptr::NonNull,
        rc::Rc,
    };

    struct MockGuard;
    impl Drop for MockGuard {
        fn drop(&mut self) {
            IOCTL_MOCK.with(|slot| *slot.borrow_mut() = None);
        }
    }
    fn mock(
        f: impl FnMut(c_int, c_ulong, *mut c_void) -> Result<(), c_int> + 'static,
    ) -> MockGuard {
        IOCTL_MOCK.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(f));
        });
        MockGuard
    }
    fn mode() -> Mode {
        Mode {
            width: 128,
            height: 32,
            refresh_hz: 60,
            bits_per_pixel: 32,
            pixel_format: XRGB8888,
        }
    }
    fn device() -> Device {
        let context = Box::new(crate::evdi_device_context {
            fd: -1,
            buffer_to_update: 0,
            device_index: -1,
            cursor_events_enabled: Cell::new(true),
            frame_buffers: vec![],
        });
        Device {
            handle: NonNull::new(Box::into_raw(context)).unwrap(),
            state: Box::<EventState>::default(),
            buffer: None,
            mode: None,
            update_pending: false,
            external_buffers: vec![],
            external_mode: false,
            pending_buffer: None,
        }
    }
    fn output() -> AcquireFrame {
        AcquireFrame {
            width: 128,
            height: 32,
            pitch: 512,
            format: XRGB8888,
            size: 512 * 32,
            token: 42,
            ..Default::default()
        }
    }
    fn descriptors() -> ((i32, i32), (UnixStream, UnixStream)) {
        let (a, pa) = UnixStream::pair().unwrap();
        let (b, pb) = UnixStream::pair().unwrap();
        pa.set_nonblocking(true).unwrap();
        pb.set_nonblocking(true).unwrap();
        ((a.into_raw_fd(), b.into_raw_fd()), (pa, pb))
    }
    fn open(peer: &mut UnixStream) -> bool {
        match peer.read(&mut [0]) {
            Err(error) if error.kind() == ErrorKind::WouldBlock => true,
            Ok(0) => false,
            result => panic!("unexpected descriptor peer state: {result:?}"),
        }
    }
    fn request_mock(counter: Rc<Cell<usize>>, ready: bool) -> ffi::MockIoctlGuard {
        ffi::mock_ioctl(move |_, request, _| {
            if request == crate::uapi::ioctl_request_update() {
                counter.set(counter.get() + 1);
                Some(i32::from(ready))
            } else {
                Some(0)
            }
        })
    }

    #[test]
    fn shared_capture_abi_matches_fixed_kernel_layout() {
        assert_eq!(std::mem::size_of::<AcquireFrame>(), 80);
        assert_eq!(std::mem::offset_of!(AcquireFrame, dmabuf_fd), 8);
        assert_eq!(std::mem::offset_of!(AcquireFrame, width), 16);
        assert_eq!(std::mem::offset_of!(AcquireFrame, offset), 32);
        assert_eq!(std::mem::offset_of!(AcquireFrame, token), 56);
        assert_eq!(std::mem::offset_of!(AcquireFrame, reserved), 64);
        assert_eq!(std::mem::size_of::<ReleaseFrame>(), 16);
        assert_eq!(ACQUIRE, 0xc0506445);
        assert_eq!(RELEASE, 0xc0106446);
        let input = AcquireFrame::default();
        assert_eq!(
            (
                input.version,
                input.flags,
                input.dmabuf_fd,
                input.ready_fence_fd
            ),
            (1, 0, -1, -1)
        );
    }

    #[test]
    fn shared_layout_rejects_unsupported_and_out_of_bounds_metadata() {
        let good = output();
        assert!(validate_info(&good, mode()).is_ok());
        for alter in [
            |f: &mut AcquireFrame| f.version = 2,
            |f: &mut AcquireFrame| f.flags = 1,
            |f: &mut AcquireFrame| f.reserved[1] = 1,
            |f: &mut AcquireFrame| f.token = 0,
            |f: &mut AcquireFrame| f.width = 0,
            |f: &mut AcquireFrame| f.width = 64,
            |f: &mut AcquireFrame| f.height = 0,
            |f: &mut AcquireFrame| f.pitch = 508,
            |f: &mut AcquireFrame| f.pitch = 513,
            |f: &mut AcquireFrame| f.offset = 1,
            |f: &mut AcquireFrame| f.offset = u64::MAX - 3,
            |f: &mut AcquireFrame| f.size -= 1,
            |f: &mut AcquireFrame| f.size = u64::MAX,
            |f: &mut AcquireFrame| f.format = u32::from_le_bytes(*b"AR24"),
            |f: &mut AcquireFrame| f.modifier = 1,
        ] {
            let mut bad = good;
            alter(&mut bad);
            assert!(validate_info(&bad, mode()).is_err());
        }
        let mut prefixed = good;
        prefixed.offset = 4096;
        prefixed.size += 4096;
        assert_eq!(validate_info(&prefixed, mode()).unwrap().offset, 4096);
    }

    #[test]
    fn eagain_retains_ready_update_and_finish_releases_once() {
        let requests = Rc::new(Cell::new(0));
        let _requests = request_mock(requests.clone(), true);
        let ((dma, fence), (mut pdma, mut pfence)) = descriptors();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let seen = calls.clone();
        let mut acquire_count = 0;
        let _shared = mock(move |_, request, arg| {
            seen.borrow_mut().push(request);
            if request == ACQUIRE {
                acquire_count += 1;
                if acquire_count == 1 {
                    return Err(ffi::EAGAIN);
                }
                let mut answer = output();
                answer.dmabuf_fd = dma;
                answer.ready_fence_fd = fence;
                unsafe {
                    *(arg as *mut AcquireFrame) = answer;
                }
            } else {
                let release = unsafe { &*(arg as *const ReleaseFrame) };
                assert_eq!((release.token, release.flags, release.reserved), (42, 0, 0));
            }
            Ok(())
        });
        let mut device = device();
        device.enable_shared_capture().unwrap();
        device.register_mode(mode());
        assert!(device.buffer.is_none());
        assert!(device.external_buffers.is_empty());
        assert!(device
            .try_next_shared_frame(Duration::ZERO)
            .unwrap()
            .is_none());
        assert!(device.update_pending);
        assert_eq!(requests.get(), 1);
        assert_eq!(*device.state.ready_buffer.lock().unwrap(), Some(BUFFER_ID));
        let lease = device
            .try_next_shared_frame(Duration::ZERO)
            .unwrap()
            .unwrap();
        assert_eq!(lease.info().token, 42);
        assert!(open(&mut pdma));
        assert!(open(&mut pfence));
        lease.finish().unwrap();
        assert!(!open(&mut pdma));
        assert!(!open(&mut pfence));
        assert_eq!(requests.get(), 1);
        assert!(!device.update_pending);
        assert_eq!(*calls.borrow(), vec![ACQUIRE, ACQUIRE, RELEASE]);
    }

    #[test]
    fn delayed_ready_survives_timeout_and_drop_releases() {
        let requests = Rc::new(Cell::new(0));
        let _requests = request_mock(requests.clone(), false);
        let ((dma, fence), (mut pdma, mut pfence)) = descriptors();
        let releases = Rc::new(Cell::new(0));
        let seen = releases.clone();
        let _shared = mock(move |_, request, arg| {
            if request == ACQUIRE {
                let mut answer = output();
                answer.dmabuf_fd = dma;
                answer.ready_fence_fd = fence;
                unsafe {
                    *(arg as *mut AcquireFrame) = answer;
                }
            } else {
                seen.set(seen.get() + 1);
            }
            Ok(())
        });
        let mut device = device();
        device.enable_shared_capture().unwrap();
        device.register_mode(mode());
        assert!(device
            .try_next_shared_frame(Duration::ZERO)
            .unwrap()
            .is_none());
        assert!(device
            .try_next_shared_frame(Duration::ZERO)
            .unwrap()
            .is_none());
        assert_eq!(requests.get(), 1);
        *device.state.ready_buffer.lock().unwrap() = Some(BUFFER_ID);
        drop(
            device
                .try_next_shared_frame(Duration::ZERO)
                .unwrap()
                .unwrap(),
        );
        assert_eq!(requests.get(), 1);
        assert_eq!(releases.get(), 1);
        assert!(!open(&mut pdma));
        assert!(!open(&mut pfence));
    }

    #[test]
    fn invalid_output_releases_guard_and_closes_both_fds() {
        let requests = Rc::new(Cell::new(0));
        let _requests = request_mock(requests, true);
        let ((dma, fence), (mut pdma, mut pfence)) = descriptors();
        let releases = Rc::new(Cell::new(0));
        let seen = releases.clone();
        let _shared = mock(move |_, request, arg| {
            if request == ACQUIRE {
                let mut answer = output();
                answer.dmabuf_fd = dma;
                answer.ready_fence_fd = fence;
                answer.modifier = 3;
                unsafe {
                    *(arg as *mut AcquireFrame) = answer;
                }
            } else {
                seen.set(seen.get() + 1);
            }
            Ok(())
        });
        let mut device = device();
        device.enable_shared_capture().unwrap();
        device.register_mode(mode());
        assert!(matches!(
            device.try_next_shared_frame(Duration::ZERO),
            Err(Error::ExternalBuffer)
        ));
        assert_eq!(releases.get(), 1);
        assert!(!open(&mut pdma));
        assert!(!open(&mut pfence));
    }

    #[test]
    fn release_failure_is_reported_and_drop_retries_before_closing() {
        let requests = Rc::new(Cell::new(0));
        let _requests = request_mock(requests, true);
        let ((dma, fence), (mut pdma, mut pfence)) = descriptors();
        let releases = Rc::new(Cell::new(0));
        let seen = releases.clone();
        let _shared = mock(move |_, request, arg| {
            if request == ACQUIRE {
                let mut answer = output();
                answer.dmabuf_fd = dma;
                answer.ready_fence_fd = fence;
                unsafe {
                    *(arg as *mut AcquireFrame) = answer;
                }
                Ok(())
            } else {
                seen.set(seen.get() + 1);
                Err(116)
            }
        });
        let mut device = device();
        device.enable_shared_capture().unwrap();
        device.register_mode(mode());
        let lease = device
            .try_next_shared_frame(Duration::ZERO)
            .unwrap()
            .unwrap();
        assert_eq!(lease.finish(), Err(Error::CaptureIoctl(116)));
        assert_eq!(releases.get(), 2);
        assert!(!open(&mut pdma));
        assert!(!open(&mut pfence));
    }

    #[test]
    fn malformed_descriptors_are_closed_once_and_guard_is_released() {
        let requests = Rc::new(Cell::new(0));
        let _requests = request_mock(requests, true);
        for case in 0..3 {
            let ((dma, fence), (mut pdma, mut pfence)) = descriptors();
            // The duplicate/missing cases return only the DMA descriptor. Own
            // the unused test fence separately so malformed-output cleanup has
            // no authority over it.
            let unused = if case < 2 {
                Some(unsafe { OwnedFd::from_raw_fd(fence) })
            } else {
                // Model a broken kernel that omitted O_CLOEXEC.
                assert_eq!(unsafe { fcntl(fence, 2, 0) }, 0);
                None
            };
            let releases = Rc::new(Cell::new(0));
            let seen = releases.clone();
            let _shared = mock(move |_, request, arg| {
                if request == ACQUIRE {
                    let mut answer = output();
                    answer.dmabuf_fd = dma;
                    answer.ready_fence_fd = match case {
                        0 => -1,
                        1 => dma,
                        _ => fence,
                    };
                    unsafe {
                        *(arg as *mut AcquireFrame) = answer;
                    }
                } else {
                    seen.set(seen.get() + 1);
                }
                Ok(())
            });
            let mut device = device();
            device.enable_shared_capture().unwrap();
            device.register_mode(mode());
            assert!(matches!(
                device.try_next_shared_frame(Duration::ZERO),
                Err(Error::ExternalBuffer)
            ));
            assert_eq!(releases.get(), 1);
            assert!(!open(&mut pdma));
            assert_eq!(open(&mut pfence), unused.is_some());
        }
    }
}
