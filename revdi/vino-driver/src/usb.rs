//! USB I/O for a DisplayLink DL3 dock.
//!
//! Backed by **rusb** (safe Rust bindings over the libusb-1.0 C library) so the
//! userspace tools (Chimera and the focused sniff utility) drive the dock through the same
//! libusb DisplayLinkManager itself uses*. libusb resolves by soname
//! `libusb-1.0.so.0`; run the binary with cwd `/opt/displaylink` (DLM's own
//! resolution) or `LD_LIBRARY_PATH=/opt/displaylink` / `LD_PRELOAD=/opt/
//! displaylink/libusb-1.0.so.0.3.0` to load DLM's exact bundled build rather than
//! the system libusb.
//!
//! Control transfers and
//! bulk **OUT** (EP 0x02 control, the profile's video endpoints) are synchronous
//! `libusb_bulk_transfer`/`libusb_control_transfer` with `flags=0`; the EP 0x84
//! dock-reply **IN** path is a *persistently posted* pool of asynchronous
//! `libusb_transfer`s reaped via `libusb_handle_events`, so a read URB is always
//! waiting in the kernel the instant the dock produces a reply (the "host wasn't
//! ready" window a sync `read_bulk`-per-call model could open never exists).
//! Video OUT frames use a bounded async submit/reap window (depth 8) matching
//! DLM's measured submit-ahead.

use crate::profile::{Family, Identity, Placement, CLASS_VENDOR, MAX_HEADS, PROTOCOL_DL3, VID};
use crate::{EP_IN_CTRL, EP_OUT_CTRL};
use rusb::constants::{LIBUSB_TRANSFER_COMPLETED, LIBUSB_TRANSFER_TYPE_BULK};
use rusb::ffi::{
    libusb_alloc_transfer, libusb_cancel_transfer, libusb_context, libusb_device_handle,
    libusb_free_transfer, libusb_handle_events_timeout_completed, libusb_submit_transfer,
    libusb_transfer,
};
use rusb::{Context, DeviceHandle, UsbContext};
use std::collections::VecDeque;
use std::os::raw::{c_int, c_uint, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Depth of the EP 0x84 read queue kept perpetually posted to the kernel.
const RECV_QUEUE_DEPTH: usize = 4;
/// Buffer size for each queued EP 0x84 read. Dock control/HDCP responses are
/// well under 1 KiB; 8 KiB is generous and avoids per-call size juggling.
const RECV_BUF_LEN: usize = 8192;
/// EP 0x83 status interrupt (on interface 2) that DLM polls.
const EP_INTR: u8 = 0x83;

/// bmRequestType bytes (direction | type | recipient).
const RT_VENDOR_IN_IFACE: u8 = 0xc1; // IN  | Vendor   | Interface
const RT_VENDOR_OUT_DEV: u8 = 0x40; //  OUT | Vendor   | Device
const RT_STD_IN_DEV: u8 = 0x80; //      IN  | Standard | Device
const RT_STD_OUT_IFACE: u8 = 0x01; //   OUT | Standard | Interface
const RT_STD_OUT_DEV: u8 = 0x00; //     OUT | Standard | Device
const RT_CLASS_OUT_DEV: u8 = 0x20; //   OUT | Class    | Device

/// Depth of the video OUT submit-ahead window (DLM keeps ~8 outstanding).
const PIPELINE_DEPTH: usize = 8;

pub struct Dock {
    /// Idle video OUT transfers retained between frames. Both async pools must drop before
    /// `handle` and `ctx`: cancelling/draining callbacks still needs a live libusb context.
    video_out: Mutex<AsyncOut>,
    /// EP 0x84 bulk IN — dock → host responses (persistently queued async).
    ///
    /// Declared before handle/context on purpose. Rust drops fields in declaration order;
    /// `AsyncIn::drop` cancels its in-flight transfers and then pumps libusb events
    /// (`pump_events(self.ctx)`) to drain the completion callbacks before freeing the transfers --
    /// which requires the libusb context to still be alive. If `handle` (device close) and `ctx`
    /// (`libusb_exit`) dropped first, that pump/cancel would run against an already-exited context
    /// and segfault inside libusb (observed: `segfault at 50 in libusb-1.0.so`). Keep `ep84` above
    /// `handle`/`ctx`. See the 2026-07-12 teardown-crash fix.
    ep84: Mutex<AsyncIn>,
    handle: DeviceHandle<Context>,
    ctx: Context,
    pub timeout: Duration,
    /// Serialises host→dock bulk OUT (EP 0x02) — libusb sync bulk is thread-safe
    /// but we keep frame ordering deterministic.
    out_lock: Mutex<()>,
    /// How the caller placed this device: its name and the endpoints it drives.
    placement: Placement,
    /// What the dock said it is, or `None` if it would not answer.
    identity: Option<Identity>,
    /// Each connector's video bulk-OUT endpoint, or `None` where the device does not expose it.
    ///
    /// Indexed by connector, not by endpoint: Navarro multiplexes four connectors over two
    /// endpoints, so entries repeat. Taking the addresses from the profile rather than naming
    /// `0x08`/`0x0b` inline is what lets one build drive both generations.
    video: [Option<u8>; MAX_HEADS],
    /// How many connectors this device actually backs, which is what loops must use.
    connectors: u8,
    has_intr: bool,
    /// A trial may only claim display/control interfaces, without detaching drivers.
    strict_display_only: bool,
}

#[derive(Debug)]
pub enum Error {
    DeviceNotFound,
    UnsafeDevice(String),
    /// Transfer timed out.
    Timeout,
    /// Endpoint STALL / halt condition (libusb `Pipe`).
    Stall,
    /// Device went away mid-transfer.
    Disconnected,
    /// Any other libusb error (open, claim, config, transfer).
    Usb(rusb::Error),
    Decode,
    ShortRead(usize),
    /// The device accepted fewer bytes than were offered. libusb reports this as a successful
    /// transfer with a smaller count, so it has to be turned into an error deliberately or a
    /// truncated frame reaches the dock unnoticed.
    ShortWrite {
        wrote: usize,
        wanted: usize,
    },
}

impl From<rusb::Error> for Error {
    fn from(e: rusb::Error) -> Self {
        map_err(e)
    }
}

/// Normalise a libusb error into our `Error` so callers can match on the common
/// cases (`Timeout`, `Stall`, `Disconnected`) directly.
fn map_err(e: rusb::Error) -> Error {
    match e {
        rusb::Error::Timeout => Error::Timeout,
        rusb::Error::Pipe => Error::Stall,
        rusb::Error::NoDevice | rusb::Error::NotFound => Error::Disconnected,
        other => Error::Usb(other),
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnsafeDevice(reason) => write!(f, "refusing USB takeover: {reason}"),
            Error::DeviceNotFound => write!(f, "DisplayLink D6000 (17e9:6006) not found"),
            Error::Timeout => write!(f, "USB transfer timed out"),
            Error::Stall => write!(f, "endpoint stalled (Pipe)"),
            // Keep the "NoDevice" token so existing string-based disconnect
            // checks in callers (`contains(\"NoDevice\")`) still fire.
            Error::Disconnected => write!(f, "device disconnected (NoDevice)"),
            Error::Usb(e) => write!(f, "USB error: {e}"),
            Error::ShortWrite { wrote, wanted } => {
                write!(f, "device accepted {wrote} of {wanted} bytes")
            }
            Error::Decode => write!(f, "frame decode failed"),
            Error::ShortRead(n) => write!(f, "short read: {n} bytes"),
        }
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------------
// Persistent async IN pool for EP 0x84 (DLM's "always-posted read URB" model).
// ---------------------------------------------------------------------------

/// State shared between the Rust side and the libusb completion callback.
/// Referenced by raw pointer from `libusb_transfer::user_data`; kept alive in a
/// `Box` inside `AsyncIn` and only freed once every transfer is idle.
struct InShared {
    /// Completed dock replies, in arrival order.
    ready: Mutex<VecDeque<Vec<u8>>>,
    /// Transfers currently owned by libusb (submitted, callback not yet final).
    inflight: AtomicI32,
    /// Set during teardown so the callback stops re-arming.
    closing: AtomicBool,
}

/// libusb completion callback for an EP 0x84 IN transfer. Runs on whichever
/// thread is inside `libusb_handle_events`.
extern "system" fn ep84_complete(transfer: *mut libusb_transfer) {
    // SAFETY: `transfer` is a live transfer we allocated; `user_data` points at
    // the `InShared` boxed in the owning `AsyncIn`, which outlives all transfers.
    unsafe {
        let t = &*transfer;
        let shared = &*(t.user_data as *const InShared);
        if t.status == LIBUSB_TRANSFER_COMPLETED && !shared.closing.load(Ordering::Acquire) {
            let n = t.actual_length as usize;
            if n > 0 {
                let data = std::slice::from_raw_parts(t.buffer, n).to_vec();
                shared.ready.lock().unwrap().push_back(data);
            }
            // Re-arm to keep the queue perpetually posted (DLM model).
            if libusb_submit_transfer(transfer) != 0 {
                shared.inflight.fetch_sub(1, Ordering::AcqRel);
            }
        } else {
            // Cancelled, closing, device gone, or error: transfer is now idle.
            shared.inflight.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

struct AsyncIn {
    ctx: *mut libusb_context,
    transfers: Vec<*mut libusb_transfer>,
    /// Stable heap backing for each transfer's buffer (kept alive until Drop).
    _buffers: Vec<Box<[u8]>>,
    shared: Box<InShared>,
}

// SAFETY: every access to the raw pointers goes through `Dock`'s `Mutex<AsyncIn>`;
// the pointers address libusb objects / heap buffers this `AsyncIn` owns.
unsafe impl Send for AsyncIn {}

impl AsyncIn {
    fn new(handle: *mut libusb_device_handle, ctx: *mut libusb_context, ep: u8) -> AsyncIn {
        Self::with_depth(handle, ctx, ep, RECV_QUEUE_DEPTH)
    }

    fn with_depth(
        handle: *mut libusb_device_handle,
        ctx: *mut libusb_context,
        ep: u8,
        depth: usize,
    ) -> AsyncIn {
        assert!(depth > 0 && depth <= RECV_QUEUE_DEPTH);
        let shared = Box::new(InShared {
            ready: Mutex::new(VecDeque::new()),
            inflight: AtomicI32::new(0),
            closing: AtomicBool::new(false),
        });
        let shared_ptr = &*shared as *const InShared as *mut c_void;
        let mut buffers: Vec<Box<[u8]>> = Vec::with_capacity(depth);
        let mut transfers: Vec<*mut libusb_transfer> = Vec::with_capacity(depth);
        for _ in 0..depth {
            let mut buf = vec![0u8; RECV_BUF_LEN].into_boxed_slice();
            let bptr = buf.as_mut_ptr();
            // SAFETY: alloc a bulk transfer (0 iso packets) and fill its fields;
            // `libusb_fill_bulk_transfer` is a C inline, so we set them directly.
            let t = unsafe { libusb_alloc_transfer(0) };
            assert!(!t.is_null(), "libusb_alloc_transfer failed");
            unsafe {
                let tr = &mut *t;
                tr.dev_handle = handle;
                tr.endpoint = ep;
                tr.transfer_type = LIBUSB_TRANSFER_TYPE_BULK;
                tr.timeout = 0; // infinite: stays posted until data or cancel
                tr.buffer = bptr;
                tr.length = RECV_BUF_LEN as c_int;
                tr.callback = ep84_complete;
                tr.user_data = shared_ptr;
                tr.flags = 0;
                tr.num_iso_packets = 0;
            }
            buffers.push(buf);
            transfers.push(t);
        }
        let ai = AsyncIn {
            ctx,
            transfers,
            _buffers: buffers,
            shared,
        };
        for &t in &ai.transfers {
            // SAFETY: `t` is a filled, un-submitted transfer.
            if unsafe { libusb_submit_transfer(t) } == 0 {
                ai.shared.inflight.fetch_add(1, Ordering::AcqRel);
            }
        }
        ai
    }

    /// Read one dock reply, pumping libusb events until one lands or `timeout`.
    fn recv(&self, timeout: Duration) -> Result<Vec<u8>, Error> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(b) = self.shared.ready.lock().unwrap().pop_front() {
                return Ok(b);
            }
            if self.shared.inflight.load(Ordering::Acquire) == 0 {
                // No posted URBs left (device gone at submit time) and nothing ready.
                return Err(Error::Disconnected);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Timeout);
            }
            let slice = (deadline - now).min(Duration::from_millis(20));
            pump_events(self.ctx, slice);
        }
    }
}

impl Drop for AsyncIn {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        for &t in &self.transfers {
            // SAFETY: cancelling a live or already-idle transfer is safe
            // (returns NOT_FOUND if not in flight); ignore the result.
            unsafe { libusb_cancel_transfer(t) };
        }
        // Drain callbacks so no transfer is still owned by libusb before we free.
        let deadline = Instant::now() + Duration::from_millis(500);
        while self.shared.inflight.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
            pump_events(self.ctx, Duration::from_millis(20));
        }
        for &t in &self.transfers {
            // SAFETY: transfer is idle (callback fired) — safe to free.
            unsafe { libusb_free_transfer(t) };
        }
    }
}

/// Run libusb's event handler for up to `slice`. Multiple threads may call this
/// concurrently; libusb serialises via its internal event lock.
fn pump_events(ctx: *mut libusb_context, slice: Duration) {
    let tv = libc::timeval {
        tv_sec: slice.as_secs() as libc::time_t,
        tv_usec: slice.subsec_micros() as libc::suseconds_t,
    };
    let mut completed: c_int = 0;
    // SAFETY: `ctx` is a live libusb context; `tv`/`completed` are stack locals.
    unsafe { libusb_handle_events_timeout_completed(ctx, &tv, &mut completed) };
}

fn status_to_err(status: c_int) -> Error {
    match status {
        rusb::constants::LIBUSB_TRANSFER_TIMED_OUT => Error::Timeout,
        rusb::constants::LIBUSB_TRANSFER_STALL => Error::Stall,
        rusb::constants::LIBUSB_TRANSFER_NO_DEVICE => Error::Disconnected,
        _ => Error::Usb(rusb::Error::Other),
    }
}

// ---------------------------------------------------------------------------
// Persistent async OUT pool. Frame buffers remain borrowed only for one call.
// ---------------------------------------------------------------------------

/// The only operations the OUT scheduler needs from libusb. Keeping this boundary small lets
/// tests deliver real callbacks without opening a device or submitting a USB transfer.
trait OutBackend {
    fn alloc(&mut self) -> *mut libusb_transfer;
    fn free(&mut self, transfer: *mut libusb_transfer);
    fn submit(&mut self, transfer: *mut libusb_transfer) -> c_int;
    fn cancel(&mut self, transfer: *mut libusb_transfer);
    fn pump(&mut self);
}

struct LibusbOut {
    ctx: *mut libusb_context,
}

impl OutBackend for LibusbOut {
    fn alloc(&mut self) -> *mut libusb_transfer {
        // SAFETY: allocate an unsubmitted bulk transfer, with no isochronous descriptors.
        unsafe { libusb_alloc_transfer(0) }
    }

    fn free(&mut self, transfer: *mut libusb_transfer) {
        // SAFETY: AsyncOut frees its transfers only after all callbacks have been drained.
        unsafe { libusb_free_transfer(transfer) };
    }

    fn submit(&mut self, transfer: *mut libusb_transfer) -> c_int {
        // SAFETY: the idle transfer has been filled with a buffer borrowed until cancellation
        // or completion is drained, and stable callback state owned by AsyncOut.
        unsafe { libusb_submit_transfer(transfer) }
    }

    fn cancel(&mut self, transfer: *mut libusb_transfer) {
        // SAFETY: the pool owns the transfer. NOT_FOUND is harmless if a callback is queued.
        unsafe { libusb_cancel_transfer(transfer) };
    }

    fn pump(&mut self) {
        pump_events(self.ctx, Duration::from_millis(20));
    }
}

type OutCompletion = (*mut libusb_transfer, c_int, c_int);
type OutCompletions = Mutex<VecDeque<OutCompletion>>;

struct OutSlot {
    transfer: *mut libusb_transfer,
    /// Submitted successfully, but its completion has not yet been consumed by the scheduler.
    pending: bool,
}

struct AsyncOut<B: OutBackend = LibusbOut> {
    backend: B,
    handle: *mut libusb_device_handle,
    slots: Vec<OutSlot>,
    /// The allocation stays fixed even when the pool moves. At most one completion per slot
    /// can be queued, so its initial capacity covers every frame without queue allocations.
    completions: Box<OutCompletions>,
}

// SAFETY: Dock serialises all pool operations through `video_out`; libusb permits using its
// handle/context from different threads. Concurrent callbacks touch only the boxed mutex.
unsafe impl Send for AsyncOut<LibusbOut> {}

impl AsyncOut {
    fn new(handle: *mut libusb_device_handle, ctx: *mut libusb_context) -> Result<Self, Error> {
        Self::with_backend(handle, LibusbOut { ctx })
    }
}

impl<B: OutBackend> AsyncOut<B> {
    fn with_backend(handle: *mut libusb_device_handle, backend: B) -> Result<Self, Error> {
        let mut pool = Self {
            backend,
            handle,
            slots: Vec::with_capacity(PIPELINE_DEPTH),
            completions: Box::new(Mutex::new(VecDeque::with_capacity(PIPELINE_DEPTH))),
        };
        for _ in 0..PIPELINE_DEPTH {
            let transfer = pool.backend.alloc();
            if transfer.is_null() {
                // `pool` drops and frees every slot already allocated; nothing was submitted.
                return Err(Error::Usb(rusb::Error::NoMem));
            }
            pool.slots.push(OutSlot {
                transfer,
                pending: false,
            });
        }
        Ok(pool)
    }

    fn write(&mut self, ep: u8, chunks: &[Vec<u8>], timeout: Duration) -> Result<usize, Error> {
        if chunks.is_empty() {
            return Ok(0);
        }
        if chunks.iter().any(|chunk| chunk.len() > c_int::MAX as usize) {
            return Err(Error::Usb(rusb::Error::InvalidParam));
        }
        // The guard drains while `chunks` are still borrowed, including during Rust unwinding.
        // The persistent pool cannot retain pointers into a caller's frame after this returns.
        let mut frame = OutFrame { pool: self };
        frame.run(ep, chunks, timeout)
    }

    fn submit(
        &mut self,
        slot: usize,
        ep: u8,
        chunk: &[u8],
        timeout: Duration,
    ) -> Result<(), Error> {
        let transfer = self.slots[slot].transfer;
        debug_assert!(!self.slots[slot].pending);
        // SAFETY: only an idle slot is modified. OutFrame keeps the chunk alive and immutable
        // through its completion, including timeout/error cancellation.
        unsafe {
            let tr = &mut *transfer;
            tr.dev_handle = self.handle;
            tr.endpoint = ep;
            tr.transfer_type = LIBUSB_TRANSFER_TYPE_BULK;
            tr.timeout = timeout.as_millis().min(c_uint::MAX as u128) as c_uint;
            tr.buffer = chunk.as_ptr() as *mut u8;
            tr.length = chunk.len() as c_int;
            tr.callback = out_complete;
            tr.user_data = &*self.completions as *const OutCompletions as *mut c_void;
            tr.flags = 0;
            tr.num_iso_packets = 0;
        }
        let result = self.backend.submit(transfer);
        if result != 0 {
            return Err(submit_error(result));
        }
        self.slots[slot].pending = true;
        Ok(())
    }

    fn reap(&mut self) -> Option<(usize, c_int, c_int)> {
        let (transfer, status, actual) = self.completions.lock().unwrap().pop_front()?;
        let slot = self
            .slots
            .iter()
            .position(|slot| slot.transfer == transfer)
            .expect("completion belongs to this OUT pool");
        debug_assert!(self.slots[slot].pending);
        self.slots[slot].pending = false;
        Some((slot, status, actual))
    }

    fn has_pending(&self) -> bool {
        self.slots.iter().any(|slot| slot.pending)
    }

    fn cancel_and_drain(&mut self) {
        for slot in &self.slots {
            if slot.pending {
                self.backend.cancel(slot.transfer);
            }
        }
        // A deadline cannot make borrowed buffers safe to free. Cancellation is asynchronous:
        // account for every submitted transfer, even if the device disappeared or a callback
        // arrives late. This also protects the callback queue during pool destruction.
        while self.has_pending() {
            if self.reap().is_none() {
                self.backend.pump();
            }
        }
        for slot in &self.slots {
            // SAFETY: every transfer is idle. Do not retain dangling caller-buffer pointers.
            unsafe {
                (*slot.transfer).buffer = std::ptr::null_mut();
                (*slot.transfer).length = 0;
            }
        }
    }
}

impl<B: OutBackend> Drop for AsyncOut<B> {
    fn drop(&mut self) {
        self.cancel_and_drain();
        for slot in &self.slots {
            self.backend.free(slot.transfer);
        }
    }
}

/// Per-call cleanup is necessary even though the transfer objects themselves persist.
struct OutFrame<'a, B: OutBackend> {
    pool: &'a mut AsyncOut<B>,
}

impl<B: OutBackend> OutFrame<'_, B> {
    fn run(&mut self, ep: u8, chunks: &[Vec<u8>], timeout: Duration) -> Result<usize, Error> {
        let mut next = 0;
        let mut total = 0;
        for slot in 0..PIPELINE_DEPTH.min(chunks.len()) {
            self.pool.submit(slot, ep, &chunks[next], timeout)?;
            next += 1;
        }

        let deadline = Instant::now() + timeout + Duration::from_millis(500);
        while self.pool.has_pending() {
            let Some((slot, status, actual)) = self.pool.reap() else {
                if Instant::now() >= deadline {
                    return Err(Error::Timeout);
                }
                self.pool.backend.pump();
                continue;
            };
            if status != LIBUSB_TRANSFER_COMPLETED {
                return Err(status_to_err(status));
            }
            // SAFETY: this transfer has completed; only this scheduler refills its fields.
            let wanted = unsafe { (*self.pool.slots[slot].transfer).length as usize };
            let wrote = actual.max(0) as usize;
            if wrote != wanted {
                return Err(Error::ShortWrite { wrote, wanted });
            }
            total += wrote;
            if next < chunks.len() {
                self.pool.submit(slot, ep, &chunks[next], timeout)?;
                next += 1;
            }
        }
        Ok(total)
    }
}

impl<B: OutBackend> Drop for OutFrame<'_, B> {
    fn drop(&mut self) {
        self.pool.cancel_and_drain();
    }
}

fn submit_error(code: c_int) -> Error {
    use rusb::constants::*;
    map_err(match code {
        LIBUSB_ERROR_IO => rusb::Error::Io,
        LIBUSB_ERROR_INVALID_PARAM => rusb::Error::InvalidParam,
        LIBUSB_ERROR_ACCESS => rusb::Error::Access,
        LIBUSB_ERROR_NO_DEVICE => rusb::Error::NoDevice,
        LIBUSB_ERROR_NOT_FOUND => rusb::Error::NotFound,
        LIBUSB_ERROR_BUSY => rusb::Error::Busy,
        LIBUSB_ERROR_TIMEOUT => rusb::Error::Timeout,
        LIBUSB_ERROR_OVERFLOW => rusb::Error::Overflow,
        LIBUSB_ERROR_PIPE => rusb::Error::Pipe,
        LIBUSB_ERROR_INTERRUPTED => rusb::Error::Interrupted,
        LIBUSB_ERROR_NO_MEM => rusb::Error::NoMem,
        LIBUSB_ERROR_NOT_SUPPORTED => rusb::Error::NotSupported,
        _ => rusb::Error::Other,
    })
}

impl Dock {
    /// Open the first DisplayLink display function found, and place it by what it says it is.
    ///
    /// Devices are found by *function* -- vendor `17e9` with an interface of class `0xff`,
    /// subclass `0`, protocol `0x03` -- not by a list of product IDs, which can only ever
    /// describe the hardware somebody tested. The family then comes from the device's own identity
    /// descriptor, and `place` turns that into the endpoints to open. A device whose family the
    /// caller declines is left alone rather than guessed at, because the way a dock rejects a guess
    /// is to reset itself; a device that cannot be *asked* is offered its product id instead, so a
    /// transient descriptor failure does not cost a known dock its displays.
    pub fn open(
        mut place: impl FnMut(Option<Family>, u16) -> Option<Placement>,
    ) -> Result<Self, Error> {
        let ctx = Context::new().map_err(map_err)?;

        let find_open = |ctx: &Context| -> Result<(DeviceHandle<Context>, u16), Error> {
            for dev in ctx.devices().map_err(map_err)?.iter() {
                let Ok(desc) = dev.device_descriptor() else {
                    continue;
                };
                if desc.vendor_id() != VID || !is_dl3_display_function(&dev) {
                    continue;
                }
                return Ok((dev.open().map_err(map_err)?, desc.product_id()));
            }
            Err(Error::DeviceNotFound)
        };

        let (handle, product) = find_open(&ctx)?;

        // Chimera owns the complete display function for the lifetime of this
        // handle. Detach stale interface drivers before selecting the known
        // D6000 configuration.
        for iface in 0u8..8 {
            let _ = handle.detach_kernel_driver(iface);
        }
        let _ = handle.set_active_configuration(1);

        // Configuration changes can rebind an interface, so detach once more
        // before claiming the control/video interface.
        let _ = handle.detach_kernel_driver(0);
        handle.claim_interface(0).map_err(map_err)?;
        let _ = handle.set_alternate_setting(0, 0);

        // What this hardware is, asked of the hardware: one standard GET_DESCRIPTOR, no session
        // and no crypto. A dock that names a family nobody here drives is declined rather than
        // guessed at, because the way a dock rejects a guess is to reset itself. A dock that
        // cannot be *asked* falls back to the product-ID quirk table, so a transient descriptor
        // failure does not cost a known dock its displays.
        let identity = read_identity(&handle);
        let family = identity.as_ref().and_then(Identity::family);
        if family.is_none() {
            eprintln!("[usb] identity unreadable; placing {product:04x} by its product id");
        }
        let Some(placement) = place(family, product) else {
            match identity.as_ref() {
                Some(id) => eprintln!("[usb] {id} is not a device this stack drives yet"),
                None => {
                    eprintln!("[usb] no identity descriptor and no quirk entry for {product:04x}")
                }
            }
            return Err(Error::DeviceNotFound);
        };
        if let Some(id) = identity.as_ref() {
            eprintln!("[usb] {id} running firmware {}", id.version);
        }

        // Each connector's video endpoint, taken from the profile and checked against the
        // descriptor. A connector whose endpoint the device does not expose is dropped rather
        // than guessed at, and the connector count follows: a dock in a known family with fewer
        // outputs is driven with the outputs it has.
        let mut video = [None; MAX_HEADS];
        let mut connectors = 0u8;
        for (slot, addr) in video.iter_mut().zip(placement.video_endpoints) {
            if connectors >= placement.connectors || !has_endpoint(&handle, addr) {
                break;
            }
            *slot = Some(addr);
            connectors += 1;
        }
        if connectors == 0 {
            eprintln!(
                "[usb] no video endpoint from {:#04x?}",
                placement.video_endpoints
            );
            return Err(Error::DeviceNotFound);
        }
        eprintln!(
            "[usb] matched \"{}\", {connectors} connector(s), video endpoints {:#04x?}",
            placement.name,
            &video[..usize::from(connectors)]
        );

        // EP 0x83 lives on interface 2. It is auxiliary status input, so a
        // failure to claim it does not invalidate the control/video endpoints.
        let _ = handle.detach_kernel_driver(2);
        let has_intr = match handle.claim_interface(2) {
            Ok(()) => has_endpoint(&handle, EP_INTR),
            Err(e) => {
                eprintln!("[usb] claim interface 2 for EP 0x83 failed: {e}");
                false
            }
        };

        // Clear endpoint state left by a previous userspace owner.
        let _ = handle.clear_halt(EP_OUT_CTRL);
        let _ = handle.clear_halt(EP_IN_CTRL);
        for (head, addr) in video.iter().enumerate() {
            let Some(addr) = addr else { continue };
            if let Err(e) = handle.clear_halt(*addr) {
                eprintln!("[usb] clear head {head} video halt (EP {addr:#04x}) failed: {e}");
            }
        }

        let video_out = AsyncOut::new(handle.as_raw(), ctx.as_raw())?;
        // Bring up the persistent async EP 0x84 IN pool.
        let ep84 = AsyncIn::new(handle.as_raw(), ctx.as_raw(), EP_IN_CTRL);

        Ok(Self {
            video_out: Mutex::new(video_out),
            handle,
            ctx,
            timeout: Duration::from_millis(2000),
            ep84: Mutex::new(ep84),
            out_lock: Mutex::new(()),
            placement,
            identity,
            video,
            connectors,
            has_intr,
            strict_display_only: false,
        })
    }

    /// Open the explicitly selected Ella dock without touching its audio/network functions.
    /// `port` is a Linux physical USB path (e.g. `8-1.2`), not a volatile device address.
    pub fn open_ella_at(port: &str, placement: Placement) -> Result<Self, Error> {
        let (bus, ports) = parse_port(port)?;
        let ctx = Context::new().map_err(map_err)?;
        let devices = ctx.devices().map_err(map_err)?;
        let mut matches = devices
            .iter()
            .filter(|d| d.bus_number() == bus && d.port_numbers().ok().as_ref() == Some(&ports));
        let device = matches.next().ok_or(Error::DeviceNotFound)?;
        if matches.next().is_some() {
            return Err(Error::UnsafeDevice("ambiguous physical USB path".into()));
        }
        let descriptor = device.device_descriptor().map_err(map_err)?;
        if descriptor.vendor_id() != VID || descriptor.product_id() != 0x4307 {
            return Err(Error::UnsafeDevice("expected 17e9:4307".into()));
        }
        let config = device.active_config_descriptor().map_err(map_err)?;
        let interfaces: Vec<_> = config
            .interfaces()
            .flat_map(|i| i.descriptors())
            .map(|d| InterfaceLayout {
                number: d.interface_number(),
                alternate: d.setting_number(),
                class: d.class_code(),
                subclass: d.sub_class_code(),
                protocol: d.protocol_code(),
                endpoints: d
                    .endpoint_descriptors()
                    .map(|e| (e.address(), e.transfer_type()))
                    .collect(),
            })
            .collect();
        validate_ella_layout(config.number(), &interfaces)?;
        if placement.connectors == 0
            || placement.connectors > 2
            || placement
                .video_endpoints
                .iter()
                .take(usize::from(placement.connectors))
                .any(|&e| e != EP_OUT_CTRL)
        {
            return Err(Error::UnsafeDevice(
                "placement is not an Ella display profile".into(),
            ));
        }
        let handle = device.open().map_err(map_err)?;
        let identity = read_identity(&handle)
            .filter(|id| id.family() == Some(Family::Ella))
            .ok_or_else(|| Error::UnsafeDevice("missing or non-Ella identity descriptor".into()))?;
        for iface in [0, 1] {
            if handle.kernel_driver_active(iface).map_err(map_err)? {
                return Err(Error::UnsafeDevice(format!(
                    "interface {iface} still has a kernel driver"
                )));
            }
        }
        handle.claim_interface(0).map_err(map_err)?;
        handle.set_alternate_setting(0, 0).map_err(map_err)?;
        // Only clear the two display endpoints. Never SET_CONFIGURATION or touch interface 2.
        handle.clear_halt(EP_OUT_CTRL).map_err(map_err)?;
        handle.clear_halt(EP_IN_CTRL).map_err(map_err)?;
        let video_out = AsyncOut::new(handle.as_raw(), ctx.as_raw())?;
        // Ella's measured reply discipline permits exactly one posted read (PROFILE_ELLA).
        let ep84 = AsyncIn::with_depth(handle.as_raw(), ctx.as_raw(), EP_IN_CTRL, 1);
        let mut video = [None; MAX_HEADS];
        for slot in video.iter_mut().take(usize::from(placement.connectors)) {
            *slot = Some(EP_OUT_CTRL);
        }
        eprintln!(
            "[usb] selected {port}: {identity}, firmware {}",
            identity.version
        );
        Ok(Self {
            video_out: Mutex::new(video_out),
            ep84: Mutex::new(ep84),
            handle,
            ctx,
            timeout: Duration::from_secs(2),
            out_lock: Mutex::new(()),
            connectors: placement.connectors,
            placement,
            identity: Some(identity),
            video,
            has_intr: false,
            strict_display_only: true,
        })
    }

    /// Issue a vendor-specific IN control transfer (bmRequestType=0xc1).
    pub fn vendor_in(
        &self,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
    ) -> Result<Vec<u8>, Error> {
        let mut buf = vec![0u8; length as usize];
        let n = self
            .handle
            .read_control(
                RT_VENDOR_IN_IFACE,
                request,
                value,
                index,
                &mut buf,
                self.timeout,
            )
            .map_err(map_err)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Issue a vendor-specific OUT control transfer (bmRequestType=0x40).
    pub fn vendor_out(
        &self,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
    ) -> Result<(), Error> {
        self.handle
            .write_control(RT_VENDOR_OUT_DEV, request, value, index, data, self.timeout)
            .map(|_| ())
            .map_err(map_err)
    }

    /// Standard IN control transfer (e.g. GET_DESCRIPTOR), recipient=device.
    pub fn std_in(
        &self,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>, Error> {
        let mut buf = vec![0u8; length as usize];
        let n = self
            .handle
            .read_control(RT_STD_IN_DEV, request, value, index, &mut buf, timeout)
            .map_err(map_err)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Standard OUT control transfer with recipient=interface (e.g. SET_INTERFACE).
    pub fn std_out_iface(
        &self,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), Error> {
        self.handle
            .write_control(RT_STD_OUT_IFACE, request, value, index, data, timeout)
            .map(|_| ())
            .map_err(map_err)
    }

    /// Class OUT control transfer with recipient=device. DLM issues
    /// `bmRequestType=0x20 bRequest=0x0c wValue=0/1` (no data) during early setup.
    pub fn class_out_device(
        &self,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
    ) -> Result<(), Error> {
        self.handle
            .write_control(RT_CLASS_OUT_DEV, request, value, index, data, self.timeout)
            .map(|_| ())
            .map_err(map_err)
    }

    /// Claim an additional interface beyond the ones `open()` claims by default
    /// (0 for the control plane, 2 for the EP 0x83 status poll). DLM's libusb
    /// claims interface 1 (the DFU/app-specific interface) too, via its own
    /// `usbfs` open, and the in-kernel driver binds it as a real driver
    /// (confirmed live: `vino 2-1.1:1.1: bound D6000 interface 1`) -- needed
    /// before `std_out_iface`'s `SET_INTERFACE` on it will succeed.
    pub fn claim_interface(&self, iface: u8) -> Result<(), Error> {
        if self.strict_display_only {
            if iface > 1 || self.handle.kernel_driver_active(iface).map_err(map_err)? {
                return Err(Error::UnsafeDevice(format!(
                    "cannot claim interface {iface}"
                )));
            }
        } else {
            let _ = self.handle.detach_kernel_driver(iface);
        }
        self.handle.claim_interface(iface).map_err(map_err)
    }

    /// Issue an empty SET (host→device) control transfer — DLM does this between
    /// vendor requests; function unknown but apparently benign.
    pub fn ctrl_noop(&self) -> Result<(), Error> {
        let _ = self
            .handle
            .write_control(RT_STD_OUT_DEV, 0, 0, 0, &[], self.timeout);
        Ok(())
    }

    /// Submit one control-plane transfer through libusb's asynchronous bulk
    /// path. The transfer itself has no libusb timeout; `completion_deadline`
    /// bounds the caller wait and triggers a synchronous cancellation/drain.
    pub fn write_ctrl_raw_async(
        &self,
        bytes: &[u8],
        completion_deadline: Duration,
    ) -> Result<usize, Error> {
        struct OutShared {
            done: AtomicBool,
            status: AtomicI32,
            actual_length: AtomicI32,
        }
        extern "system" fn out_complete(transfer: *mut libusb_transfer) {
            // SAFETY: `transfer` is the one transfer this call owns; `user_data`
            // points at the `OutShared` kept alive on this function's stack
            // until the transfer is known idle (join loop below).
            unsafe {
                let t = &*transfer;
                let shared = &*(t.user_data as *const OutShared);
                shared.status.store(t.status, Ordering::Release);
                shared
                    .actual_length
                    .store(t.actual_length, Ordering::Release);
                shared.done.store(true, Ordering::Release);
            }
        }

        let _g = self.out_lock.lock().unwrap();
        let shared = Box::new(OutShared {
            done: AtomicBool::new(false),
            status: AtomicI32::new(-1),
            actual_length: AtomicI32::new(0),
        });
        let shared_ptr = &*shared as *const OutShared as *mut c_void;

        // SAFETY: alloc a bulk transfer (0 iso packets); fields set directly,
        // mirroring `AsyncIn::new`'s pattern for the IN side.
        let t = unsafe { libusb_alloc_transfer(0) };
        assert!(!t.is_null(), "libusb_alloc_transfer failed");
        // `bytes` outlives the transfer (freed/cancelled before this fn
        // returns), so casting away constness for the C struct is sound: the
        // OUT direction never has libusb write back into this buffer.
        unsafe {
            let tr = &mut *t;
            tr.dev_handle = self.handle.as_raw();
            tr.endpoint = EP_OUT_CTRL;
            tr.transfer_type = LIBUSB_TRANSFER_TYPE_BULK;
            tr.timeout = 0; // matches DLM exactly -- see doc comment above
            tr.buffer = bytes.as_ptr() as *mut u8;
            tr.length = bytes.len() as c_int;
            tr.callback = out_complete;
            tr.user_data = shared_ptr;
            tr.flags = 0;
            tr.num_iso_packets = 0;
        }
        // SAFETY: `t` is a filled, un-submitted transfer.
        if unsafe { libusb_submit_transfer(t) } != 0 {
            unsafe { libusb_free_transfer(t) };
            return Err(Error::Usb(rusb::Error::Io));
        }

        let deadline = Instant::now() + completion_deadline;
        while !shared.done.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                // `shared` is stack-owned and the callback writes through a raw pointer to it,
                // so the cancellation completion must be drained before either it or the
                // transfer is released. This wait is deliberately unbounded: giving up early and
                // freeing anyway is a use-after-free the moment the callback does fire, and a
                // completion that never arrives means the device is gone and libusb will report
                // it.
                unsafe { libusb_cancel_transfer(t) };
                while !shared.done.load(Ordering::Acquire) {
                    pump_events(self.ctx.as_raw(), Duration::from_millis(20));
                }
                unsafe { libusb_free_transfer(t) };
                return Err(Error::Timeout);
            }
            pump_events(self.ctx.as_raw(), Duration::from_millis(20));
        }
        let status = shared.status.load(Ordering::Acquire);
        let actual = shared.actual_length.load(Ordering::Acquire) as usize;
        unsafe { libusb_free_transfer(t) };
        if status != LIBUSB_TRANSFER_COMPLETED {
            return Err(status_to_err(status));
        }
        if actual != bytes.len() {
            return Err(Error::ShortWrite {
                wrote: actual,
                wanted: bytes.len(),
            });
        }
        Ok(actual)
    }

    /// [`write_ctrl_raw_async`] with the standard two-second completion deadline.
    pub fn write_ctrl_dlm(&self, bytes: &[u8]) -> Result<usize, Error> {
        self.write_ctrl_raw_async(bytes, Duration::from_secs(2))
    }

    /// Write raw bytes synchronously to the control-plane bulk OUT endpoint.
    pub fn write_ctrl_raw(&self, bytes: &[u8]) -> Result<usize, Error> {
        let _g = self.out_lock.lock().unwrap();
        self.handle
            .write_bulk(EP_OUT_CTRL, bytes, self.timeout)
            .map_err(map_err)
    }

    /// How the caller placed this device.
    pub fn placement(&self) -> Placement {
        self.placement
    }

    /// What the dock said it is, or `None` if it would not answer.
    pub fn identity(&self) -> Option<&Identity> {
        self.identity.as_ref()
    }

    /// How many connectors this device actually backs. Loops must use this, never [`MAX_HEADS`].
    pub fn connectors(&self) -> usize {
        usize::from(self.connectors)
    }

    /// A connector's video bulk-OUT endpoint.
    fn video_ep(&self, head: usize) -> Result<u8, Error> {
        self.video
            .get(head)
            .copied()
            .flatten()
            .ok_or(Error::Disconnected)
    }

    /// Write a video frame to one connector. Returns bytes written.
    ///
    /// Takes the OUT lock like every other host->dock write: a video frame interleaved with a
    /// control message is a torn frame, and on a dock whose video shares the control endpoint it
    /// is a corrupted control stream as well.
    pub fn write_video(&self, head: usize, bytes: &[u8]) -> Result<usize, Error> {
        let ep = self.video_ep(head)?;
        let _g = self.out_lock.lock().unwrap();
        let written = self
            .handle
            .write_bulk(ep, bytes, self.timeout)
            .map_err(map_err)?;
        // A short write is a truncated frame, not a success. libusb reports the byte count and
        // returns Ok, so nothing else notices.
        if written == bytes.len() {
            Ok(written)
        } else {
            Err(Error::ShortWrite {
                wrote: written,
                wanted: bytes.len(),
            })
        }
    }

    /// Pipeline a whole frame's chunks on the head-0 video EP (0x08): submit up
    /// to [`PIPELINE_DEPTH`] transfers to the URB ring, reap completions, and
    /// submit-ahead — matching DLM's async libusb submission so the dock's frame
    /// assembler never sees a host-side stall mid-frame. Returns total bytes.
    pub fn write_video_frame(&self, head: usize, chunks: &[Vec<u8>]) -> Result<usize, Error> {
        self.pipeline_out(self.video_ep(head)?, chunks)
    }

    /// Bounded async submit/reap of `chunks` on a bulk OUT endpoint. Keeps at
    /// most [`PIPELINE_DEPTH`] transfers outstanding (DLM's measured depth),
    /// submitting the next chunk the instant one completes.
    fn pipeline_out(&self, ep: u8, chunks: &[Vec<u8>]) -> Result<usize, Error> {
        if chunks.is_empty() {
            return Ok(0);
        }
        // Always take the shared endpoint lock before the pool lock. Control traffic also takes
        // out_lock, so it cannot interleave with a frame on Ella's shared control/video pipe.
        let _g = self.out_lock.lock().unwrap();
        self.video_out
            .lock()
            .unwrap()
            .write(ep, chunks, self.timeout)
    }

    /// Clear the halt/stall condition on one connector's video endpoint.
    pub fn clear_video_halt(&self, head: usize) -> Result<(), Error> {
        self.handle
            .clear_halt(self.video_ep(head)?)
            .map_err(map_err)
    }

    /// Read one EP 0x83 interrupt event into `buf`. Returns the byte count
    /// (0 on timeout — matching the old poller's "no event" handling).
    pub fn read_intr(&self, buf: &mut [u8], timeout: Duration) -> Result<usize, Error> {
        if !self.has_intr {
            return Err(Error::Disconnected);
        }
        match self.handle.read_interrupt(EP_INTR, buf, timeout) {
            Ok(n) => Ok(n),
            Err(rusb::Error::Timeout) => Ok(0), // no event
            Err(e) => Err(map_err(e)),
        }
    }

    /// Read one frame from EP 0x84 using the persistent read queue. Uses the
    /// default timeout.
    pub fn recv_frame_raw(&self, max_len: usize) -> Result<Vec<u8>, Error> {
        self.recv_frame_raw_timeout(max_len, self.timeout)
    }

    /// Read one frame from EP 0x84 with an explicit timeout. The async pool keeps
    /// `RECV_QUEUE_DEPTH` URBs posted so the dock always has a waiting buffer.
    pub fn recv_frame_raw_timeout(
        &self,
        _max_len: usize,
        timeout: Duration,
    ) -> Result<Vec<u8>, Error> {
        let ep = self.ep84.lock().unwrap();
        ep.recv(timeout)
    }
}

/// libusb completion callback for a video OUT transfer. Another thread pumping this context
/// (for example the dock-reply reader) may deliver it; only the boxed queue is shared.
extern "system" fn out_complete(transfer: *mut libusb_transfer) {
    // SAFETY: AsyncOut keeps the transfer and queue alive until every submitted callback has
    // been drained. The callback does not access either after releasing the queue lock.
    unsafe {
        let t = &*transfer;
        let q = &*(t.user_data as *const OutCompletions);
        q.lock()
            .unwrap()
            .push_back((transfer, t.status, t.actual_length));
    }
}

/// Does the active configuration expose an endpoint with address `addr`?
/// Whether this device exposes a DL3 display function.
///
/// Class `0xff` / subclass `0` / protocol `0x03`, which is what DisplayLink's own udev rules key
/// on. Protocol `0x00` is the older `udl` hardware and is excluded for free.
fn is_dl3_display_function(dev: &rusb::Device<Context>) -> bool {
    let Ok(config) = dev.active_config_descriptor() else {
        return false;
    };
    config.interfaces().flat_map(|i| i.descriptors()).any(|d| {
        d.class_code() == CLASS_VENDOR
            && d.sub_class_code() == 0
            && d.protocol_code() == PROTOCOL_DL3
    })
}

/// Read the dock's identity by walking its configuration descriptor.
///
/// The blob is a vendor descriptor inside the configuration, not a separately addressable one, so
/// the whole configuration is fetched and walked.
fn read_identity(handle: &DeviceHandle<Context>) -> Option<Identity> {
    const DESCRIPTOR_CONFIG: u16 = 0x02 << 8;
    const CONFIG_DESCRIPTOR_MAX: usize = 1024;
    let timeout = Duration::from_millis(1000);

    let mut head = [0u8; 9];
    handle
        .read_control(
            RT_STD_IN_DEV,
            0x06,
            DESCRIPTOR_CONFIG,
            0,
            &mut head,
            timeout,
        )
        .ok()?;
    let total = usize::from(u16::from_le_bytes([head[2], head[3]]));
    if total < head.len() || total > CONFIG_DESCRIPTOR_MAX {
        return None;
    }
    let mut all = vec![0u8; total];
    handle
        .read_control(RT_STD_IN_DEV, 0x06, DESCRIPTOR_CONFIG, 0, &mut all, timeout)
        .ok()?;
    Identity::parse(&all)
}

fn has_endpoint(handle: &DeviceHandle<Context>, addr: u8) -> bool {
    let dev = handle.device();
    let Ok(config) = dev.active_config_descriptor() else {
        return false;
    };
    config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .flat_map(|d| d.endpoint_descriptors())
        .any(|e| e.address() == addr)
}

fn parse_port(port: &str) -> Result<(u8, Vec<u8>), Error> {
    let invalid = || Error::UnsafeDevice("expected a physical USB path such as 8-1.2".into());
    let (bus, path) = port.split_once('-').ok_or_else(invalid)?;
    let bus = bus
        .parse::<u8>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(invalid)?;
    let ports: Vec<u8> = path
        .split('.')
        .map(|v| v.parse::<u8>().ok().filter(|p| *p > 0).ok_or_else(invalid))
        .collect::<Result<_, _>>()?;
    if ports.is_empty() || ports.len() > 7 {
        return Err(invalid());
    }
    Ok((bus, ports))
}

struct InterfaceLayout {
    number: u8,
    alternate: u8,
    class: u8,
    subclass: u8,
    protocol: u8,
    endpoints: Vec<(u8, rusb::TransferType)>,
}

fn validate_ella_layout(config: u8, interfaces: &[InterfaceLayout]) -> Result<(), Error> {
    let valid = |number, class, subclass, protocol, endpoints: &[(u8, rusb::TransferType)]| {
        let mut candidates = interfaces
            .iter()
            .filter(|d| d.number == number && d.alternate == 0);
        candidates.next().is_some_and(|d| {
            d.class == class
                && d.subclass == subclass
                && d.protocol == protocol
                && endpoints.iter().all(|e| d.endpoints.contains(e))
        }) && candidates.next().is_none()
    };
    if config != 1
        || !valid(
            0,
            0xff,
            0,
            3,
            &[
                (EP_OUT_CTRL, rusb::TransferType::Bulk),
                (EP_IN_CTRL, rusb::TransferType::Bulk),
            ],
        )
        || !valid(1, 0xfe, 1, 1, &[])
    {
        return Err(Error::UnsafeDevice(
            "unexpected Ella configuration/interfaces".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod out_pool_tests {
    use super::*;
    use rusb::constants::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    struct Submitted {
        transfer: usize,
        queue: usize,
        endpoint: u8,
        timeout: c_uint,
        bytes: Vec<u8>,
    }

    struct Pending {
        transfer: *mut libusb_transfer,
        status: c_int,
        actual: c_int,
        cancelled: bool,
    }

    #[derive(Default)]
    struct MockState {
        allocations: Vec<usize>,
        frees: Vec<usize>,
        allocation_attempts: usize,
        fail_allocation_at: Option<usize>,
        submit_attempts: usize,
        fail_submit_at: Option<(usize, c_int)>,
        submissions: Vec<Submitted>,
        pending: VecDeque<Pending>,
        outcomes: VecDeque<(c_int, Option<c_int>)>,
        cancellations: Vec<usize>,
        peak_pending: usize,
        pump_count: usize,
        hold_until_cancel: bool,
        delayed_cancel_pumps: usize,
        remaining_cancel_pumps: usize,
        cross_thread_callbacks: bool,
        complete_during_submit: bool,
        reverse_completions: bool,
        panic_once: bool,
    }

    struct MockOut(Rc<RefCell<MockState>>);

    impl OutBackend for MockOut {
        fn alloc(&mut self) -> *mut libusb_transfer {
            let mut state = self.0.borrow_mut();
            let attempt = state.allocation_attempts;
            state.allocation_attempts += 1;
            if state.fail_allocation_at == Some(attempt) {
                return std::ptr::null_mut();
            }
            // This libusb operation only allocates host memory: no context or device is opened.
            let transfer = unsafe { libusb_alloc_transfer(0) };
            assert!(!transfer.is_null());
            state.allocations.push(transfer as usize);
            transfer
        }

        fn free(&mut self, transfer: *mut libusb_transfer) {
            let mut state = self.0.borrow_mut();
            assert!(!state.pending.iter().any(|p| p.transfer == transfer));
            assert!(!state.frees.contains(&(transfer as usize)));
            unsafe {
                assert!((*transfer).buffer.is_null());
                assert_eq!((*transfer).length, 0);
                libusb_free_transfer(transfer);
            }
            state.frees.push(transfer as usize);
        }

        fn submit(&mut self, transfer: *mut libusb_transfer) -> c_int {
            let mut state = self.0.borrow_mut();
            let attempt = state.submit_attempts;
            state.submit_attempts += 1;
            if let Some((index, error)) = state.fail_submit_at {
                if attempt == index {
                    return error;
                }
            }
            assert!(!state.pending.iter().any(|p| p.transfer == transfer));
            let tr = unsafe { &*transfer };
            state.submissions.push(Submitted {
                transfer: transfer as usize,
                queue: tr.user_data as usize,
                endpoint: tr.endpoint,
                timeout: tr.timeout,
                bytes: unsafe {
                    std::slice::from_raw_parts(tr.buffer, tr.length as usize).to_vec()
                },
            });
            let (status, actual) = state
                .outcomes
                .pop_front()
                .unwrap_or((LIBUSB_TRANSFER_COMPLETED, None));
            state.pending.push_back(Pending {
                transfer,
                status,
                actual: actual.unwrap_or(tr.length),
                cancelled: false,
            });
            state.peak_pending = state.peak_pending.max(state.pending.len());
            assert!(state.pending.len() <= PIPELINE_DEPTH);
            if state.complete_during_submit {
                // Another libusb event thread can finish a transfer before submit returns.
                let pending = state.pending.pop_back().unwrap();
                unsafe {
                    (*transfer).status = pending.status;
                    (*transfer).actual_length = pending.actual;
                    ((*transfer).callback)(transfer);
                }
            }
            0
        }

        fn cancel(&mut self, transfer: *mut libusb_transfer) {
            let mut state = self.0.borrow_mut();
            state.cancellations.push(transfer as usize);
            state.remaining_cancel_pumps = state.delayed_cancel_pumps;
            if let Some(pending) = state.pending.iter_mut().find(|p| p.transfer == transfer) {
                pending.cancelled = true;
                pending.status = LIBUSB_TRANSFER_CANCELLED;
                pending.actual = 0;
            }
        }

        fn pump(&mut self) {
            let mut state = self.0.borrow_mut();
            state.pump_count += 1;
            if std::mem::take(&mut state.panic_once) {
                panic!("injected pump panic");
            }
            if state.remaining_cancel_pumps > 0 {
                state.remaining_cancel_pumps -= 1;
                return;
            }
            if state.hold_until_cancel && state.pending.iter().all(|p| !p.cancelled) {
                drop(state);
                std::thread::sleep(Duration::from_millis(2));
                return;
            }
            let pending = if state.reverse_completions {
                state.pending.pop_back()
            } else {
                state.pending.pop_front()
            };
            let cross_thread = state.cross_thread_callbacks;
            drop(state);
            let Some(pending) = pending else {
                return;
            };
            let address = pending.transfer as usize;
            let callback = move || unsafe {
                let transfer = address as *mut libusb_transfer;
                (*transfer).status = pending.status;
                (*transfer).actual_length = pending.actual;
                ((*transfer).callback)(transfer);
            };
            if cross_thread {
                std::thread::spawn(callback).join().unwrap();
            } else {
                callback();
            }
        }
    }

    fn pool(state: &Rc<RefCell<MockState>>) -> AsyncOut<MockOut> {
        AsyncOut::with_backend(std::ptr::null_mut(), MockOut(Rc::clone(state))).unwrap()
    }

    fn assert_idle(pool: &AsyncOut<MockOut>, state: &Rc<RefCell<MockState>>) {
        assert!(!pool.has_pending());
        assert!(pool.completions.lock().unwrap().is_empty());
        assert!(state.borrow().pending.is_empty());
        for slot in &pool.slots {
            unsafe {
                assert!((*slot.transfer).buffer.is_null());
                assert_eq!((*slot.transfer).length, 0);
            }
        }
    }

    fn chunks(count: usize) -> Vec<Vec<u8>> {
        (0..count).map(|n| vec![n as u8; n * 17 + 1]).collect()
    }

    #[test]
    fn out_pool_reuses_slots_and_callback_storage_across_frames() {
        let state = Rc::new(RefCell::new(MockState {
            reverse_completions: true,
            cross_thread_callbacks: true,
            ..Default::default()
        }));
        let mut pool = pool(&state);
        let queue_capacity = pool.completions.lock().unwrap().capacity();
        assert_eq!(pool.write(8, &[], Duration::from_millis(17)).unwrap(), 0);
        assert!(state.borrow().submissions.is_empty());
        let first = chunks(25);
        let first_bytes = first.iter().map(Vec::len).sum::<usize>();
        assert_eq!(
            pool.write(8, &first, Duration::from_millis(17)).unwrap(),
            first_bytes
        );
        assert_idle(&pool, &state);
        // Include an empty chunk: it is a valid zero-length transfer, not a stale buffer.
        let second = vec![vec![0xcc; 19], vec![], vec![0x44; 3]];
        assert_eq!(
            pool.write(2, &second, Duration::from_millis(231)).unwrap(),
            22
        );
        assert_idle(&pool, &state);
        assert_eq!(pool.completions.lock().unwrap().capacity(), queue_capacity);
        {
            let state = state.borrow();
            assert_eq!(state.allocations.len(), PIPELINE_DEPTH);
            assert_eq!(state.peak_pending, PIPELINE_DEPTH);
            assert!(state.frees.is_empty());
            assert!(state.cancellations.is_empty());
            let queue = state.submissions[0].queue;
            for (i, submission) in state.submissions.iter().enumerate() {
                assert!(state.allocations.contains(&submission.transfer));
                assert_eq!(submission.queue, queue);
                let (expected, endpoint, timeout) = if i < first.len() {
                    (&first[i], 8, 17)
                } else {
                    (&second[i - first.len()], 2, 231)
                };
                assert_eq!(&submission.bytes, expected);
                assert_eq!(submission.endpoint, endpoint);
                assert_eq!(submission.timeout, timeout);
            }
        }
        drop(pool);
        assert_eq!(state.borrow().frees.len(), PIPELINE_DEPTH);
    }

    #[test]
    fn out_pool_partial_allocation_failure_frees_all_earlier_slots() {
        for fail_at in 0..PIPELINE_DEPTH {
            let state = Rc::new(RefCell::new(MockState {
                fail_allocation_at: Some(fail_at),
                ..Default::default()
            }));
            let result = AsyncOut::with_backend(std::ptr::null_mut(), MockOut(Rc::clone(&state)));
            assert!(matches!(result, Err(Error::Usb(rusb::Error::NoMem))));
            let state = state.borrow();
            assert_eq!(state.allocation_attempts, fail_at + 1);
            assert_eq!(state.allocations.len(), fail_at);
            assert_eq!(state.frees, state.allocations);
            assert!(state.submissions.is_empty());
        }
    }

    #[test]
    fn out_pool_handles_callbacks_before_submit_returns_and_full_completion_queue() {
        let state = Rc::new(RefCell::new(MockState {
            complete_during_submit: true,
            ..Default::default()
        }));
        let mut pool = pool(&state);
        let capacity = pool.completions.lock().unwrap().capacity();
        let chunks = chunks(32);
        assert_eq!(
            pool.write(2, &chunks, Duration::from_millis(10)).unwrap(),
            chunks.iter().map(Vec::len).sum::<usize>()
        );
        assert_idle(&pool, &state);
        assert_eq!(pool.completions.lock().unwrap().capacity(), capacity);
        assert_eq!(state.borrow().pump_count, 0);
        assert_eq!(state.borrow().submissions.len(), chunks.len());
        assert!(state.borrow().cancellations.is_empty());
    }

    #[test]
    fn out_pool_submission_failure_drains_primed_or_refilled_slots_and_can_retry() {
        for fail_at in [0, 3, 10] {
            let state = Rc::new(RefCell::new(MockState {
                fail_submit_at: Some((fail_at, LIBUSB_ERROR_NO_DEVICE)),
                ..Default::default()
            }));
            let mut pool = pool(&state);
            assert!(matches!(
                pool.write(2, &chunks(25), Duration::from_millis(10)),
                Err(Error::Disconnected)
            ));
            assert_idle(&pool, &state);
            assert_eq!(state.borrow().submit_attempts, fail_at + 1);
            state.borrow_mut().fail_submit_at = None;
            assert!(pool
                .write(2, &chunks(12), Duration::from_millis(10))
                .is_ok());
            assert_idle(&pool, &state);
            assert_eq!(state.borrow().allocations.len(), PIPELINE_DEPTH);
        }
        let state = Rc::new(RefCell::new(MockState {
            fail_submit_at: Some((2, LIBUSB_ERROR_NO_MEM)),
            ..Default::default()
        }));
        let mut pool = pool(&state);
        assert!(matches!(
            pool.write(2, &chunks(9), Duration::from_millis(10)),
            Err(Error::Usb(rusb::Error::NoMem))
        ));
        assert_idle(&pool, &state);
    }

    #[test]
    fn out_pool_completion_errors_stop_submit_ahead_and_preserve_the_first_error() {
        for (status, actual) in [
            (LIBUSB_TRANSFER_STALL, None),
            (LIBUSB_TRANSFER_NO_DEVICE, None),
            (LIBUSB_TRANSFER_TIMED_OUT, None),
            (LIBUSB_TRANSFER_ERROR, None),
            (LIBUSB_TRANSFER_COMPLETED, Some(0)),
        ] {
            let state = Rc::new(RefCell::new(MockState {
                outcomes: VecDeque::from([(status, actual)]),
                ..Default::default()
            }));
            let mut pool = pool(&state);
            let error = pool
                .write(2, &chunks(12), Duration::from_millis(10))
                .unwrap_err();
            match status {
                LIBUSB_TRANSFER_STALL => assert!(matches!(error, Error::Stall)),
                LIBUSB_TRANSFER_NO_DEVICE => assert!(matches!(error, Error::Disconnected)),
                LIBUSB_TRANSFER_TIMED_OUT => assert!(matches!(error, Error::Timeout)),
                LIBUSB_TRANSFER_COMPLETED => {
                    assert!(matches!(
                        error,
                        Error::ShortWrite {
                            wrote: 0,
                            wanted: 1
                        }
                    ));
                }
                _ => assert!(matches!(error, Error::Usb(rusb::Error::Other))),
            }
            assert_idle(&pool, &state);
            assert_eq!(state.borrow().submissions.len(), PIPELINE_DEPTH);
            assert_eq!(state.borrow().cancellations.len(), PIPELINE_DEPTH - 1);
            assert!(pool.write(8, &chunks(2), Duration::from_millis(10)).is_ok());
            assert_idle(&pool, &state);
        }
    }

    #[test]
    fn out_pool_deadline_waits_for_late_cancel_callbacks_before_reusing_buffers() {
        let state = Rc::new(RefCell::new(MockState {
            hold_until_cancel: true,
            delayed_cancel_pumps: 3,
            ..Default::default()
        }));
        let mut pool = pool(&state);
        assert!(matches!(
            pool.write(2, &chunks(12), Duration::ZERO),
            Err(Error::Timeout)
        ));
        assert_idle(&pool, &state);
        assert_eq!(state.borrow().cancellations.len(), PIPELINE_DEPTH);
        assert_eq!(state.borrow().remaining_cancel_pumps, 0);
        state.borrow_mut().hold_until_cancel = false;
        assert!(pool
            .write(2, &chunks(12), Duration::from_millis(10))
            .is_ok());
        assert_idle(&pool, &state);
        assert_eq!(state.borrow().allocations.len(), PIPELINE_DEPTH);
    }

    #[test]
    fn out_pool_frame_guard_drains_during_unwinding() {
        let state = Rc::new(RefCell::new(MockState {
            panic_once: true,
            ..Default::default()
        }));
        let mut pool = pool(&state);
        let chunks = chunks(12);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = pool.write(2, &chunks, Duration::from_millis(10));
        }));
        assert!(result.is_err());
        assert_idle(&pool, &state);
        assert_eq!(state.borrow().cancellations.len(), PIPELINE_DEPTH);
        assert!(pool.write(2, &chunks, Duration::from_millis(10)).is_ok());
        assert_idle(&pool, &state);
    }

    #[test]
    fn out_pool_drop_drains_before_freeing_transfer_or_callback_storage() {
        let state = Rc::new(RefCell::new(MockState::default()));
        let mut pool = pool(&state);
        let bytes = vec![0x5a; 128];
        pool.submit(0, 2, &bytes, Duration::from_millis(10))
            .unwrap();
        drop(pool);
        let state = state.borrow();
        assert!(state.pending.is_empty());
        assert_eq!(state.cancellations.len(), 1);
        assert_eq!(state.frees.len(), PIPELINE_DEPTH);
    }
}

#[cfg(test)]
mod selected_dock_tests {
    use super::*;
    fn layout() -> Vec<InterfaceLayout> {
        vec![
            InterfaceLayout {
                number: 0,
                alternate: 0,
                class: 0xff,
                subclass: 0,
                protocol: 3,
                endpoints: vec![
                    (2, rusb::TransferType::Bulk),
                    (0x84, rusb::TransferType::Bulk),
                ],
            },
            InterfaceLayout {
                number: 1,
                alternate: 0,
                class: 0xfe,
                subclass: 1,
                protocol: 1,
                endpoints: vec![],
            },
            InterfaceLayout {
                number: 2,
                alternate: 0,
                class: 1,
                subclass: 1,
                protocol: 0x20,
                endpoints: vec![(0x83, rusb::TransferType::Interrupt)],
            },
            InterfaceLayout {
                number: 5,
                alternate: 0,
                class: 2,
                subclass: 0x0d,
                protocol: 0,
                endpoints: vec![(0x86, rusb::TransferType::Interrupt)],
            },
        ]
    }
    #[test]
    fn actual_dock_audio_and_network_are_not_display_endpoints() {
        let mut interfaces = layout();
        assert!(validate_ella_layout(1, &interfaces).is_ok());
        let ep = interfaces[0].endpoints.pop().unwrap();
        interfaces[2].endpoints.push(ep);
        assert!(validate_ella_layout(1, &interfaces).is_err());
        assert!(validate_ella_layout(2, &layout()).is_err());
    }
    #[test]
    fn physical_selector_rejects_addresses_and_incomplete_paths() {
        assert_eq!(parse_port("8-1.2").unwrap(), (8, vec![1, 2]));
        for p in ["", "008:002", "8-", "8-0", "0-1", "8-1/2", "8-1.2:1.0"] {
            assert!(parse_port(p).is_err(), "{p}");
        }
    }
}
