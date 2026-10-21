// SPDX-License-Identifier: LGPL-2.1-or-later
// Copyright (c) 2026 revdi contributors

//! Owned Rust interface to Revdi.
//!
//! The C ABI remains available for DisplayLinkManager compatibility. Rust clients should use this
//! module so device handles, callback storage, registered framebuffer memory, and teardown stay
//! tied to one lifetime.

use super::{
    evdi_add_device, evdi_buffer, evdi_check_device, evdi_close, evdi_connect2, evdi_cursor_move,
    evdi_cursor_set, evdi_ddcci_data, evdi_ddcci_response, evdi_device_context, evdi_device_status,
    evdi_disconnect, evdi_enable_cursor_events, evdi_event_context, evdi_get_event_ready,
    evdi_handle_events, evdi_mode, evdi_open, evdi_rect, evdi_register_buffer,
    evdi_unregister_buffer,
};
use crate::ffi;
use core::ffi::{c_int, c_void};
use core::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub mod shared_capture;

const BUFFER_ID: c_int = 1;
/// How long to wait for a freshly added card's node to appear, and how often to look.
const ADD_SETTLE_POLLS: usize = 50;
const ADD_SETTLE_DELAY: Duration = Duration::from_millis(20);

const MAX_DEVICE_INDEX: c_int = 64;
const MAX_DAMAGE_RECTS: usize = 16;

/// Errors produced before a Revdi device can be used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AddDevice,
    NoAvailableDevice,
    OpenDevice(c_int),
    InvalidEdidLength,
    CaptureIoctl(c_int),
    ExternalBuffer,
    PendingBuffer,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::CaptureIoctl(errno) => write!(
                f,
                "EVDI capture ioctl failed: {}",
                std::io::Error::from_raw_os_error(*errno)
            ),
            Self::AddDevice => write!(f, "failed to add a Revdi device"),
            Self::NoAvailableDevice => write!(f, "no available Revdi device"),
            Self::OpenDevice(index) => write!(f, "failed to open Revdi device {index}"),
            Self::InvalidEdidLength => write!(f, "EDID is too large for the Revdi ABI"),
            Self::ExternalBuffer => write!(f, "invalid external capture buffer or mode"),
            Self::PendingBuffer => write!(f, "cannot change the destination of a pending update"),
        }
    }
}

impl std::error::Error for Error {}

/// A compositor-selected scanout mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mode {
    pub width: usize,
    pub height: usize,
    pub refresh_hz: u32,
    pub bits_per_pixel: u32,
    pub pixel_format: u32,
}

impl Mode {
    fn from_abi(mode: evdi_mode) -> Option<Self> {
        Some(Self {
            width: usize::try_from(mode.width).ok().filter(|&v| v != 0)?,
            height: usize::try_from(mode.height).ok().filter(|&v| v != 0)?,
            refresh_hz: u32::try_from(mode.refresh_rate).ok()?,
            bits_per_pixel: u32::try_from(mode.bits_per_pixel).ok()?,
            pixel_format: mode.pixel_format,
        })
    }
}

/// One damaged rectangle, using exclusive right and bottom coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DamageRect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

/// An owned hardware-cursor update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cursor {
    pub hot_x: i32,
    pub hot_y: i32,
    pub width: u32,
    pub height: u32,
    pub enabled: bool,
    pub pixels: Vec<u8>,
    pub pixel_format: u32,
    pub stride: u32,
}

/// An owned cursor position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorPosition {
    pub x: i32,
    pub y: i32,
}

/// One DDC/CI transaction requested by the virtual I2C adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DdcciRequest {
    pub address: u16,
    pub flags: u16,
    pub buffer: Vec<u8>,
}

/// A non-frame event delivered by Revdi.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceEvent {
    Dpms(i32),
    Mode(Mode),
    CrtcState(i32),
    CursorSet(Cursor),
    CursorMove(CursorPosition),
    Ddcci(DdcciRequest),
}

/// A coherent XRGB8888 framebuffer snapshot borrowed from a [`Device`].
pub struct Frame<'a> {
    pub pixels: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub damage: Vec<DamageRect>,
}

/// An externally allocated destination with ownership retained through registration.
/// Pixel access still requires an exclusive capture lease at each grab.
pub struct ExternalBuffer {
    id: c_int,
    pointer: NonNull<u8>,
    width: usize,
    height: usize,
    stride: usize,
    _owner: Arc<dyn Send + Sync>,
}

impl ExternalBuffer {
    /// # Safety
    /// `owner` must retain this stable allocation, and `pointer` must address at least `length`
    /// initialized writable bytes. Registration grants no pixel access. A caller holding an
    /// exclusive capture lease may grab/repair pixels; all other CPU/GPU access requires an
    /// immutable frame lease and must finish before the next exclusive reservation.
    pub unsafe fn new(
        id: c_int,
        pointer: *mut u8,
        length: usize,
        geometry: (usize, usize, usize),
        owner: Arc<dyn Send + Sync>,
    ) -> Result<Self, Error> {
        let (width, height, stride) = geometry;
        let pointer = NonNull::new(pointer).ok_or(Error::ExternalBuffer)?;
        if width == 0
            || height == 0
            || width.checked_mul(4).map_or(true, |n| n > stride)
            || stride.checked_mul(height).map_or(true, |n| n > length)
            || [width, height, stride]
                .iter()
                .any(|&n| n > c_int::MAX as usize)
        {
            return Err(Error::ExternalBuffer);
        }
        Ok(Self {
            id,
            pointer,
            width,
            height,
            stride,
            _owner: owner,
        })
    }
}

/// Completion metadata for an externally owned destination; no borrowed pixel alias is created.
pub struct ExternalFrame {
    pub buffer_id: c_int,
    pub damage: Vec<DamageRect>,
}

#[derive(Default)]
struct EventState {
    mode: Mutex<Option<evdi_mode>>,
    ready_buffer: Mutex<Option<c_int>>,
    events: Mutex<Vec<DeviceEvent>>,
}

fn with_event_state(user_data: *mut c_void, f: impl FnOnce(&EventState)) {
    // SAFETY: `Device::event_context` supplies its boxed `EventState`, and
    // `evdi_handle_events` invokes callbacks synchronously while the device is
    // exclusively borrowed.
    f(unsafe { &*(user_data.cast::<EventState>()) });
}

extern "C" fn dpms_changed(mode: c_int, user_data: *mut c_void) {
    with_event_state(user_data, |state| {
        state
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(DeviceEvent::Dpms(mode));
    });
}

extern "C" fn mode_changed(mode: evdi_mode, user_data: *mut c_void) {
    with_event_state(user_data, |state| {
        *state.mode.lock().unwrap_or_else(|e| e.into_inner()) = Some(mode);
        if let Some(mode) = Mode::from_abi(mode) {
            state
                .events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(DeviceEvent::Mode(mode));
        }
    });
}

extern "C" fn update_ready(buffer_id: c_int, user_data: *mut c_void) {
    with_event_state(user_data, |state| {
        *state.ready_buffer.lock().unwrap_or_else(|e| e.into_inner()) = Some(buffer_id);
    });
}

extern "C" fn crtc_state_changed(state_value: c_int, user_data: *mut c_void) {
    with_event_state(user_data, |state| {
        state
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(DeviceEvent::CrtcState(state_value));
    });
}

extern "C" fn cursor_set(cursor: evdi_cursor_set, user_data: *mut c_void) {
    let length = usize::try_from(cursor.buffer_length).unwrap_or(0);
    let pixels = if cursor.buffer.is_null() || length == 0 {
        Vec::new()
    } else {
        // SAFETY: libevdi allocates this callback buffer with `malloc`, supplies
        // exactly `buffer_length` initialized bytes, and transfers ownership to
        // the callback.
        let bytes =
            unsafe { core::slice::from_raw_parts(cursor.buffer.cast::<u8>(), length).to_vec() };
        // SAFETY: the pointer came from `malloc` in `to_evdi_cursor_set` and is
        // released exactly once after copying.
        unsafe { ffi::free(cursor.buffer.cast()) };
        bytes
    };
    with_event_state(user_data, |state| {
        state
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(DeviceEvent::CursorSet(Cursor {
                hot_x: cursor.hot_x,
                hot_y: cursor.hot_y,
                width: cursor.width,
                height: cursor.height,
                enabled: cursor.enabled != 0,
                pixels,
                pixel_format: cursor.pixel_format,
                stride: cursor.stride,
            }));
    });
}

extern "C" fn cursor_move(cursor: evdi_cursor_move, user_data: *mut c_void) {
    with_event_state(user_data, |state| {
        state
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(DeviceEvent::CursorMove(CursorPosition {
                x: cursor.x,
                y: cursor.y,
            }));
    });
}

extern "C" fn ddcci_data(request: evdi_ddcci_data, user_data: *mut c_void) {
    let length = usize::try_from(request.buffer_length).unwrap_or(0);
    let buffer = if request.buffer.is_null() || length == 0 {
        Vec::new()
    } else {
        // SAFETY: the pointer is borrowed from the complete DRM event for this
        // synchronous callback and names `buffer_length` initialized bytes.
        unsafe { core::slice::from_raw_parts(request.buffer, length).to_vec() }
    };
    with_event_state(user_data, |state| {
        state
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(DeviceEvent::Ddcci(DdcciRequest {
                address: request.address,
                flags: request.flags,
                buffer,
            }));
    });
}

struct RegisteredBuffer {
    pixels: Vec<u8>,
    width: usize,
    height: usize,
    stride: usize,
}

/// An exclusively owned Revdi device and its registered scanout storage.
pub struct Device {
    handle: NonNull<evdi_device_context>,
    state: Box<EventState>,
    buffer: Option<RegisteredBuffer>,
    mode: Option<Mode>,
    update_pending: bool,
    external_buffers: Vec<ExternalBuffer>,
    external_mode: bool,
    pending_buffer: Option<c_int>,
}

/// Every Revdi card index the module currently exposes.
///
/// `evdi_check_device` reports whether a card is one of ours, not whether anybody is driving it, so
/// this is a census and never an availability test.
fn present_cards() -> Vec<i32> {
    (0..MAX_DEVICE_INDEX)
        .filter(|&index| evdi_check_device(index) == evdi_device_status::AVAILABLE)
        .collect()
}

impl Device {
    /// Open the first Revdi card, creating one through sysfs if necessary.
    ///
    /// A client driving more than one output wants [`Self::open_nth`] instead.
    pub fn open() -> Result<Self, Error> {
        Self::open_nth(0)
    }

    /// Open the `nth` Revdi card, creating cards until there is one.
    ///
    /// A client driving several outputs needs one card per output, and cannot get them from
    /// [`Self::open`]: the ABI can only be asked which cards exist, never which are free, so that
    /// returns the same card to every caller. Asking by position gives each output a card of its
    /// own and gives it the same one again after a reconnect, which matters because nothing removes
    /// a single card -- a client that added one per attempt would accumulate them.
    pub fn open_nth(nth: usize) -> Result<Self, Error> {
        for _ in 0..ADD_SETTLE_POLLS {
            if let Some(&index) = present_cards().get(nth) {
                return Self::open_index(index);
            }
            if evdi_add_device() < 0 {
                return Err(Error::AddDevice);
            }
            // The card node appears once the module has registered it, which is not synchronous
            // with the sysfs write that asked for it.
            std::thread::sleep(ADD_SETTLE_DELAY);
        }
        Err(Error::NoAvailableDevice)
    }

    fn open_index(index: i32) -> Result<Self, Error> {
        Self::open_existing(index, true)
    }

    /// Open one existing EVDI card with its cursor policy set before CONNECT.
    /// This never adds cards or detaches another client's handle.
    pub fn open_existing(index: i32, cursor_events: bool) -> Result<Self, Error> {
        if !(0..MAX_DEVICE_INDEX).contains(&index)
            || evdi_check_device(index) != evdi_device_status::AVAILABLE
        {
            return Err(Error::OpenDevice(index));
        }
        let handle = NonNull::new(evdi_open(index)).ok_or(Error::OpenDevice(index))?;
        let device = Self {
            handle,
            state: Box::default(),
            buffer: None,
            mode: None,
            update_pending: false,
            external_buffers: Vec::new(),
            external_mode: false,
            pending_buffer: None,
        };
        // SAFETY: the owned handle remains live until `Device::drop`.
        unsafe { evdi_enable_cursor_events(device.handle.as_ptr(), cursor_events) };
        Ok(device)
    }

    /// Ask for -- or stop asking for -- cursor shape and position out of band.
    ///
    /// A client that receives them is expected to draw the pointer itself, because the compositor
    /// keeps it out of the framebuffer once its cursor plane commits. A client that cannot draw it
    /// must turn them off here, or the pointer disappears entirely rather than merely lagging.
    pub fn set_cursor_events(&mut self, enable: bool) {
        // SAFETY: the owned handle remains live until `Device::drop`.
        unsafe { evdi_enable_cursor_events(self.handle.as_ptr(), enable) };
    }

    /// Whether this handle successfully enabled cursor events without falling back after a
    /// bitmap-mapping failure. Clients requiring separate composition must check after polling.
    pub fn cursor_events_enabled(&self) -> bool {
        // SAFETY: this device owns the live context, and event handling is synchronous.
        unsafe { self.handle.as_ref().cursor_events_enabled.get() }
    }

    /// Advertise a connected monitor using its EDID and explicit scanout limits.
    pub fn connect(
        &mut self,
        edid: &[u8],
        pixel_area_limit: u32,
        pixel_per_second_limit: u32,
    ) -> Result<(), Error> {
        let edid_len = u32::try_from(edid.len()).map_err(|_| Error::InvalidEdidLength)?;
        // SAFETY: the owned handle is live and `edid` remains valid for the synchronous ioctl.
        unsafe {
            evdi_connect2(
                self.handle.as_ptr(),
                edid.as_ptr(),
                edid_len,
                pixel_area_limit,
                pixel_per_second_limit,
            );
        }
        Ok(())
    }

    /// The last mode selected by the compositor.
    pub fn mode(&self) -> Option<Mode> {
        self.mode
    }

    /// Wait for a compositor to select a usable mode.
    pub fn wait_for_mode(&mut self, timeout: Duration) -> Option<Mode> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(mode) = self.mode {
                return Some(mode);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            self.poll_events(remaining.min(Duration::from_millis(200)));
        }
    }

    /// Request and return the next compositor update.
    ///
    /// The returned frame borrows the registered framebuffer, preventing a mode change or
    /// re-registration until the caller has finished reading it.
    pub fn next_frame(&mut self, timeout: Duration) -> Option<Frame<'_>> {
        self.try_next_frame(timeout).ok().flatten()
    }

    /// Checked capture for clients that must stop on device errors rather than treat them as idle.
    pub fn try_next_frame(&mut self, timeout: Duration) -> Result<Option<Frame<'_>>, Error> {
        if self.external_mode {
            return Err(Error::ExternalBuffer);
        }
        self.poll_events(Duration::ZERO);
        if self.buffer.is_none() {
            return Ok(None);
        }
        // A reply can arrive after an earlier call's timeout. Consume it before asking again.
        if self.take_ready() {
            self.update_pending = false;
            return self.grab_frame();
        }
        if !self.update_pending {
            // SAFETY: BUFFER_ID is registered for this owned handle.
            if unsafe { super::request_update_checked(self.handle.as_ptr(), BUFFER_ID) }
                .map_err(Error::CaptureIoctl)?
            {
                return self.grab_frame();
            }
            self.update_pending = true;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.poll_events(remaining);
            if self.take_ready() {
                self.update_pending = false;
                return self.grab_frame();
            }
            // A mode event invalidates the registered buffer and its outstanding request.
            if !self.update_pending {
                return Ok(None);
            }
        }
    }

    /// Select owned external storage instead of the default single-vector capture path.
    /// Registration itself grants no permission to modify a destination.
    pub fn register_external_buffers(&mut self, buffers: Vec<ExternalBuffer>) -> Result<(), Error> {
        if buffers.is_empty() || self.update_pending || self.mode.is_some() {
            return Err(Error::ExternalBuffer);
        }
        for (i, buffer) in buffers.iter().enumerate() {
            if buffers[..i].iter().any(|old| old.id == buffer.id) {
                return Err(Error::ExternalBuffer);
            }
        }
        self.unregister_buffer();
        self.external_mode = true;
        self.external_buffers = buffers;
        for buffer in &self.external_buffers {
            let abi = evdi_buffer {
                id: buffer.id,
                buffer: buffer.pointer.as_ptr().cast(),
                width: buffer.width as c_int,
                height: buffer.height as c_int,
                stride: buffer.stride as c_int,
                rects: core::ptr::null_mut(),
                rect_count: 0,
            };
            // SAFETY: ExternalBuffer retains its validated allocation until unregister.
            unsafe { evdi_register_buffer(self.handle.as_ptr(), abi) };
        }
        Ok(())
    }

    /// Capture into one previously registered, exclusively reserved destination.
    ///
    /// # Safety
    /// The caller must exclusively own the pixels in `buffer_id` throughout this call and
    /// retain the same reservation across timeouts until completion or device teardown.
    /// No CPU/GPU reader may access this destination until the grab and repair are finished.
    pub unsafe fn try_next_external_frame(
        &mut self,
        buffer_id: c_int,
        timeout: Duration,
    ) -> Result<Option<ExternalFrame>, Error> {
        self.poll_events(Duration::ZERO);
        if !self.external_mode {
            return Err(Error::ExternalBuffer);
        }
        let Some(mode) = self.mode else {
            return Ok(None);
        };
        let buffer = self
            .external_buffers
            .iter()
            .find(|b| b.id == buffer_id)
            .ok_or(Error::ExternalBuffer)?;
        if (buffer.width, buffer.height) != (mode.width, mode.height) || mode.bits_per_pixel != 32 {
            return Err(Error::ExternalBuffer);
        }
        if self.pending_buffer.is_some_and(|id| id != buffer_id) {
            return Err(Error::PendingBuffer);
        }
        let take_ready = |this: &mut Self| {
            this.state
                .ready_buffer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                == Some(buffer_id)
        };
        if take_ready(self) {
            self.update_pending = false;
            self.pending_buffer = None;
            return self.grab_external(buffer_id);
        }
        if !self.update_pending {
            self.pending_buffer = Some(buffer_id);
            let ready = super::request_update_checked(self.handle.as_ptr(), buffer_id)
                .map_err(Error::CaptureIoctl)?;
            if ready {
                self.pending_buffer = None;
                return self.grab_external(buffer_id);
            }
            self.update_pending = true;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.poll_events(remaining);
            if !self.update_pending {
                return Ok(None);
            }
            if take_ready(self) {
                self.update_pending = false;
                self.pending_buffer = None;
                return self.grab_external(buffer_id);
            }
        }
    }

    fn grab_external(&mut self, buffer_id: c_int) -> Result<Option<ExternalFrame>, Error> {
        let mut rects = [evdi_rect {
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 0,
        }; MAX_DAMAGE_RECTS];
        let mut count = rects.len() as c_int;
        // SAFETY: callers retain the selected exclusive destination; ioctl is synchronous.
        unsafe {
            super::grab_pixels_checked(self.handle.as_ptr(), rects.as_mut_ptr(), &mut count)
                .map_err(Error::CaptureIoctl)?;
        }
        let count = usize::try_from(count).unwrap_or(0).min(rects.len());
        if count == 0 {
            return Ok(None);
        }
        Ok(Some(ExternalFrame {
            buffer_id,
            damage: rects[..count]
                .iter()
                .map(|r| DamageRect {
                    x1: r.x1,
                    y1: r.y1,
                    x2: r.x2,
                    y2: r.y2,
                })
                .collect(),
        }))
    }

    fn take_ready(&mut self) -> bool {
        self.state
            .ready_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            == Some(BUFFER_ID)
    }

    /// Poll and drain non-frame events into owned Rust values.
    pub fn events(&mut self, timeout: Duration) -> Vec<DeviceEvent> {
        self.poll_events(timeout);
        core::mem::take(&mut *self.state.events.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Complete a DDC/CI request previously returned as [`DeviceEvent::Ddcci`].
    pub fn ddcci_response(&mut self, response: &[u8], success: bool) {
        let Ok(length) = u32::try_from(response.len()) else {
            // The EVDI event protocol limits DDC/CI payloads to 64 bytes.
            return;
        };
        // SAFETY: the owned handle is live and `response` remains valid for the
        // synchronous ioctl.
        unsafe {
            evdi_ddcci_response(self.handle.as_ptr(), response.as_ptr(), length, success);
        }
    }

    fn grab_frame(&mut self) -> Result<Option<Frame<'_>>, Error> {
        let mut rects = [evdi_rect {
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 0,
        }; MAX_DAMAGE_RECTS];
        let mut count = rects.len() as c_int;
        // SAFETY: `rects` has `count` slots, and `BUFFER_ID` is registered while `buffer` is Some.
        unsafe {
            super::grab_pixels_checked(self.handle.as_ptr(), rects.as_mut_ptr(), &mut count)
                .map_err(Error::CaptureIoctl)?;
        }
        let count = usize::try_from(count)
            .ok()
            .map(|count| count.min(rects.len()))
            .unwrap_or(0);
        if count == 0 {
            return Ok(None);
        }
        let damage = rects[..count]
            .iter()
            .map(|r| DamageRect {
                x1: r.x1,
                y1: r.y1,
                x2: r.x2,
                y2: r.y2,
            })
            .collect();
        let Some(buffer) = self.buffer.as_ref() else {
            return Ok(None);
        };
        Ok(Some(Frame {
            pixels: &buffer.pixels,
            width: buffer.width,
            height: buffer.height,
            stride: buffer.stride,
            damage,
        }))
    }

    fn poll_events(&mut self, timeout: Duration) {
        // SAFETY: the owned handle remains live.
        let fd = unsafe { evdi_get_event_ready(self.handle.as_ptr()) };
        if !wait_readable(fd, timeout) {
            return;
        }
        let mut context = self.event_context();
        // SAFETY: `context` and its boxed callback state remain valid for this synchronous call.
        unsafe {
            evdi_handle_events(self.handle.as_ptr(), &mut context);
        }
        let selected = self
            .state
            .mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .and_then(Mode::from_abi);
        if let Some(mode) = selected {
            if self.mode != Some(mode) {
                self.register_mode(mode);
            }
        }
    }

    fn event_context(&self) -> evdi_event_context {
        evdi_event_context {
            dpms_handler: Some(dpms_changed),
            mode_changed_handler: Some(mode_changed),
            update_ready_handler: Some(update_ready),
            crtc_state_handler: Some(crtc_state_changed),
            cursor_set_handler: Some(cursor_set),
            cursor_move_handler: Some(cursor_move),
            ddcci_data_handler: Some(ddcci_data),
            user_data: (&*self.state as *const EventState).cast_mut().cast(),
        }
    }

    fn register_mode(&mut self, mode: Mode) {
        if self.external_mode {
            self.update_pending = false;
            self.pending_buffer = None;
            self.state
                .ready_buffer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            self.mode = Some(mode);
            return;
        }
        self.unregister_buffer();
        self.mode = None;
        let Some(stride) = mode.width.checked_mul(4) else {
            return;
        };
        let Some(length) = stride.checked_mul(mode.height) else {
            return;
        };
        let buffer = RegisteredBuffer {
            pixels: vec![0; length],
            width: mode.width,
            height: mode.height,
            stride,
        };
        let Some(width) = c_int::try_from(mode.width).ok() else {
            return;
        };
        let Some(height) = c_int::try_from(mode.height).ok() else {
            return;
        };
        let Some(stride) = c_int::try_from(stride).ok() else {
            return;
        };
        self.buffer = Some(buffer);
        let buffer = self.buffer.as_mut().unwrap();
        let abi_buffer = evdi_buffer {
            id: BUFFER_ID,
            buffer: buffer.pixels.as_mut_ptr().cast(),
            width,
            height,
            stride,
            rects: core::ptr::null_mut(),
            rect_count: 0,
        };
        // SAFETY: the vector allocation is stored in `self.buffer` before it can be observed by
        // event processing and is unregistered before it is ever replaced or dropped.
        unsafe {
            evdi_register_buffer(self.handle.as_ptr(), abi_buffer);
        }
        self.mode = Some(mode);
    }

    fn unregister_buffer(&mut self) {
        self.update_pending = false;
        self.pending_buffer = None;
        self.state
            .ready_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if self.buffer.is_some() {
            // SAFETY: `BUFFER_ID` is registered exactly while `self.buffer` is Some.
            unsafe {
                evdi_unregister_buffer(self.handle.as_ptr(), BUFFER_ID);
            }
            self.buffer = None;
        }
        for buffer in &self.external_buffers {
            // SAFETY: each external allocation is retained until after unregistering its ID.
            unsafe { evdi_unregister_buffer(self.handle.as_ptr(), buffer.id) };
        }
        self.external_buffers.clear();
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.unregister_buffer();
        // SAFETY: this object owns the live handle and drops it exactly once.
        unsafe {
            evdi_disconnect(self.handle.as_ptr());
            evdi_close(self.handle.as_ptr());
        }
    }
}

fn wait_readable(fd: c_int, timeout: Duration) -> bool {
    if fd < 0 {
        return false;
    }
    let mut poll_fd = ffi::PollFd {
        fd,
        events: ffi::POLLIN,
        revents: 0,
    };
    // Deadline callers pass the remaining duration, often below one millisecond.
    // Flooring it to zero makes them spin in poll(0) until their deadline expires.
    // Round positive waits up; zero stays nonblocking and readable fds still wake early.
    let timeout_ms = timeout
        .as_nanos()
        .div_ceil(1_000_000)
        .min(c_int::MAX as u128) as c_int;
    // SAFETY: `poll_fd` is one initialized entry and remains live for the call.
    let ready = unsafe { ffi::poll(&mut poll_fd, 1, timeout_ms) };
    ready > 0 && poll_fd.revents & ffi::POLLIN != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn a_fractional_millisecond_wait_blocks_until_timeout() {
        let (reader, _writer) = UnixStream::pair().unwrap();
        let timeout = Duration::from_micros(900);
        let started = Instant::now();
        assert!(!wait_readable(reader.as_raw_fd(), timeout));
        // Rounding a positive remainder down to zero turns the caller's deadline loop
        // into a busy wait. An empty descriptor must actually wait for this remainder.
        assert!(started.elapsed() >= timeout);
    }

    #[test]
    fn zero_timeout_checks_readiness_without_consuming_the_event() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        assert!(!wait_readable(reader.as_raw_fd(), Duration::ZERO));
        writer.write_all(&[7]).unwrap();
        assert!(wait_readable(reader.as_raw_fd(), Duration::ZERO));
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [7]);
        assert!(!wait_readable(reader.as_raw_fd(), Duration::ZERO));
    }

    #[test]
    fn an_incoming_event_wakes_a_blocking_wait() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            writer.write_all(&[1]).unwrap();
        });
        let timeout = Duration::from_secs(2);
        let started = Instant::now();
        let ready = wait_readable(reader.as_raw_fd(), timeout);
        sender.join().unwrap();
        assert!(ready);
        assert!(started.elapsed() < timeout);
    }

    #[test]
    fn a_pending_update_survives_timeout_and_consumes_its_late_reply() {
        // An invalid fd makes any unintended ioctl fail immediately without opening a device.
        let context = Box::new(super::super::evdi_device_context {
            fd: -1,
            buffer_to_update: 2,
            device_index: -1,
            cursor_events_enabled: core::cell::Cell::new(false),
            frame_buffers: vec![evdi_buffer {
                id: 2,
                buffer: core::ptr::null_mut(),
                width: 1,
                height: 1,
                stride: 4,
                rects: core::ptr::null_mut(),
                rect_count: 0,
            }],
        });
        let mut device = Device {
            handle: NonNull::new(Box::into_raw(context)).unwrap(),
            state: Box::default(),
            buffer: Some(RegisteredBuffer {
                pixels: vec![0; 4],
                width: 1,
                height: 1,
                stride: 4,
            }),
            mode: None,
            update_pending: true,
            external_buffers: Vec::new(),
            external_mode: false,
            pending_buffer: None,
        };
        assert!(device.try_next_frame(Duration::ZERO).unwrap().is_none());
        assert!(device.update_pending);
        *device.state.ready_buffer.lock().unwrap() = Some(BUFFER_ID);
        assert!(matches!(
            device.try_next_frame(Duration::ZERO),
            Err(Error::CaptureIoctl(_))
        ));
        assert!(!device.update_pending);
        // A duplicate REQUEST_UPDATE would have changed this to BUFFER_ID (1).
        assert_eq!(unsafe { device.handle.as_ref().buffer_to_update }, 2);
    }

    #[test]
    fn reports_cursor_ioctl_and_bitmap_mapping_failures() {
        let context = Box::new(super::super::evdi_device_context {
            fd: -1,
            buffer_to_update: 0,
            device_index: -1,
            frame_buffers: vec![],
            cursor_events_enabled: core::cell::Cell::new(false),
        });
        let mut device = Device {
            handle: NonNull::new(Box::into_raw(context)).unwrap(),
            state: Box::default(),
            buffer: None,
            mode: None,
            update_pending: false,
            external_buffers: Vec::new(),
            external_mode: false,
            pending_buffer: None,
        };
        device.set_cursor_events(true);
        assert!(!device.cursor_events_enabled());
        // Model successful enable followed by a cursor whose GEM handle cannot be mapped.
        unsafe { device.handle.as_ref().cursor_events_enabled.set(true) };
        let event = super::super::uapi::DrmEvdiEventCursorSet {
            base: super::super::uapi::DrmEvent {
                type_: super::super::uapi::DRM_EVDI_EVENT_CURSOR_SET,
                length: std::mem::size_of::<super::super::uapi::DrmEvdiEventCursorSet>() as u32,
            },
            hot_x: 0,
            hot_y: 0,
            width: 1,
            height: 1,
            enabled: 1,
            buffer_handle: 0,
            buffer_length: 4,
            pixel_format: u32::from_le_bytes(*b"AR24"),
            stride: 4,
        };
        unsafe {
            super::super::dispatch_event(
                device.handle.as_ref(),
                &device.event_context(),
                (&event as *const super::super::uapi::DrmEvdiEventCursorSet).cast(),
                event.base.type_,
            );
        }
        assert!(!device.cursor_events_enabled());
        assert!(
            matches!(device.events(Duration::ZERO).as_slice(), [DeviceEvent::CursorSet(c)] if !c.enabled)
        );
    }

    #[test]
    fn queued_cursor_events_reuse_live_handles_and_read_fresh_pixels() {
        use std::cell::RefCell;
        use std::fs::OpenOptions;
        use std::io::{Seek, SeekFrom};
        use std::os::fd::IntoRawFd;
        use std::os::unix::fs::{FileExt, OpenOptionsExt};
        use std::rc::Rc;
        use std::sync::atomic::{AtomicU64, Ordering};

        const HANDLE: u32 = 17;
        const BITMAP_OFFSET: u64 = 65536;
        static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vino-cursor-regression-{}-{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed),
        ));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        // The descriptor alone owns this scratch file, including on test failure.
        std::fs::remove_file(path).unwrap();
        file.set_len(BITMAP_OFFSET + 16).unwrap();
        let fd = file.try_clone().unwrap().into_raw_fd();

        struct CursorIo {
            alive: bool,
            maps: usize,
            closes: usize,
            disabled: usize,
        }
        let io = Rc::new(RefCell::new(CursorIo {
            alive: true,
            maps: 0,
            closes: 0,
            disabled: 0,
        }));
        let captured = Rc::clone(&io);
        let _hook = ffi::mock_ioctl(move |called_fd, request, argument| {
            if called_fd != fd {
                return None;
            }
            let mut state = captured.borrow_mut();
            if request == ffi::drm_ioctl_mode_map_dumb() {
                // SAFETY: this ioctl's caller supplies the matching ABI structure.
                let map = unsafe { &mut *argument.cast::<ffi::DrmModeMapDumb>() };
                state.maps += 1;
                if map.handle != HANDLE || !state.alive {
                    ffi::set_errno(2); // ENOENT, matching the live failure
                    return Some(-1);
                }
                map.offset = BITMAP_OFFSET;
                Some(0)
            } else if request == ffi::drm_ioctl_gem_close() {
                // Model the real handle table: closing the handle invalidates already
                // queued events naming it; no fresh handle is minted for each event.
                let close = unsafe { &*argument.cast::<ffi::DrmGemClose>() };
                assert_eq!(close.handle, HANDLE);
                state.closes += 1;
                state.alive = false;
                Some(0)
            } else if request == super::super::uapi::ioctl_enable_cursor_events() {
                let policy =
                    unsafe { &*argument.cast::<super::super::uapi::DrmEvdiEnableCursorEvents>() };
                assert_eq!(policy.enable, 0);
                state.disabled += 1;
                Some(0)
            } else {
                None
            }
        });
        let context = Box::new(super::super::evdi_device_context {
            fd,
            buffer_to_update: 0,
            device_index: -1,
            frame_buffers: Vec::new(),
            cursor_events_enabled: core::cell::Cell::new(true),
        });
        let mut device = Device {
            handle: NonNull::new(Box::into_raw(context)).unwrap(),
            state: Box::default(),
            buffer: None,
            mode: None,
            update_pending: false,
            external_buffers: Vec::new(),
            external_mode: false,
            pending_buffer: None,
        };
        fn queue(file: &mut std::fs::File, events: &[(u8, u32, u32)]) {
            let mut bytes = [0_u8; 1024];
            for (i, &(enabled, handle, length)) in events.iter().enumerate() {
                let event = &mut bytes[i * 44..][..44];
                // Serialize explicit fields rather than reading Rust struct padding.
                for (offset, value) in [
                    (0, super::super::uapi::DRM_EVDI_EVENT_CURSOR_SET),
                    (4, 44),
                    (16, 2),
                    (20, 2),
                    (28, handle),
                    (32, length),
                    (36, u32::from_le_bytes(*b"AR24")),
                    (40, 8),
                ] {
                    event[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
                }
                event[24] = enabled;
            }
            file.write_all_at(&bytes, 0).unwrap();
            // The cloned descriptor shares the offset. evdi_handle_events performs the real
            // read, decode, mmap, bitmap copy, munmap and owned safe callback.
            file.seek(SeekFrom::Start(0)).unwrap();
        }
        let initial = [3_u8, 2, 1, 255, 6, 5, 4, 128, 9, 8, 7, 0, 12, 11, 10, 255];
        file.write_all_at(&initial, BITMAP_OFFSET).unwrap();
        queue(&mut file, &[(1, HANDLE, 16), (1, HANDLE, 16)]);
        let events = device.events(Duration::ZERO);
        assert_eq!(events.len(), 2);
        for event in &events {
            assert!(
                matches!(event, DeviceEvent::CursorSet(c) if c.enabled && c.pixels == initial),
                "queued same-handle cursor must retain its bitmap: {event:?}"
            );
        }
        assert!(device.cursor_events_enabled());
        assert_eq!((io.borrow().maps, io.borrow().closes), (2, 0));

        let changed = [37_u8; 16];
        file.write_all_at(&changed, BITMAP_OFFSET).unwrap();
        queue(&mut file, &[(1, HANDLE, 16)]);
        assert!(matches!(device.events(Duration::ZERO).as_slice(),
                [DeviceEvent::CursorSet(c)] if c.enabled && c.pixels == changed));
        assert!(matches!(&events[0], DeviceEvent::CursorSet(c) if c.pixels == initial));
        assert_eq!(io.borrow().maps, 3);

        queue(&mut file, &[(0, 0, 0), (0, HANDLE, 16)]);
        let hidden = device.events(Duration::ZERO);
        assert_eq!(hidden.len(), 2);
        assert!(hidden.iter().all(
            |event| matches!(event, DeviceEvent::CursorSet(c) if !c.enabled && c.pixels.is_empty())
        ));
        assert!(device.cursor_events_enabled());
        assert_eq!(
            (io.borrow().maps, io.borrow().closes, io.borrow().disabled),
            (3, 0, 0)
        );

        // Genuine mapping failures still disable the separate-cursor policy; direct capture
        // must stop instead of accepting framebuffer pixels with a kernel-composited cursor.
        queue(&mut file, &[(1, HANDLE + 1, 16)]);
        assert!(matches!(device.events(Duration::ZERO).as_slice(),
                [DeviceEvent::CursorSet(c)] if !c.enabled && c.pixels.is_empty()));
        assert!(!device.cursor_events_enabled());
        assert_eq!(
            (io.borrow().maps, io.borrow().closes, io.borrow().disabled),
            (4, 0, 1)
        );
        drop(device);
    }

    #[test]
    fn external_pending_update_keeps_its_destination_and_consumes_a_late_reply() {
        let owner = Arc::new(Mutex::new(vec![0_u8; 8]));
        let pointer = owner.lock().unwrap().as_mut_ptr();
        let buffers = (0..2)
            .map(|offset| {
                // SAFETY: fixed Vec allocation is retained by owner and never accessed elsewhere.
                unsafe {
                    ExternalBuffer::new(
                        7 + offset,
                        pointer.add(offset as usize * 4),
                        4,
                        (1, 1, 4),
                        owner.clone(),
                    )
                    .unwrap()
                }
            })
            .collect::<Vec<_>>();
        let context = Box::new(super::super::evdi_device_context {
            fd: -1,
            buffer_to_update: 7,
            device_index: -1,
            cursor_events_enabled: core::cell::Cell::new(true),
            frame_buffers: buffers
                .iter()
                .map(|buffer| evdi_buffer {
                    id: buffer.id,
                    buffer: buffer.pointer.as_ptr().cast(),
                    width: 1,
                    height: 1,
                    stride: 4,
                    rects: core::ptr::null_mut(),
                    rect_count: 0,
                })
                .collect(),
        });
        let mut device = Device {
            handle: NonNull::new(Box::into_raw(context)).unwrap(),
            state: Box::default(),
            buffer: None,
            mode: Some(Mode {
                width: 1,
                height: 1,
                refresh_hz: 60,
                bits_per_pixel: 32,
                pixel_format: u32::from_le_bytes(*b"XR24"),
            }),
            update_pending: true,
            external_buffers: buffers,
            external_mode: true,
            pending_buffer: Some(7),
        };
        // SAFETY: test retains exclusive pixel ownership throughout all pending calls.
        unsafe {
            assert!(device
                .try_next_external_frame(7, Duration::ZERO)
                .unwrap()
                .is_none());
            assert!(device.update_pending);
            assert_eq!(device.pending_buffer, Some(7));
            assert!(matches!(
                device.try_next_external_frame(8, Duration::ZERO),
                Err(Error::PendingBuffer)
            ));
            *device.state.ready_buffer.lock().unwrap() = Some(7);
            assert!(matches!(
                device.try_next_external_frame(7, Duration::ZERO),
                Err(Error::CaptureIoctl(_))
            ));
            assert_eq!(device.handle.as_ref().buffer_to_update, 7);
        }
        assert!(!device.update_pending);
        assert_eq!(device.pending_buffer, None);
        assert!(device.buffer.is_none());
        let mut changed_mode = device.mode.unwrap();
        changed_mode.width = 2;
        device.register_mode(changed_mode);
        assert!(device.buffer.is_none());
        assert_eq!(device.external_buffers.len(), 2);
        // SAFETY: reservation remains exclusive; changed geometry must fail before any ioctl.
        assert!(matches!(
            unsafe { device.try_next_external_frame(7, Duration::ZERO) },
            Err(Error::ExternalBuffer)
        ));
        assert!(Arc::strong_count(&owner) > 1);
        drop(device);
        assert_eq!(Arc::strong_count(&owner), 1);
    }

    #[test]
    fn rejects_invalid_modes() {
        assert_eq!(
            Mode::from_abi(evdi_mode {
                width: 0,
                height: 1080,
                refresh_rate: 60,
                bits_per_pixel: 32,
                pixel_format: 0,
            }),
            None
        );
        assert_eq!(
            Mode::from_abi(evdi_mode {
                width: 1920,
                height: -1,
                refresh_rate: 60,
                bits_per_pixel: 32,
                pixel_format: 0,
            }),
            None
        );
    }

    #[test]
    fn converts_valid_mode() {
        assert_eq!(
            Mode::from_abi(evdi_mode {
                width: 1920,
                height: 1080,
                refresh_rate: 60,
                bits_per_pixel: 32,
                pixel_format: 0x34325258,
            }),
            Some(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
                bits_per_pixel: 32,
                pixel_format: 0x34325258,
            })
        );
    }
}
