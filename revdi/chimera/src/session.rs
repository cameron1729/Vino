// SPDX-License-Identifier: GPL-2.0-or-later

//! Owned userspace Vino control session.
//!
//! Protocol messages and cryptographic operations come from the kernel Vino sources compiled
//! through [`crate::kvino`]. This module owns transport ordering, counters, deadlines, and replies.

use crate::kvino;
use crate::kvino::DockProfile;
use crate::MAX_CONNECTORS;
use std::time::{Duration, Instant};
use vino_driver::{Dock, Error as UsbError};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
const REPLY_TIMEOUT: Duration = Duration::from_millis(8);
const HDCP_HPRIME_WAIT: Duration = Duration::from_micros(165_000);
/// How long to keep draining EP84 for the reply that echoes a request's counter. The driver allows
/// the same 64 ms before calling a probe round unanswered.
const PROBE_REPLY_DEADLINE: Duration = Duration::from_millis(64);
/// How long to wait on a single EP84 read while a downstream connector is authenticating.
///
/// The driver's own per-connector wait. A dock that answers each step with a push takes longer than
/// the ordinary reply timeout to produce one, so the shorter value reads empty and moves on.
const PERHEAD_READ_WAIT: Duration = Duration::from_millis(30);
/// How long to keep waiting for the push a step expects before giving up on it.
const PERHEAD_PUSH_WAIT: Duration = Duration::from_millis(480);
/// Frames to drain from EP84 after each control message, matching the kernel driver's
/// `drain_ep84`. The dock queues unprompted pushes alongside acknowledgements.
const EP84_DRAIN_READS: usize = 16;
const SET_INTERFACE: u8 = 0x0b;
const GET_DESCRIPTOR: u8 = 0x06;

const STREAM_OPEN: [u8; 64] = [
    0x00, 0x00, 0x1c, 0x00, 0x02, 0x00, 0x00, 0x00, 0x24, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x04, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x1c, 0x00, 0x02, 0x00, 0x00, 0x00, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x05, 0x00, 0x0e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// The last cursor a connector was given: its bitmap, and where it was put.
#[derive(Clone)]
struct CursorShot {
    width: u16,
    height: u16,
    bgra: Vec<u8>,
    x: u16,
    y: u16,
    visible: bool,
}

struct SessionKeys {
    control_key: kernel::crypto::Secret<16>,
    control_nonce: [u8; 8],
    next_counter: u16,
    rsa: kvino::RsaPublicKey,
    receiver_id_list: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SharedSetupStep {
    Fetch(u8),
    Hello,
    Engage(u8),
    Poll,
    Open(u8),
    Capability(u8),
}

/// The phase order from native session/setup.rs, with measured spacing from the profile.
fn shared_setup_steps(profile: DockProfile) -> Vec<SharedSetupStep> {
    use SharedSetupStep::*;
    let mut steps: Vec<_> = (0..profile.connectors()).map(Fetch).collect();
    steps.push(Hello);
    steps.extend((0..profile.connectors()).map(Engage));
    for head in 0..profile.connectors() {
        steps.extend((0..profile.setup_polls_before_open(head)).map(|_| Poll));
        steps.push(Open(head));
    }
    steps.extend((0..profile.setup_polls_after_opens()).map(|_| Poll));
    steps.extend((0..profile.connectors()).map(Capability));
    steps
}

fn setup_finalizers(
    profile: DockProfile,
    authenticated: &[bool; MAX_CONNECTORS],
) -> impl Iterator<Item = (u16, u16, u8)> + '_ {
    (0..profile.connectors())
        .filter(move |&head| !profile.video_on_ctrl_pipe() && authenticated[usize::from(head)])
        .flat_map(|head| {
            kvino::CP_SETUP_FINALIZE_STEPS
                .into_iter()
                .map(move |(id, sub)| (id, sub, head))
        })
}

/// One authenticated userspace control session.
pub struct ControlSession {
    dock: Dock,
    /// What this dock is, out of the driver's own profile table: strip size, record framing,
    /// delivery counts, endpoints and every other per-dock fact this session reads.
    profile: DockProfile,
    keys: SessionKeys,
    wire_seq: u32,
    inner_counter: u16,
    video_keys: [[u8; 24]; MAX_CONNECTORS],
    /// EDIDs collected during Ella's dock-wide setup, consumed by each connector's first fetch.
    setup_edids: [Option<Vec<u8>>; MAX_CONNECTORS],
    /// Whether each connector still owes its stream a report restating the mode.
    ///
    /// The frame after a mode set carries it. On a dock that shares its pipes it is the only
    /// report the stream wants, and the vendor sends fourteen across seven thousand frames.
    mode_restate_owed: [bool; MAX_CONNECTORS],
    /// Ella starts blanked; release it after the carrier and two picture presentations.
    unblank_owed: [bool; MAX_CONNECTORS],
    /// The mode each connector was last programmed with, which its stream reports restate.
    programmed_mode: [(u16, u16); MAX_CONNECTORS],
    /// Which connectors currently hold a programmed mode.
    ///
    /// Read only by the dock-wide guard in [`ControlSession::activate_mode`]; a dock that
    /// reconfigures itself whole cannot have one connector programmed while another is lit.
    mode_active: [bool; MAX_CONNECTORS],
    /// Running AES block counter of each connector's sealed video records.
    ///
    /// The records a connector's stream is opened with share one counter, and a message rebuilt
    /// from block zero after the stream has started reuses a keystream block the dock has already
    /// accounted for.
    video_seal_seq: [u32; MAX_CONNECTORS],
    frame_seq: [u32; MAX_CONNECTORS],
    /// Reused only after submission (including timeout retries) has drained every USB transfer.
    /// One pool serves all heads because a session presents them sequentially.
    frame_packets: FramePackets,
    /// Per-head content shadow and retransmit ledger; see [`crate::scanout`].
    scanout: [crate::scanout::HeadScanout; MAX_CONNECTORS],
    /// The cursor each connector was last given, so a mode set can put it back.
    ///
    /// Whatever leaves the dock's framebuffer undefined leaves its cursor bitmap undefined with
    /// it. A compositor does not resend a cursor whose shape has not changed, so a connector whose
    /// mode was programmed after the pointer was uploaded keeps no pointer at all -- and having
    /// taken the cursor out of band, nothing draws one into the frame either.
    cursor_shot: [Option<CursorShot>; MAX_CONNECTORS],
}

/// Pipeline timings; USB completion is not a monitor presentation timestamp.
#[derive(Clone, Copy, Debug, Default)]
pub struct PresentStats {
    pub encode_time: Duration,
    pub usb_time: Duration,
    pub bytes: usize,
    pub presentations: u32,
    /// Newly changed content, excluding strips sent only to refresh another dock buffer.
    pub changed_strips: usize,
    /// Strip-aligned half-open [x0, y0, x1, y1] bounds, not exact changed pixels.
    pub changed_bounds: Option<[usize; 4]>,
}

impl ControlSession {
    /// Open the dock and establish its encrypted control channel and downstream streams.
    pub fn engage() -> Result<Self, String> {
        // The device names its own family; the driver's profile table says what that family is,
        // and the transport is told only the endpoints that follow. A device whose family the
        // table declines is left alone rather than driven on a guess.
        let mut placed: Option<DockProfile> = None;
        let dock = Dock::open(|family, product| {
            let profile = match family {
                Some(family) => DockProfile::for_family(family.into()),
                None => DockProfile::for_product(product),
            }?;
            placed = Some(profile);
            Some(vino_driver::Placement {
                name: profile.name(),
                video_endpoints: profile.video_endpoints(),
                connectors: profile.connectors(),
            })
        })
        .map_err(|e| format!("open DisplayLink dock: {e}"))?;
        let profile = placed.expect("a placed device carries its profile");
        // Drive the link as deep as the dock can carry. The depth reaches the dock through the
        // set-mode, the decoder configuration and the codec's escape ceilings, and this is the one
        // place that decides it so those three cannot disagree.
        let profile = if profile.hdr_capable() {
            profile.ten_bit()
        } else {
            profile
        };
        Self::engage_opened(dock, profile)
    }

    /// The bounded single-head trial only supports the explicitly selected Ella dock.
    pub fn engage_ella_at(port: &str) -> Result<Self, String> {
        let profile = DockProfile::for_family(kvino::Family::Ella).expect("Ella profile");
        let dock = Dock::open_ella_at(
            port,
            vino_driver::Placement {
                name: profile.name(),
                video_endpoints: profile.video_endpoints(),
                connectors: profile.connectors(),
            },
        )
        .map_err(|e| format!("open selected Ella dock: {e}"))?;
        Self::engage_opened(dock, profile)
    }

    fn engage_opened(dock: Dock, profile: DockProfile) -> Result<Self, String> {
        device_open_preamble(&dock)?;
        session_init(&dock)?;
        let mut keys = authenticate(&dock)?;
        let (wire_seq, inner_counter, video_keys, acknowledgements) =
            configure_control(&dock, profile, &mut keys)?;
        if acknowledgements == 0 {
            return Err("dock did not acknowledge the encrypted control session".into());
        }
        let mut session = Self {
            dock,
            profile,
            keys,
            wire_seq,
            inner_counter,
            video_keys,
            setup_edids: core::array::from_fn(|_| None),
            mode_restate_owed: [false; MAX_CONNECTORS],
            unblank_owed: [false; MAX_CONNECTORS],
            programmed_mode: [(0, 0); MAX_CONNECTORS],
            mode_active: [false; MAX_CONNECTORS],
            video_seal_seq: [0; MAX_CONNECTORS],
            frame_seq: [0; MAX_CONNECTORS],
            frame_packets: FramePackets::default(),
            scanout: core::array::from_fn(|_| crate::scanout::HeadScanout::new(profile)),
            cursor_shot: core::array::from_fn(|_| None),
        };
        if profile.video_on_ctrl_pipe() {
            session.setup_shared_streams()?;
        }
        Ok(session)
    }

    /// How many connectors this dock actually backs. Every per-head loop is bounded by this,
    /// never by [`MAX_CONNECTORS`], which is only an array size.
    pub fn connectors(&self) -> usize {
        self.dock.connectors()
    }

    /// This dock's profile, chosen from its identity descriptor.
    pub fn profile(&self) -> DockProfile {
        self.profile
    }

    /// Fetch one downstream head's EDID without losing the session counters.
    pub fn fetch_edid(&mut self, head: u8) -> Result<Vec<u8>, crate::edid_fetch::FetchError> {
        if usize::from(head) >= self.connectors() {
            return Err(crate::edid_fetch::FetchError::NoEdid { head });
        }
        if self.profile.video_on_ctrl_pipe() {
            if let Some(edid) = self.setup_edids[usize::from(head)].take() {
                return Ok(edid);
            }
            return crate::edid_fetch::fetch_shared_edid(
                &self.dock,
                &self.keys.control_key,
                &self.keys.control_nonce,
                &mut self.wire_seq,
                &mut self.inner_counter,
                head,
            )?
            .ok_or(crate::edid_fetch::FetchError::NoEdid { head });
        }
        crate::edid_fetch::fetch_edid(
            &self.dock,
            self.profile,
            &self.keys.control_key,
            &self.keys.control_nonce,
            &mut self.wire_seq,
            &mut self.inner_counter,
            head,
        )
    }

    /// Native Ella setup walks each phase across both connectors before changing phase. Stream
    /// opens consume video block zero here; the later decoder configuration must start at one.
    fn setup_shared_streams(&mut self) -> Result<(), String> {
        for step in shared_setup_steps(self.profile) {
            if std::env::var_os("VINO_SUPERVISED").is_some() {
                eprintln!("chimera-setup: {step:?}, counter {}", self.inner_counter);
            }
            let content = match step {
                SharedSetupStep::Fetch(head) => {
                    self.setup_edids[usize::from(head)] = crate::edid_fetch::fetch_shared_edid(
                        &self.dock,
                        &self.keys.control_key,
                        &self.keys.control_nonce,
                        &mut self.wire_seq,
                        &mut self.inner_counter,
                        head,
                    )
                    .map_err(|e| e.to_string())?;
                    continue;
                }
                SharedSetupStep::Hello => kvino::session_hello(self.inner_counter).to_vec(),
                SharedSetupStep::Engage(head) => kvino::edid_engage_req(self.inner_counter, head)
                    .map_err(build_error("shared-pipe engage"))?,
                SharedSetupStep::Poll => kvino::device_query_req(self.inner_counter, 0x000c)
                    .map_err(build_error("setup status"))?,
                SharedSetupStep::Open(head) => {
                    let (key, nonce) = self.video_key(usize::from(head)).ok_or("invalid head")?;
                    let mut sequence = self.video_seal_seq[usize::from(head)];
                    let (open, announce) =
                        kvino::shared_stream_open(self.profile, head, &key, &nonce, &mut sequence)
                            .map_err(build_error("shared-pipe stream open"))?;
                    self.send_plain_video(head, &open)?;
                    self.send_plain_video(head, &announce)?;
                    self.video_seal_seq[usize::from(head)] = sequence;
                    drain_acknowledgements(&self.dock, &self.keys, EP84_DRAIN_READS);
                    continue;
                }
                SharedSetupStep::Capability(head) => {
                    kvino::post_edid_query(self.inner_counter, head)
                        .map_err(build_error("connector capability"))?
                }
            };
            let id = u16::from_le_bytes([content[0], content[1]]);
            self.send_control(id, &content)?;
            drain_acknowledgements(&self.dock, &self.keys, EP84_DRAIN_READS);
        }
        Ok(())
    }

    /// Probe whether a monitor is attached to one downstream head.
    ///
    /// Presence is bit `0x1000` of the reply's status word (inner byte 23 bit 4): `05 11 27 00` for
    /// an occupied connector, `05 01 <20|21|60|61> 00` for an empty one. **Which handler answered
    /// says nothing about it** -- the dock replies `id=0x44` either way, and reading presence from
    /// the id reports every head as connected.
    pub fn probe_head_present(&mut self, head: u8) -> Result<Option<bool>, String> {
        let request_counter = self.inner_counter;
        let message = kvino::get_edid_req_sub(self.inner_counter, 0x20, head)
            .map_err(build_error("monitor presence probe"))?;
        let Some(reply) = self.send_control_echoing(0x15, &message, request_counter)? else {
            return Ok(None);
        };
        let Some((_, status, _)) =
            kvino::probe_reply_status(&self.keys.control_key, &self.keys.control_nonce, &reply)
        else {
            return Ok(None);
        };
        Ok(Some(status & 0x0000_1000 != 0))
    }

    /// Send the steady-state heartbeat and reap its paired reply.
    pub fn heartbeat(&mut self) -> Result<(), String> {
        let message =
            kvino::heartbeat(self.inner_counter).map_err(|e| format!("build heartbeat: {e}"))?;
        self.send_control(0x16, &message).map(|_| ())
    }

    /// Send the high-cadence device-status poll that keeps the control session live.
    pub fn keepalive_poll(&mut self) -> Result<(), String> {
        self.poll_status()
    }

    /// Consume replies already collected by libusb while video was in flight. This sends no
    /// control traffic and does not wait for USB; firmware diagnostic pushes otherwise sit queued
    /// until the next keepalive, which can be later than a failed frame.
    pub fn drain_pending_replies(&self) -> Result<(), String> {
        for _ in 0..64 {
            let Some(reply) = receive_optional(&self.dock, Duration::ZERO)? else {
                break;
            };
            trace_control_reply(&self.keys, &reply);
        }
        Ok(())
    }

    /// Borrow the transport for video submission.
    pub fn dock(&self) -> &Dock {
        &self.dock
    }

    /// Whether `head` still owes strip retransmissions, so the caller must re-present its last
    /// surface even with nothing new from the compositor. See [`crate::scanout`].
    pub fn owes_repaint(&self, head: u8) -> bool {
        self.scanout
            .get(usize::from(head))
            .is_some_and(crate::scanout::HeadScanout::owes_retransmission)
    }

    /// The surface size the codec tiles, which is the mode rounded up to whole strips.
    ///
    /// A partial strip at the right or bottom edge is still a whole strip on the wire, and the
    /// strip is not the same size on every dock.
    fn padded(&self, width: usize, height: usize) -> (usize, usize) {
        let (strip_w, strip_h) = self.profile.strip_dims();
        (
            width.div_ceil(strip_w) * strip_w,
            height.div_ceil(strip_h) * strip_h,
        )
    }

    /// The report `head` owes its stream for the frame it is about to send.
    ///
    /// The frame right after a mode set restates the mode; on a dock that shares its pipes that
    /// restatement is the only report it wants at all, and the vendor sends it with the third
    /// frame of the stream rather than the first.
    fn frame_report(
        &mut self,
        head: u8,
        mode: (u16, u16),
        sequence: u32,
    ) -> Result<Vec<u8>, String> {
        let head_index = usize::from(head);
        let Some((video_key, video_nonce)) = self.video_key(head_index) else {
            return Ok(Vec::new());
        };
        let restate = self.mode_restate_owed[head_index];
        if restate && self.profile.video_on_ctrl_pipe() && sequence < 2 {
            return Ok(Vec::new());
        }
        let report = kvino::stream_report(
            self.profile,
            head,
            &video_key,
            &video_nonce,
            &mut self.video_seal_seq[head_index],
            mode.0,
            mode.1,
            restate,
        )
        .map_err(build_error("stream report"))?;
        if restate && report.is_some() {
            self.mode_restate_owed[head_index] = false;
        }
        Ok(report.unwrap_or_default())
    }

    /// Send a shared-pipe dock's stream prologue: the ring descriptor, then its sealed decoder
    /// configuration. Both are ordinary control records on such a dock, so they are ordered
    /// against the mode-set markers rather than glued to the front of a frame. Docks with a video
    /// pipe of their own carry theirs with the first frame and get nothing here.
    fn send_stream_prologue(
        &mut self,
        head: u8,
        video_key: &[u8; 16],
        video_nonce: &[u8; 8],
        width: u16,
        height: u16,
    ) -> Result<(), String> {
        let Some(ring) = kvino::stream_ring_record(self.profile, head) else {
            return Ok(());
        };
        self.send_plain_video(head, &ring)?;
        let config = kvino::stream_config_message(
            self.profile,
            head,
            video_key,
            video_nonce,
            &mut self.video_seal_seq[usize::from(head)],
            width,
            height,
        )
        .map_err(build_error("decoder configuration"))?;
        if let Some(config) = config {
            self.send_plain_video(head, &config)?;
        }
        Ok(())
    }

    /// Return the content key and nonce for `head`.
    pub fn video_key(&self, head: usize) -> Option<([u8; 16], [u8; 8])> {
        let material = self.video_keys.get(head)?;
        let mut key = [0; 16];
        let mut nonce = [0; 8];
        key.copy_from_slice(&material[..16]);
        nonce.copy_from_slice(&material[16..]);
        Some((key, nonce))
    }

    /// Program a mode and train the connector's video endpoint, following the driver's
    /// `activate_head`.
    ///
    /// Every choice in the bracket below comes from the dock's own profile rather than from one
    /// dock's measurements: which sink state precedes the timing, which two follow it, whether the
    /// pipe is cleared first, what opens the stream, and how much flat carrier is presented before
    /// the first picture. Sending one dock's sequence to another is not a degraded picture -- it is
    /// a dock that accepts every byte of every frame and lights nothing, or one that resets itself
    /// a few seconds later.
    /// Program every connector this dock should be driving, in one reconfiguration.
    ///
    /// A dock that reconfigures itself whole cannot have one connector programmed while another is
    /// lit: doing so resets it and takes the desktop with it. So every lit connector comes down
    /// first and the whole set is programmed from dark, which is what the vendor does and what
    /// `activate_mode` declines to do on its own.
    pub fn activate_modes(&mut self, requests: &[(u8, usize, usize, u32)]) -> Result<(), String> {
        for head in 0..MAX_CONNECTORS {
            if self.mode_active[head] {
                let (width, height) = self.programmed_mode[head];
                self.deactivate_mode(head as u8, usize::from(width), usize::from(height))?;
            }
        }
        for &(head, width, height, refresh_hz) in requests {
            self.program_mode(head, width, height, refresh_hz)?;
        }
        Ok(())
    }

    /// Program one connector's mode.
    ///
    /// `Ok(false)` means the dock declined it rather than that anything failed: see the
    /// dock-wide guard below.
    pub fn activate_mode(
        &mut self,
        head: u8,
        width: usize,
        height: usize,
        refresh_hz: u32,
    ) -> Result<bool, String> {
        // On a dock that reconfigures itself whole, one connector cannot be programmed while
        // another is lit. Say so rather than resetting the dock; the caller gathers the whole set
        // and comes back through `activate_modes`.
        if self.profile.dock_wide_modeset()
            && self
                .mode_active
                .iter()
                .enumerate()
                .any(|(other, active)| *active && other != usize::from(head))
        {
            return Ok(false);
        }
        self.program_mode(head, width, height, refresh_hz)
    }

    /// Program one connector's mode, with no regard for what else this dock is driving.
    fn program_mode(
        &mut self,
        head: u8,
        width: usize,
        height: usize,
        refresh_hz: u32,
    ) -> Result<bool, String> {
        let width16 = u16::try_from(width).map_err(|_| "mode width exceeds the wire format")?;
        let height16 = u16::try_from(height).map_err(|_| "mode height exceeds the wire format")?;
        let refresh16 =
            u16::try_from(refresh_hz).map_err(|_| "refresh rate exceeds the wire format")?;
        let head_index = usize::from(head);
        let (video_key, video_nonce) = self
            .video_key(head_index)
            .ok_or_else(|| format!("invalid Vino head {head}"))?;
        let (padded_width, padded_height) = self.padded(width, height);
        // Every frame here opens the stream, so none of them carries the steady-state record bit.
        let opening_profile = self.profile.opening();
        let prompt = kvino::black_frame_ep08(opening_profile, padded_width, padded_height, head)
            .map_err(build_error("black training frame"))?;
        let prefix = kvino::stream_prefix(
            self.profile,
            head,
            &video_key,
            &video_nonce,
            &mut self.video_seal_seq[head_index],
            width16,
            height16,
        )
        .map_err(build_error("video stream opening"))?;

        // The bracket the timing is programmed inside. A connector arriving from a blank needs it
        // exactly as much as one being configured cold, because it is the sink that has to be
        // retrained onto the new timing.
        self.unblank_owed[head_index] = self.profile.video_on_ctrl_pipe();
        self.send_marker(head, 0x2f, 1)?;
        if let Some(state) = self.profile.pre_mode_sink_state() {
            self.send_marker(head, 0x2e, state)?;
        }
        self.poll_status()?;
        if self.profile.clear_mode_before_set() {
            let clear =
                kvino::clear_mode(self.inner_counter, head).map_err(build_error("clear mode"))?;
            self.send_control(0x48, &clear)?;
        }
        // Build after the preceding markers/polls have advanced the inner counter.
        let mode = kvino::set_mode_profile(
            self.profile,
            self.inner_counter,
            head,
            width16,
            height16,
            refresh16,
        )
        .map_err(build_error("mode profile"))?;
        self.send_control(0x48, &mode)?;
        // Anchor AFTER the first status poll, not before it. The whole activation bracket is paced
        // against this anchor over 125 ms, but the poll's USB write can block for far longer than
        // that -- measured at 1.9 s on a dock that had just reported its monitor. Anchoring first
        // meant every later wait_mode_offset() was already past its deadline and returned
        // immediately, collapsing the bracket into a couple of milliseconds. The dock answers that
        // by resetting.
        self.poll_status()?;
        let anchor = Instant::now();
        self.wait_mode_offset(anchor, 5);
        self.send_marker(head, 0x2f, 1)?;
        self.wait_mode_offset(anchor, 9);
        self.send_marker(head, 0x2e, self.profile.post_mode_sink_state(0))?;
        self.wait_mode_offset(anchor, 12);
        self.send_marker(head, 0x2f, 1)?;
        self.wait_mode_offset(anchor, 14);
        self.send_marker(head, 0x2e, self.profile.post_mode_sink_state(1))?;
        // A shared-pipe dock's ring descriptor and decoder configuration land here, between the
        // fourth marker and the fifth, so the closing `2e 0` below is the last thing the dock sees
        // before pixels. A dock told to bring its sink up after a frame has already gone out has
        // been handed that frame with nothing scanning it out.
        self.send_stream_prologue(head, &video_key, &video_nonce, width16, height16)?;
        self.wait_mode_offset(anchor, 20);
        self.send_marker(head, 0x2f, 1)?;
        self.poll_status()?;
        self.wait_mode_offset(anchor, 26);
        self.send_marker(head, 0x2e, 0)?;
        self.wait_mode_offset(anchor, 89);
        self.poll_status()?;
        self.wait_mode_offset(anchor, 95);
        self.poll_status()?;
        self.wait_mode_offset(anchor, 110);
        self.poll_status()?;
        // If anything stalled badly enough that the bracket is no longer being paced, abandon and
        // let the caller retry rather than sending a compressed sequence the dock will reject.
        let late = anchor.elapsed();
        if late > Duration::from_millis(300) {
            return Err(format!(
                "activation bracket overran ({} ms before first video); retrying",
                late.as_millis()
            ));
        }
        self.programmed_mode[head_index] = (width16, height16);
        self.mode_restate_owed[head_index] = true;
        self.frame_seq[head_index] = 0;
        let mut carrier_seq: u32 = 0;
        // The frame that carries the stream's opening carries no report: the opening has just said
        // everything the report says, and a second copy spends a block the dock has accounted for.
        self.frame_packets
            .rebuild(opening_profile, &prefix, &[], &prompt, head, carrier_seq);
        self.submit_video(head, self.frame_packets.chunks())?;
        if !self.profile.video_on_ctrl_pipe() {
            for _ in 0..2 {
                let commit = kvino::stream_commit(self.inner_counter, head)
                    .map_err(build_error("stream commit"))?;
                self.send_control(0x16, &commit).map(|_| ())?;
            }
        }
        // Dedicated-pipe docks close here. Ella releases blanking after two picture
        // presentations in present_rgb_with_encoder, matching the measured cold-start tail.
        if !self.profile.video_on_ctrl_pipe() {
            self.wait_mode_offset(anchor, 123);
            self.send_marker(head, 0x2f, 0)?;
            self.wait_mode_offset(anchor, 125);
            self.send_marker(head, 0x2e, 0)?;
        }

        // Flat carrier frames, so the downstream link has something to train on before the first
        // picture. Where the dock states a count, that count is the vendor's own and every extra
        // frame walks its ring one slot further than the vendor walks it -- the dock then presents
        // a slot holding the carrier rather than the one holding the picture. Where it states none,
        // the carrier is bounded by the window it was measured over instead.
        let mut next_status = Instant::now();
        let deadline = anchor + Duration::from_millis(700);
        // The opening above already presented one carrier; the profile counts the whole burst.
        let mut frames_left = self.profile.carrier_frames().map(|n| n.saturating_sub(1));
        // A stated count is sent in full. It walks the dock's ring by exactly as many slots as the
        // vendor walks it, so stopping short leaves the ring behind where the dock expects it and
        // the first picture is written to a slot it is not reading -- which it answers by not
        // completing the transfer at all. Bounding a stated count by a deadline made that depend on
        // how busy the machine was in the preceding hundred milliseconds.
        while frames_left.is_none_or(|frames| frames > 0) {
            if frames_left.is_none() && Instant::now() >= deadline {
                break;
            }
            carrier_seq = carrier_seq.wrapping_add(1);
            let report = self.frame_report(head, (width16, height16), carrier_seq)?;
            self.frame_packets
                .rebuild(opening_profile, &[], &report, &prompt, head, carrier_seq);
            self.submit_video(head, self.frame_packets.chunks())?;
            // Advanced behind the submission, never ahead of it: a sequence spent on a frame that
            // did not reach the dock leaves its ring a slot ahead of what it has been sent.
            self.frame_seq[head_index] = carrier_seq;
            frames_left = frames_left.map(|frames| frames - 1);
            if Instant::now() >= next_status {
                self.poll_status()?;
                next_status = Instant::now() + Duration::from_millis(16);
            }
        }
        // The first picture takes the sequence after the last carrier, not the same one. The
        // trailer's phase (`seq % dock_buffers`) is how the dock steps buffers, so repeating the
        // sequence writes the picture into the buffer the dock is still presenting, and it answers
        // by not completing the transfer -- but only when the repeat lands on that buffer, which is
        // why it came and went with how many carrier frames the bracket had time for.
        self.frame_seq[head_index] = carrier_seq.wrapping_add(1);
        self.mode_active[head_index] = true;
        // The dock now holds the black training carrier, not a desktop, so the next presented frame
        // must be a full keyframe whatever the compositor changed. The same programming left its
        // cursor bitmap undefined, so the pointer is put back with it -- the two invalidations
        // belong together, and separating them is how a connector ends up with no pointer at all.
        self.scanout[head_index].owe_keyframe();
        self.rearm_cursor(head)?;
        Ok(true)
    }

    /// Encode and present one padded RGB frame, sending only what the dock still needs.
    ///
    /// A still desktop puts nothing on the wire at all, which is what DLM does and what the dock's
    /// buffer rotation assumes; see [`crate::scanout`].
    pub fn present_rgb(
        &mut self,
        head: u8,
        width: usize,
        height: usize,
        rgb: &[u8],
    ) -> Result<(), String> {
        self.present_rgb_with_encoder(
            head,
            width,
            height,
            rgb,
            &mut crate::scanout::CpuStripEncoder,
        )
        .map(|_| ())
    }

    /// Present using the caller's persistent encoder; errors leave the presentation ledger owed.
    pub fn present_rgb_with_encoder<E: crate::scanout::StripEncoder + ?Sized>(
        &mut self,
        head: u8,
        width: usize,
        height: usize,
        rgb: &[u8],
        encoder: &mut E,
    ) -> Result<PresentStats, String>
    where
        E::Error: core::fmt::Display,
    {
        self.present_surface_with_encoder(
            head,
            crate::surface::SurfaceView::rgb(width, height, rgb)
                .map_err(|_| "invalid padded RGB surface")?,
            encoder,
        )
    }

    /// Re-present pixels known to match this head's previous presentation input exactly.
    ///
    /// Includes pixels composited by the caller, such as a software cursor. New/resized
    /// surfaces are still hashed; all required transmissions and failure recovery are retained.
    pub fn present_unchanged_rgb_with_encoder<E: crate::scanout::StripEncoder + ?Sized>(
        &mut self,
        head: u8,
        width: usize,
        height: usize,
        rgb: &[u8],
        encoder: &mut E,
    ) -> Result<PresentStats, String>
    where
        E::Error: core::fmt::Display,
    {
        self.present_unchanged_surface_with_encoder(
            head,
            crate::surface::SurfaceView::rgb(width, height, rgb)
                .map_err(|_| "invalid padded RGB surface")?,
            encoder,
        )
    }

    /// Present a native or packed logical image with the supplied encoder.
    pub fn present_surface_with_encoder<E: crate::scanout::StripEncoder + ?Sized>(
        &mut self,
        head: u8,
        surface: crate::surface::SurfaceView<'_>,
        encoder: &mut E,
    ) -> Result<PresentStats, String>
    where
        E::Error: core::fmt::Display,
    {
        self.present_surface_inner(head, surface, false, None, encoder)
    }

    /// Re-present an unchanged logical image, retaining the existing retransmission ledger.
    pub fn present_unchanged_surface_with_encoder<E: crate::scanout::StripEncoder + ?Sized>(
        &mut self,
        head: u8,
        surface: crate::surface::SurfaceView<'_>,
        encoder: &mut E,
    ) -> Result<PresentStats, String>
    where
        E::Error: core::fmt::Display,
    {
        self.present_surface_inner(head, surface, true, None, encoder)
    }

    /// Present native pixels already compared exactly by the consuming thread. The previous
    /// capture lease can be released before this call; only current pixels reach the GPU.
    pub fn present_native_revisions_with_encoder<E: crate::scanout::StripEncoder + ?Sized>(
        &mut self,
        head: u8,
        surface: crate::surface::NativeSurface<'_>,
        changes: &crate::native_changes::NativeChanges,
        encoder: &mut E,
    ) -> Result<PresentStats, String>
    where
        E::Error: core::fmt::Display,
    {
        self.present_surface_inner(
            head,
            crate::surface::SurfaceView::Native(surface),
            false,
            Some(changes),
            encoder,
        )
    }

    fn present_surface_inner<E: crate::scanout::StripEncoder + ?Sized>(
        &mut self,
        head: u8,
        surface: crate::surface::SurfaceView<'_>,
        unchanged: bool,
        native_changes: Option<&crate::native_changes::NativeChanges>,
        encoder: &mut E,
    ) -> Result<PresentStats, String>
    where
        E::Error: core::fmt::Display,
    {
        let (width, height) = (surface.width(), surface.height());
        let started = Instant::now();
        let mut stats = PresentStats::default();
        let (sw, sh) = self.profile.strip_dims();
        if width == 0
            || height == 0
            || width > 65535
            || height > 65535
            || !width.is_multiple_of(sw)
            || !height.is_multiple_of(sh)
            || !surface.is_valid()
        {
            return Err("invalid padded surface".into());
        }
        let head_index = usize::from(head);
        if head_index >= self.connectors() {
            return Err(format!("invalid Vino head {head}"));
        }
        let plan = if let Some(changes) = native_changes {
            let crate::surface::SurfaceView::Native(native) = surface else {
                return Err("native revisions require a native surface".into());
            };
            self.scanout[head_index]
                .plan_native_revisions(native, changes)
                .map_err(|_| "native revision geometry does not match the surface")?
        } else if unchanged {
            self.scanout[head_index].plan_unchanged_view(surface)
        } else {
            self.scanout[head_index].plan_view(surface)
        };
        (stats.changed_strips, stats.changed_bounds) = self.scanout[head_index].content_changes();
        let Some(frame) = self.scanout[head_index]
            .encode_view_with(&plan, surface, head, encoder)
            .map_err(|e| format!("video frame: {e}"))?
        else {
            // Nothing moved and nothing is owed. DLM puts zero bytes on the wire here.
            stats.encode_time = started.elapsed();
            if !self.profile.video_on_ctrl_pipe() {
                self.poll_status()?;
            }
            return Ok(stats);
        };

        // Each presentation carries the same image with a freshly advanced trailer, whose phase
        // (`seq % dock_buffers`) is how the dock steps to the next buffer.
        stats.encode_time = started.elapsed();
        let usb_started = Instant::now();
        let sequence = self.frame_seq[head_index];
        let mode = self.programmed_mode[head_index];
        let next_sequence = deliver_presentations(frame.presentations, sequence, |seq| {
            // Native Ella separates ring copies by one display period on the shared pipe.
            if seq != sequence && self.profile.video_on_ctrl_pipe() {
                std::thread::sleep(Duration::from_millis(self.profile.frame_period_ms() as u64));
            }
            let report = self.frame_report(head, mode, seq)?;
            self.frame_packets
                .rebuild(self.profile, &[], &report, &frame.records, head, seq);
            let sent = self.submit_video(head, self.frame_packets.chunks());
            // Keep the original transfer error even if the device disappeared during diagnosis.
            let _ = self.drain_pending_replies();
            sent?;
            stats.bytes += self.frame_packets.bytes;
            let mut unblank_owed = self.unblank_owed[head_index];
            finish_ella_activation(&mut unblank_owed, seq, |sub, state| {
                self.send_marker(head, sub, state)
            })?;
            self.unblank_owed[head_index] = unblank_owed;
            Ok(())
        })?;
        stats.usb_time = usb_started.elapsed();
        stats.presentations = frame.presentations;
        self.frame_seq[head_index] = next_sequence;
        // Published only now: every early return and transport error above deliberately leaves the
        // previous dock-visible state intact, so the next frame repairs it.
        self.scanout[head_index].presented(&plan);
        // The live loop owns the profile's keepalive interval. An inline poll on Ella inserts a
        // control transaction into every frame boundary, where its vendor leaves the pipe quiet.
        if !self.profile.video_on_ctrl_pipe() {
            self.poll_status()?;
        }
        Ok(stats)
    }

    /// Upload a complete cursor image for one head.
    pub fn set_cursor(
        &mut self,
        head: u8,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) -> Result<(), String> {
        let width = u16::try_from(width).map_err(|_| "cursor width exceeds the wire format")?;
        let height = u16::try_from(height).map_err(|_| "cursor height exceeds the wire format")?;
        let create = kvino::cursor_create(self.inner_counter, head, width, height)
            .map_err(build_error("cursor create"))?;
        self.send_control(0x1b, &create)?;
        let image = kvino::cursor_image(self.inner_counter, head, width, height, bgra)
            .map_err(build_error("cursor image"))?;
        self.send_control(0x1c, &image)?;
        if let Some(slot) = self.cursor_shot.get_mut(usize::from(head)) {
            let (x, y, visible) = slot
                .as_ref()
                .map_or((0, 0, true), |s| (s.x, s.y, s.visible));
            *slot = Some(CursorShot {
                width,
                height,
                bgra: bgra.to_vec(),
                x,
                y,
                visible,
            });
        }
        Ok(())
    }

    /// Put back the cursor a connector was last given.
    ///
    /// A mode set leaves the dock's cursor bitmap undefined along with its framebuffer, and the
    /// compositor will not resend a shape that has not changed, so the pointer has to be replayed
    /// from here or the connector simply has none.
    fn rearm_cursor(&mut self, head: u8) -> Result<(), String> {
        let Some(shot) = self
            .cursor_shot
            .get(usize::from(head))
            .and_then(|slot| slot.clone())
        else {
            return Ok(());
        };
        self.set_cursor(
            head,
            u32::from(shot.width),
            u32::from(shot.height),
            &shot.bgra,
        )?;
        if shot.visible {
            self.move_cursor(head, i32::from(shot.x), i32::from(shot.y))
        } else {
            self.hide_cursor(head)
        }
    }

    /// Move a previously uploaded cursor.
    pub fn move_cursor(&mut self, head: u8, x: i32, y: i32) -> Result<(), String> {
        let x = u16::try_from(x.clamp(0, i32::from(u16::MAX)))
            .map_err(|_| "cursor x position exceeds the wire format")?;
        let y = u16::try_from(y.clamp(0, i32::from(u16::MAX)))
            .map_err(|_| "cursor y position exceeds the wire format")?;
        let message = kvino::cursor_move(self.inner_counter, head, x, y, true)
            .map_err(build_error("cursor move"))?;
        self.send_control(0x1a, &message)?;
        if let Some(Some(shot)) = self.cursor_shot.get_mut(usize::from(head)) {
            shot.x = x;
            shot.y = y;
            shot.visible = true;
        }
        Ok(())
    }

    /// Hide a head's cursor by clearing the dock's visible flag.
    ///
    /// Parking it off-screen instead leaves a ghost pointer at the top-left of the panel: the dock
    /// wraps an out-of-range origin rather than clipping the cursor away.
    pub fn hide_cursor(&mut self, head: u8) -> Result<(), String> {
        let message = kvino::cursor_move(self.inner_counter, head, 0, 0, false)
            .map_err(build_error("cursor hide"))?;
        self.send_control(0x1a, &message)?;
        if let Some(Some(shot)) = self.cursor_shot.get_mut(usize::from(head)) {
            shot.visible = false;
        }
        Ok(())
    }

    /// Blank and close a head's active stream.
    pub fn deactivate_mode(&mut self, head: u8, width: usize, height: usize) -> Result<(), String> {
        // Whatever this leaves the dock holding, it is not what the shadow says it is.
        if let Some(state) = self.scanout.get_mut(usize::from(head)) {
            state.owe_keyframe();
        }
        if let Some(active) = self.mode_active.get_mut(usize::from(head)) {
            *active = false;
        }
        if let Some(owed) = self.unblank_owed.get_mut(usize::from(head)) {
            *owed = false;
        }

        // A dock that wants its bracket held gets two markers and then silence. Presenting black
        // at it instead halts its shared pipe: the session dies, nothing else reaches the dock, and
        // the panel stays lit on the last image it decoded.
        if self.profile.blank_holds_bracket() {
            self.send_marker(head, 0x2f, 1)?;
            return self.send_marker(head, 0x2e, self.profile.sink_down_state());
        }

        let (padded_width, padded_height) = self.padded(width, height);
        let black = kvino::black_frame_ep08(self.profile, padded_width, padded_height, head)
            .map_err(build_error("black shutdown frame"))?;
        // Present for long enough to reach every dock buffer. One presentation lands in one buffer
        // only -- the same reason the retransmit debt exists -- so a one-shot blank leaves another
        // buffer holding the frozen desktop and the panel alternates between black and stale
        // content.
        let deadline = Instant::now() + Duration::from_millis(120);
        let mut next_status = Instant::now();
        while Instant::now() < deadline {
            self.submit_video(head, &black)?;
            if Instant::now() >= next_status {
                self.poll_status()?;
                next_status = Instant::now() + Duration::from_millis(16);
            }
        }
        self.send_marker(head, 0x2f, 0)?;
        self.send_marker(head, 0x2e, 0)?;
        // Black frames alone leave the panel lit on a black image: the dock goes on scanning out
        // whatever it last decoded, and only powering the downstream sink down ends the signal.
        self.send_marker(head, 0x2f, 1)?;
        self.send_marker(head, 0x2e, self.profile.sink_down_state())?;
        self.poll_status()?;
        self.send_marker(head, 0x2f, 0)
    }

    /// Forward one Revdi DDC/CI transaction to a downstream monitor.
    pub fn ddcci(
        &mut self,
        head: u8,
        address: u16,
        flags: u16,
        payload: &[u8],
    ) -> Result<Vec<u8>, String> {
        const I2C_M_RD: u16 = 0x0001;

        if address != crate::ddcci::I2C_ADDR {
            return Err(format!("unsupported DDC/CI address {address:#04x}"));
        }
        if flags & I2C_M_RD == 0 {
            let message = crate::ddcci::forward(self.inner_counter, head, payload)
                .map_err(build_error("DDC/CI write"))?;
            self.send_control(0x36, &message)?;
            return Ok(Vec::new());
        }

        let message = crate::ddcci::read_request(self.inner_counter, head);
        let reply = self
            .send_control(0x15, &message)?
            .ok_or_else(|| "dock did not return a DDC/CI read reply".to_string())?;
        crate::ddcci::parse_reply(&self.keys.control_key, &self.keys.control_nonce, &reply)
            .map_err(build_error("DDC/CI reply"))?
            .ok_or_else(|| "dock returned an invalid DDC/CI read reply".to_string())
    }

    fn send_control(&mut self, id: u16, content: &[u8]) -> Result<Option<Vec<u8>>, String> {
        if std::env::var_os("VINO_SUPERVISED").is_some() {
            eprintln!(
                "chimera-control: tx id={id:04x} sub={:04x} counter={} blocks={}",
                u16::from_le_bytes([content[2], content[3]]),
                u16::from_le_bytes([content[4], content[5]]),
                self.wire_seq
            );
        }
        let frame = kvino::seal_interactive(
            &self.keys.control_key,
            &self.keys.control_nonce,
            id,
            self.wire_seq,
            content,
        )
        .map_err(|e| format!("seal control message {id:#06x}: {e}"))?;
        self.dock
            .write_ctrl_raw(&frame)
            .map_err(|e| format!("send control message {id:#06x}: {e}"))?;
        self.wire_seq = self
            .wire_seq
            .wrapping_add(content.len().div_ceil(16) as u32);
        self.inner_counter = self.inner_counter.wrapping_add(1);
        let reply = receive_optional(&self.dock, REPLY_TIMEOUT)?;
        if let Some(frame) = reply.as_ref() {
            trace_control_reply(&self.keys, frame);
        }
        Ok(reply)
    }

    /// Send a control message and return the reply that answers *this* one.
    ///
    /// Taking simply the next frame off EP84 is wrong twice over: the dock interleaves unprompted
    /// pushes with its answers, and its reply to one message routinely arrives only after the next
    /// message has gone out -- measured here as every probe reply echoing the *previous* request's
    /// counter, so back-to-back head probes each read the other head's answer and the wrong
    /// monitor is reported connected. Drain until a reply echoes this request's counter, as the
    /// driver's `probe_connector_present` does, and treat never seeing it as "learned nothing"
    /// rather than as an answer.
    fn send_control_echoing(
        &mut self,
        id: u16,
        content: &[u8],
        request_counter: u16,
    ) -> Result<Option<Vec<u8>>, String> {
        let mut reply = self.send_control(id, content)?;
        let deadline = Instant::now() + PROBE_REPLY_DEADLINE;
        loop {
            if let Some(frame) = reply {
                if kvino::decode_in_lenient(
                    &self.keys.control_key,
                    &self.keys.control_nonce,
                    &frame,
                )
                .is_some_and(|(_, _, echoed)| echoed == request_counter)
                {
                    return Ok(Some(frame));
                }
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            // An empty read is not the end of the answer: keep draining until the deadline.
            reply = receive_optional(&self.dock, REPLY_TIMEOUT)?;
        }
    }

    fn send_marker(&mut self, head: u8, sub: u16, state: u8) -> Result<(), String> {
        let message = kvino::stream_marker(self.inner_counter, head, sub, state)
            .map_err(build_error("stream marker"))?;
        self.send_control(0x16, &message).map(|_| ())
    }

    fn poll_status(&mut self) -> Result<(), String> {
        let message = kvino::device_query_req(self.inner_counter, 0x000c)
            .map_err(build_error("status poll"))?;
        self.send_control(0x14, &message).map(|_| ())
    }

    fn wait_mode_offset(&self, anchor: Instant, offset_ms: u64) {
        wait_until(anchor, Duration::from_millis(offset_ms));
    }

    /// Write one already-framed record straight to the control endpoint.
    ///
    /// The stream prologue of a dock that shares its pipes is built as a complete record -- the
    /// ring descriptor unsealed, the decoder configuration sealed with the connector's own video
    /// key -- so it goes out as it is rather than through the control seal.
    fn send_plain_video(&self, head: u8, record: &[u8]) -> Result<(), String> {
        self.dock
            .write_ctrl_raw(record)
            .map(|_| ())
            .map_err(|e| format!("send head {head} stream prologue: {e}"))
    }

    fn submit_video(&self, head: u8, frames: &[Vec<u8>]) -> Result<(), String> {
        match self.dock.write_video_frame(usize::from(head), frames) {
            Ok(_) => Ok(()),
            // A dock that has just been opened can hold its video endpoints stalled for a second or
            // two before it accepts anything on them, and answers a write in that window by not
            // completing it. Offer the frame once more rather than tearing the session down: the
            // stall clears itself, and rebuilding the session pays for a whole authentication to
            // arrive back at the same endpoint.
            Err(UsbError::Timeout) => self
                .dock
                .write_video_frame(usize::from(head), frames)
                .map(|_| ())
                .map_err(|e| format!("submit head {head} video: {e}")),
            Err(e) => Err(format!("submit head {head} video: {e}")),
        }
    }
}

/// Run the standalone control-session smoke test.
pub fn run_coldstart() {
    match ControlSession::engage() {
        Ok(_) => println!("Vino control session engaged"),
        Err(e) => {
            eprintln!("failed to engage Vino control session: {e}");
            std::process::exit(1);
        }
    }
}

fn device_open_preamble(dock: &Dock) -> Result<(), String> {
    dock.claim_interface(1)
        .map_err(|e| format!("claim interface 1: {e}"))?;
    let _ = dock.vendor_in(0xfe, 0, 1, 16);
    let _ = dock.vendor_in(0xfc, 0, 1, 3);
    let _ = dock.std_out_iface(SET_INTERFACE, 0, 1, &[], CONTROL_TIMEOUT);
    dock.vendor_out(0x24, 3, 0, &[])
        .map_err(|e| format!("start dock application: {e}"))?;
    dock.vendor_in(0x22, 1, 0, 28)
        .map_err(|e| format!("read dock state: {e}"))?;
    Ok(())
}

fn session_init(dock: &Dock) -> Result<(), String> {
    // These descriptor requests are part of the receiver's observed initialization fingerprint.
    let _ = dock.std_in(GET_DESCRIPTOR, 0x0200, 0, 40, CONTROL_TIMEOUT);
    let _ = dock.std_in(GET_DESCRIPTOR, 0x0200, 0, 618, CONTROL_TIMEOUT);
    send_plain(dock, &kvino::init_0().map_err(build_error("init_0"))?)?;
    send_plain(dock, &kvino::init_25().map_err(build_error("init_25"))?)?;
    let _ = dock.std_in(GET_DESCRIPTOR, 0x0300, 0, 255, CONTROL_TIMEOUT);
    let _ = dock.std_in(GET_DESCRIPTOR, 0x0303, 0x0409, 255, CONTROL_TIMEOUT);
    send_plain(
        dock,
        &kvino::init_4_probe().map_err(build_error("init_4_probe"))?,
    )?;
    dock.recv_frame_raw_timeout(4096, CONTROL_TIMEOUT)
        .map_err(|e| format!("receive session-init acknowledgement: {e}"))?;
    Ok(())
}

fn authenticate(dock: &Dock) -> Result<SessionKeys, String> {
    use kvino::id;

    let mut sequence = 1u32;
    send_plain(
        dock,
        &kvino::session_init_ack(sequence, 0).map_err(build_error("session_init_ack"))?,
    )?;
    pace_cap_ack(dock, sequence as u16);
    sequence += 1;

    let mut rtx = [0; 8];
    kvino::rng::fill(&mut rtx);
    send_plain(
        dock,
        &kvino::ake_init(sequence, 0, &rtx, &[0; 3]).map_err(build_error("AKE_Init"))?,
    )?;
    sequence += 1;

    let (message_id, certificate_message) = receive_hdcp(dock)?;
    if message_id != id::AKE_SEND_CERT || certificate_message.len() < 137 {
        return Err(format!(
            "invalid AKE_Send_Cert message {message_id:#04x} ({} bytes)",
            certificate_message.len()
        ));
    }
    let repeater = certificate_message[0] != 0;
    let certificate = &certificate_message[1..];
    let mut modulus = [0; 128];
    modulus.copy_from_slice(&certificate[5..133]);
    let mut exponent = [0; 3];
    exponent.copy_from_slice(&certificate[133..136]);

    send_plain(
        dock,
        &kvino::ake_transmitter_info(sequence, 0).map_err(build_error("AKE_Transmitter_Info"))?,
    )?;
    sequence += 1;
    let _ = receive_hdcp(dock)?;

    let mut km = [0; 16];
    kvino::rng::fill(&mut km);
    let mut rsa =
        kvino::rsa_public_key(&modulus, &exponent).map_err(build_error("RSA public key"))?;
    let encrypted_km = kvino::oaep_encrypt_km(&mut rsa, &km).map_err(build_error("OAEP"))?;
    send_plain(
        dock,
        &kvino::ake_no_stored_km(sequence, 0, &encrypted_km)
            .map_err(build_error("AKE_No_Stored_km"))?,
    )?;
    sequence += 1;

    let (message_id, receiver_random) = receive_hdcp(dock)?;
    if message_id != id::AKE_SEND_RRX || receiver_random.len() < 8 {
        return Err(format!("invalid AKE_Send_Rrx message {message_id:#04x}"));
    }
    let mut rrx = [0; 8];
    rrx.copy_from_slice(&receiver_random[..8]);
    let kd = kvino::derive_kd(&km, &rtx, &rrx).map_err(build_error("derive kd"))?;

    let (message_id, h_prime) = receive_hdcp(dock)?;
    if message_id != id::AKE_SEND_H_PRIME
        || h_prime.len() < 32
        || kvino::compute_h(&kd, &rtx, repeater)[..] != h_prime[..32]
    {
        return Err("receiver H' verification failed".into());
    }
    let _ = receive_hdcp(dock)?;

    let mut rn = [0; 8];
    kvino::rng::fill(&mut rn);
    send_plain(
        dock,
        &kvino::lc_init(sequence, 0, &rn).map_err(build_error("LC_Init"))?,
    )?;
    sequence += 1;
    let (message_id, l_prime) = receive_hdcp(dock)?;
    if message_id != id::LC_SEND_L_PRIME
        || l_prime.len() < 32
        || kvino::compute_l(&kd, &rrx, &rn)[..] != l_prime[..32]
    {
        return Err("receiver L' verification failed".into());
    }

    let mut raw_session_key = [0; 16];
    let mut delivered_nonce = [0; 8];
    kvino::rng::fill(&mut raw_session_key);
    kvino::rng::fill(&mut delivered_nonce);
    let encrypted_session_key = kvino::compute_eks(&km, &rtx, &rrx, &rn, &raw_session_key)
        .map_err(build_error("SKE key derivation"))?;
    send_plain(
        dock,
        &kvino::ske_send_eks(sequence, 0, &encrypted_session_key, &delivered_nonce)
            .map_err(build_error("SKE_Send_Eks"))?,
    )?;
    sequence += 1;

    let mut receiver_id_list = Vec::new();
    if repeater {
        let (message_id, list) = receive_hdcp(dock)?;
        if message_id != id::REPEATERAUTH_SEND_RECEIVERID_LIST || list.len() < 16 {
            return Err(format!(
                "invalid RepeaterAuth_ReceiverID_List message {message_id:#04x}"
            ));
        }
        let split = list.len() - 16;
        let v = kvino::compute_v_full(&kd, &list[..split]);
        if v[..16] != list[split..] {
            return Err("receiver V' verification failed".into());
        }
        receiver_id_list.extend_from_slice(&list[..split]);
        let mut acknowledgement = [0; 16];
        acknowledgement.copy_from_slice(&v[16..]);
        send_plain(
            dock,
            &kvino::repeater_auth_send_ack(sequence, 0, &acknowledgement)
                .map_err(build_error("RepeaterAuth_Send_Ack"))?,
        )?;
        pace_cap_ack(dock, sequence as u16);
        sequence += 1;
        send_plain(
            dock,
            &kvino::repeater_auth_stream_manage(sequence, 0)
                .map_err(build_error("RepeaterAuth_Stream_Manage"))?,
        )?;
        pace_cap_ack(dock, sequence as u16);
        sequence += 1;
        wait_cap_complete(dock);
    }

    let mut control_nonce = delivered_nonce;
    control_nonce[7] ^= 0x04;
    Ok(SessionKeys {
        control_key: kvino::cp_session_key(&raw_session_key),
        control_nonce,
        next_counter: sequence as u16,
        rsa,
        receiver_id_list,
    })
}

fn configure_control(
    dock: &Dock,
    profile: DockProfile,
    keys: &mut SessionKeys,
) -> Result<(u32, u16, [[u8; 24]; MAX_CONNECTORS], usize), String> {
    send_plain(dock, &STREAM_OPEN)?;
    let mut wire_seq = 0u32;
    let mut inner_counter = keys.next_counter;
    let mut acknowledgements = 0usize;

    let first = kvino::session_hello(inner_counter);
    let body_len = first.len() + 16;
    let mut header = [0u8; 16];
    header[2..4].copy_from_slice(&((16 + body_len - 4) as u16).to_le_bytes());
    header[4..8].copy_from_slice(&4u32.to_le_bytes());
    header[8..10].copy_from_slice(&0x24u16.to_le_bytes());
    header[10..12].copy_from_slice(&kvino::aux_for_id(0x14, body_len).to_le_bytes());
    let frame = kvino::seal_livemac(&keys.control_key, &keys.control_nonce, &header, &first)
        .map_err(build_error("first encrypted control message"))?;
    send_plain_retry(dock, &frame)?;
    acknowledgements += drain_acknowledgements(dock, keys, 8);
    inner_counter = inner_counter.wrapping_add(1);
    wire_seq += 2;

    // The dock-wide records that precede the per-connector blocks, and one connector-selecting
    // `0x16/0x2a` for every connector the dock backs -- not a fixed pair, which left a four-socket
    // dock's last two slots uninitialised and every later counter four AES blocks adrift. A dock
    // whose profile does not ask for them is put out of step by exactly the same amount if they
    // are sent anyway, so this follows the profile rather than sending them to everything.
    let dock_wide: Vec<(u16, u16, Vec<u8>)> = if profile.dock_wide_init() {
        let mut records = vec![
            (0x0014u16, 0x0030u16, Vec::new()),
            (0x0015, 0x000b, vec![0x01]),
        ];
        records.extend(
            (0..dock.connectors() as u8)
                .map(|connector| (0x0016u16, 0x002au16, vec![connector, 0x01])),
        );
        records
    } else {
        Vec::new()
    };
    for (id, sub, prefix) in dock_wide {
        let mut content = [0; 32];
        content[..2].copy_from_slice(&id.to_le_bytes());
        content[2..4].copy_from_slice(&sub.to_le_bytes());
        content[4..6].copy_from_slice(&inner_counter.to_le_bytes());
        kvino::rng::fill(&mut content[22..]);
        content[22..22 + prefix.len()].copy_from_slice(&prefix);
        send_interactive(dock, keys, id, wire_seq, &content)?;
        acknowledgements += drain_acknowledgements(dock, keys, 2);
        inner_counter = inner_counter.wrapping_add(1);
        wire_seq += 2;
    }

    // Where a dock wants its video engine brought up at this exact authenticated boundary -- after
    // the reply to `0x15/0x0b`, before the connector-selecting records -- doing it after
    // finalisation instead moves the same requests tens of messages later and the dock never
    // starts its pipes.
    if profile.commits_video_before_connector_records() {
        commit_video_engine(dock)?;
    }

    // Bounded by what the device backs, not by an array size: a dock with fewer connectors must
    // not be sent per-head setup for connectors it does not have.
    let mut video_keys = [[0u8; 24]; MAX_CONNECTORS];
    let mut authenticated = [false; MAX_CONNECTORS];
    for head in 0..dock.connectors() {
        authenticated[head] = configure_head(
            dock,
            profile,
            keys,
            head as u8,
            &mut wire_seq,
            &mut inner_counter,
            &mut video_keys[head],
            &mut acknowledgements,
        )?;
        if !authenticated[head] {
            eprintln!("chimera: socket {} has no downstream sink", head + 1);
        }
    }

    // Three finalization messages per head, head-major, exactly as the driver repeats
    // `CP_SETUP_FINALIZE_STEPS` for each connector that authenticated. A connector that never
    // authenticated has nothing to finalise.
    let finalize = setup_finalizers(profile, &authenticated);
    for (id, sub, selector) in finalize {
        let mut content = [0; 32];
        content[..2].copy_from_slice(&id.to_le_bytes());
        content[2..4].copy_from_slice(&sub.to_le_bytes());
        content[4..6].copy_from_slice(&inner_counter.to_le_bytes());
        content[22] = selector;
        if sub == 0x004c {
            content[23] = 1;
            kvino::rng::fill(&mut content[24..]);
        } else {
            kvino::rng::fill(&mut content[23..]);
        }
        send_interactive(dock, keys, id, wire_seq, &content)?;
        acknowledgements += drain_acknowledgements(dock, keys, 2);
        inner_counter = inner_counter.wrapping_add(1);
        wire_seq += 2;
    }

    if !profile.commits_video_before_connector_records() {
        dock.vendor_out(0x24, 0, 0, &[])
            .map_err(|e| format!("commit encrypted session: {e}"))?;
        dock.vendor_in(0x22, 1, 0, 28)
            .map_err(|e| format!("read encrypted session state: {e}"))?;
    }
    // Native setup commits once. Ella shares EP02 between control and video, so the extra
    // video-endpoint clears also touched its live control pipe. Neither operation belongs here.
    acknowledgements += drain_acknowledgements(dock, keys, 16);

    Ok((wire_seq, inner_counter, video_keys, acknowledgements))
}

/// Bring the dock's video engine up: clear both video endpoints, then commit and read back the
/// encrypted session state.
///
/// The pause between the two endpoint clears and the one behind the read are the vendor's own; the
/// dock's pipes are still settling and the next record goes out into them.
fn commit_video_engine(dock: &Dock) -> Result<(), String> {
    dock.clear_video_halt(0)
        .map_err(|e| format!("clear video endpoint 0: {e}"))?;
    std::thread::sleep(Duration::from_millis(13));
    dock.clear_video_halt(1)
        .map_err(|e| format!("clear video endpoint 1: {e}"))?;
    dock.vendor_out(0x24, 0, 0, &[])
        .map_err(|e| format!("commit encrypted session: {e}"))?;
    dock.vendor_in(0x22, 1, 0, 28)
        .map_err(|e| format!("read encrypted session state: {e}"))?;
    std::thread::sleep(Duration::from_millis(3));
    Ok(())
}

/// The downstream-HDCP push a dock that answers per step sends back for setup message `index`.
///
/// Steps past the authentication exchange are acknowledged rather than pushed to, so they have no
/// expected id and are drained as before.
fn perhead_expected_push(index: usize) -> Option<u8> {
    use kvino::ake::id;
    /// DisplayLink's `AKE_Receiver_Info`, the dock's answer to the transmitter info this sends.
    const AKE_RECEIVER_INFO: u8 = 0x14;
    Some(match index {
        0 => id::AKE_SEND_CERT,
        1 => AKE_RECEIVER_INFO,
        2 => id::AKE_SEND_RRX,
        3 => id::LC_SEND_L_PRIME,
        4 => id::REPEATERAUTH_SEND_RECEIVERID_LIST,
        5 => id::RECEIVER_AUTH_STATUS,
        _ => return None,
    })
}

#[allow(clippy::too_many_arguments)]
fn configure_head(
    dock: &Dock,
    profile: DockProfile,
    keys: &mut SessionKeys,
    head: u8,
    wire_seq: &mut u32,
    inner_counter: &mut u16,
    video_key: &mut [u8; 24],
    acknowledgements: &mut usize,
) -> Result<bool, String> {
    let mut rtx = [0; 8];
    let mut km = [0; 16];
    let mut rn = [0; 8];
    let mut raw_video_key = [0; 16];
    let mut delivered_nonce = [0; 8];
    kvino::rng::fill(&mut rtx);
    kvino::rng::fill(&mut km);
    kvino::rng::fill(&mut rn);
    kvino::rng::fill(&mut raw_video_key);
    kvino::rng::fill(&mut delivered_nonce);
    let encrypted_km = kvino::oaep_encrypt_km(&mut keys.rsa, &km).map_err(build_error("OAEP"))?;
    let whitened_video_key = kvino::cp_session_key(&raw_video_key);
    video_key[..16].copy_from_slice(&whitened_video_key[..]);
    video_key[16..].copy_from_slice(&kvino::video_content_nonce(profile, &delivered_nonce, head));

    let mut receiver_random = None;
    let mut encrypted_session_key = None;
    let mut v_ack = None;
    for (index, (id, sub, content_len)) in kvino::CP_SETUP_PER_HEAD.into_iter().enumerate() {
        if index >= 3 && encrypted_session_key.is_none() {
            // No Rrx means this connector never began a downstream authentication, which is what an
            // empty DisplayPort socket looks like: the vendor runs one AKE for the dock and none for
            // a connector with no sink. Leave the connector unauthenticated and carry on, rather
            // than failing the device -- aborting here took the whole dock down whenever a single
            // socket was empty, so a dock with a monitor on one connector never came up at all.
            let Some(rrx) = receiver_random else {
                announce_content_stream(dock, profile, head)?;
                return Ok(false);
            };
            let kd = kvino::derive_kd(&km, &rtx, &rrx).map_err(build_error("derive head kd"))?;
            encrypted_session_key = Some(
                kvino::compute_eks(&km, &rtx, &rrx, &rn, &raw_video_key)
                    .map_err(build_error("derive head SKE key"))?,
            );
            let v = kvino::compute_v_full(&kd, &keys.receiver_id_list);
            let mut tail = [0; 16];
            tail.copy_from_slice(&v[16..]);
            v_ack = Some(tail);
        }

        let content = if id == 0x0026 {
            kvino::stream_manage_restatement(profile, *inner_counter, head)
                .map_err(build_error("stream-manage restatement"))?
        } else {
            let mut content = vec![0; content_len];
            content[..2].copy_from_slice(&id.to_le_bytes());
            content[2..4].copy_from_slice(&sub.to_le_bytes());
            content[4..6].copy_from_slice(&inner_counter.to_le_bytes());
            match index {
                0..=5 => {
                    kvino::connector_marker(profile, &mut content, head);
                    content[27] = [0x02, 0x13, 0x04, 0x09, 0x0b, 0x0f][index];
                    match index {
                        0 => {
                            content[28..36].copy_from_slice(&rtx);
                            kvino::rng::fill(&mut content[36..48]);
                        }
                        1 => {
                            content[28..33].copy_from_slice(&[0, 6, 2, 0, 2]);
                            kvino::rng::fill(&mut content[33..48]);
                        }
                        2 => {
                            content[28..156].copy_from_slice(&encrypted_km);
                            kvino::rng::fill(&mut content[156..160]);
                        }
                        3 => {
                            content[28..36].copy_from_slice(&rn);
                            kvino::rng::fill(&mut content[36..48]);
                        }
                        4 => {
                            content[28..44].copy_from_slice(
                                encrypted_session_key
                                    .as_ref()
                                    .ok_or("head SKE key was not derived")?,
                            );
                            content[44..52].copy_from_slice(&delivered_nonce);
                            kvino::rng::fill(&mut content[52..64]);
                        }
                        5 => {
                            content[28..44]
                                .copy_from_slice(v_ack.as_ref().ok_or("head V was not derived")?);
                            kvino::rng::fill(&mut content[44..48]);
                        }
                        _ => unreachable!(),
                    }
                }
                7 => kvino::rng::fill(&mut content[22..]),
                8 => {
                    content[22] = head;
                    // A per-platform constant, not a connector count: two-connector docks send
                    // 0x06 and 0x10, and the four-connector one sends 0x0c.
                    content[24] = profile.strm2_marker();
                    content[25] = head * 4;
                    content[26] = 0x04;
                    kvino::rng::fill(&mut content[27..]);
                }
                _ => {}
            }
            content
        };

        let sent_at = Instant::now();
        send_interactive(dock, keys, id, *wire_seq, &content)?;
        // Drain everything the dock has queued, not just the first frame. The control plane is
        // lockstep but the dock also pushes unprompted -- the per-head AKE_Send_Rrx among them --
        // so reading a single reply per message leaves the rest queued and every later read is one
        // or more frames stale. That desynchronisation is what made head 1 miss its Rrx while head
        // 0, which happened to receive its push inside the index-2 burst, succeeded.
        //
        // A dock that answers each step with a push of its own is waited on for the push that step
        // expects, rather than for whatever arrives inside one short window. Its replies are the
        // slower of the two by more than a drain's worth of timeout, so a blind drain returns empty
        // and the authentication fails at the first field it needed.
        let expected = if profile.per_connector_onehot() {
            perhead_expected_push(index)
        } else {
            None
        };
        let deadline = Instant::now() + PERHEAD_PUSH_WAIT;
        let mut seen = None;
        for _ in 0..EP84_DRAIN_READS {
            let Some(reply) = receive_optional(dock, PERHEAD_READ_WAIT)? else {
                if expected.is_none() || Instant::now() >= deadline {
                    break;
                }
                continue;
            };
            if let Some((msg_id, payload)) =
                kvino::perhead_push(&keys.control_key, &keys.control_nonce, &reply)
            {
                if msg_id == kvino::ake::id::AKE_SEND_RRX
                    && payload.len() >= 8
                    && receiver_random.is_none()
                {
                    let mut rrx = [0u8; 8];
                    rrx.copy_from_slice(&payload[..8]);
                    receiver_random = Some(rrx);
                }
                seen = Some(msg_id);
                if expected == Some(msg_id) {
                    break;
                }
                continue;
            }
            trace_control_reply(keys, &reply);
            if is_control_ack(keys, &reply) {
                *acknowledgements += 1;
            }
        }
        if let (Some(want), None) = (expected, seen.filter(|id| expected == Some(*id))) {
            // Not fatal on its own: an empty connector answers nothing at all, and the caller
            // decides that from the missing Rrx rather than from any single step.
            eprintln!(
                "chimera: head {head} step {index} expected HDCP push {want:#04x}, none arrived"
            );
        }
        if index == 2 {
            // The dock computes H' before it will answer anything else, and the wait is the
            // vendor's own. Skipping it because this step's own push already arrived sends LC_Init
            // into a dock still finishing the AKE, which then answers no L' at all.
            wait_until(sent_at, HDCP_HPRIME_WAIT);
            for _ in 0..EP84_DRAIN_READS {
                let Some(reply) = receive_optional(dock, PERHEAD_READ_WAIT)? else {
                    break;
                };
                receiver_random = receiver_random
                    .or_else(|| kvino::perhead_rrx(&keys.control_key, &keys.control_nonce, &reply));
            }
        }
        *inner_counter = inner_counter.wrapping_add(1);
        *wire_seq = wire_seq.wrapping_add(content.len().div_ceil(16) as u32);
        // Native setup places this between Stream_Manage (0x26) and stream-open (0x14).
        // Moving it to mode activation is too late for Ella's shared control/video pipe.
        if id == 0x0026 {
            announce_content_stream(dock, profile, head)?;
        }
    }
    if encrypted_session_key.is_none() || v_ack.is_none() {
        return Err(format!(
            "downstream head {head} authentication did not complete"
        ));
    }
    Ok(true)
}

fn announce_content_stream(dock: &Dock, profile: DockProfile, head: u8) -> Result<(), String> {
    if let Some(frame) = kvino::content_stream_announcement(profile, head) {
        send_plain(dock, &frame)?;
    }
    Ok(())
}

fn send_interactive(
    dock: &Dock,
    keys: &SessionKeys,
    id: u16,
    wire_seq: u32,
    content: &[u8],
) -> Result<(), String> {
    if std::env::var_os("VINO_SUPERVISED").is_some() {
        eprintln!(
            "chimera-setup: tx id={id:04x} sub={:04x} counter={} blocks={wire_seq}",
            u16::from_le_bytes([content[2], content[3]]),
            u16::from_le_bytes([content[4], content[5]]),
        );
    }
    let frame = kvino::seal_interactive(
        &keys.control_key,
        &keys.control_nonce,
        id,
        wire_seq,
        content,
    )
    .map_err(build_error("seal interactive control message"))?;
    send_plain(dock, &frame)
}

fn send_plain(dock: &Dock, frame: &[u8]) -> Result<(), String> {
    dock.write_ctrl_dlm(frame)
        .map(|_| ())
        .map_err(|e| format!("USB control write: {e}"))
}

fn send_plain_retry(dock: &Dock, frame: &[u8]) -> Result<(), String> {
    let mut last_error = None;
    for _ in 0..40 {
        match send_plain(dock, frame) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_error = Some(e);
                let _ = receive_optional(dock, Duration::from_millis(10));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| "control write was not accepted".into()))
}

fn receive_hdcp(dock: &Dock) -> Result<(u8, Vec<u8>), String> {
    for _ in 0..24 {
        let reply = match dock.recv_frame_raw_timeout(16_384, CONTROL_TIMEOUT) {
            Ok(reply) => reply,
            Err(UsbError::Timeout) => continue,
            Err(e) => return Err(format!("receive HDCP message: {e}")),
        };
        if reply.len() < 16 || u16::from_le_bytes([reply[8], reply[9]]) != 0x25 {
            continue;
        }
        if let Some((message_id, payload)) = kvino::ake_parse_in(&reply[16..]) {
            if message_id != 0 {
                return Ok((message_id, payload.to_vec()));
            }
        }
    }
    Err("receiver did not provide the expected HDCP message".into())
}

fn pace_cap_ack(dock: &Dock, wanted_counter: u16) {
    for _ in 0..8 {
        let Ok(reply) = dock.recv_frame_raw_timeout(16_384, Duration::from_millis(30)) else {
            return;
        };
        if reply.len() >= 22
            && u16::from_le_bytes([reply[8], reply[9]]) == 0x25
            && u16::from_le_bytes([reply[16], reply[17]]) == 0x14
            && u16::from_le_bytes([reply[20], reply[21]]) == wanted_counter
        {
            return;
        }
    }
}

fn wait_cap_complete(dock: &Dock) {
    let mut saw_capability_end = false;
    let mut saw_stream_ready = false;
    let mut quiet = 0;
    for _ in 0..48 {
        match dock.recv_frame_raw_timeout(16_384, Duration::from_millis(5)) {
            Ok(reply) if reply.len() >= 20 => {
                quiet = 0;
                saw_capability_end |= u16::from_le_bytes([reply[16], reply[17]]) == 0x0b
                    && u16::from_le_bytes([reply[18], reply[19]]) == 0x84;
                saw_stream_ready |=
                    reply.len() >= 58 && reply[25] == kvino::id::REPEATERAUTH_STREAM_READY;
                if saw_capability_end && saw_stream_ready {
                    return;
                }
            }
            _ if saw_capability_end || saw_stream_ready => {
                quiet += 1;
                if quiet == 3 {
                    return;
                }
            }
            _ => {}
        }
    }
}

fn receive_optional(dock: &Dock, timeout: Duration) -> Result<Option<Vec<u8>>, String> {
    match dock.recv_frame_raw_timeout(4096, timeout) {
        Ok(reply) => Ok(Some(reply)),
        Err(UsbError::Timeout) => Ok(None),
        Err(e) => Err(format!("receive control reply: {e}")),
    }
}

fn drain_acknowledgements(dock: &Dock, keys: &SessionKeys, limit: usize) -> usize {
    let mut acknowledgements = 0;
    for _ in 0..limit {
        let Ok(Some(reply)) = receive_optional(dock, Duration::from_millis(5)) else {
            break;
        };
        trace_control_reply(keys, &reply);
        acknowledgements += usize::from(is_control_ack(keys, &reply));
    }
    acknowledgements
}

fn is_control_ack(keys: &SessionKeys, reply: &[u8]) -> bool {
    kvino::decode_in_lenient(&keys.control_key, &keys.control_nonce, reply).is_some()
}

fn trace_control_reply(keys: &SessionKeys, reply: &[u8]) {
    if std::env::var_os("VINO_SUPERVISED").is_some() {
        eprintln!(
            "chimera-setup: rx {} bytes, authenticated header {:?}",
            reply.len(),
            kvino::decode_in_lenient(&keys.control_key, &keys.control_nonce, reply),
        );
        if let Some(line) = kvino::dock_trace_line(&keys.control_key, &keys.control_nonce, reply) {
            eprintln!("chimera-dock: {line}");
        }
    }
}

fn wait_until(start: Instant, duration: Duration) {
    if let Some(remaining) = duration.checked_sub(start.elapsed()) {
        std::thread::sleep(remaining);
    }
}

fn build_error(label: &'static str) -> impl FnOnce(crate::kshim::Error) -> String {
    move |error| format!("build {label}: {error}")
}

/// Assemble directly into retained USB chunks, without an intermediate full-frame copy.
///
/// The active slice excludes chunks retained from a previous, larger frame. Storage grows only
/// when a larger frame needs it and is released with the session. USB borrows the active chunks
/// until all submissions, including cancellations and retries, have completed.
#[derive(Default)]
struct FramePackets {
    storage: Vec<Vec<u8>>,
    used: usize,
    bytes: usize,
}

impl FramePackets {
    const TRANSFER_SIZE: usize = 65_536;

    fn chunks(&self) -> &[Vec<u8>] {
        &self.storage[..self.used]
    }

    fn next_chunk(&mut self) {
        if self.used == self.storage.len() {
            self.storage.push(Vec::with_capacity(Self::TRANSFER_SIZE));
        } else {
            self.storage[self.used].clear();
        }
        self.used += 1;
    }

    fn append(&mut self, mut bytes: &[u8]) {
        self.bytes += bytes.len();
        while !bytes.is_empty() {
            if self.used == 0 || self.storage[self.used - 1].len() == Self::TRANSFER_SIZE {
                self.next_chunk();
            }
            let chunk = &mut self.storage[self.used - 1];
            let take = bytes.len().min(Self::TRANSFER_SIZE - chunk.len());
            chunk.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
        }
    }

    fn rebuild(
        &mut self,
        dock: DockProfile,
        prefix: &[u8],
        report: &[u8],
        frames: &[Vec<u8>],
        head: u8,
        seq: u32,
    ) {
        self.used = 0;
        self.bytes = 0;
        // Preserve the exact stream order, including a freshly advanced trailer on every
        // presentation. Its phase selects the dock buffer, even for retransmitted images.
        self.append(prefix);
        self.append(&kvino::frame_opener(dock, head, seq).unwrap_or_default());
        self.append(report);
        for frame in frames {
            self.append(frame);
        }
        self.append(&kvino::frame_trailer(dock, head, seq));
        if dock.split_full_packet_frame()
            && self.used != 0
            && self.storage[self.used - 1].len().is_multiple_of(1024)
        {
            // Ella cannot end a frame on a full USB packet. Reuse another chunk for the final
            // 16 bytes, retaining the original transfer boundaries and all bytes on the wire.
            self.next_chunk();
            let (previous, tail) = self.storage.split_at_mut(self.used - 1);
            let last = previous.last_mut().unwrap();
            let end = last.len() - 16;
            tail[0].extend_from_slice(&last[end..]);
            last.truncate(end);
        }
    }
}

/// Finish Ella's cold activation only after the carrier (sequence 0) and two pictures.
/// Native drm_sink/timeline.rs::ELLA_DOCK_WIDE releases 0x2f after Stream(0, 2),
/// then reasserts power state 0. Firmware trace on 12.3.37 identifies the commands
/// as SetBlanking and SetPowerSaving. Keeping 0x2f asserted leaves a cold
/// connector blanked even though it accepted the mode and enabled its output.
///
/// Call after a successful frame write. Keep the obligation on either marker error.
fn finish_ella_activation(
    pending: &mut bool,
    sequence: u32,
    mut marker: impl FnMut(u16, u8) -> Result<(), String>,
) -> Result<(), String> {
    if !*pending || sequence < 2 {
        return Ok(());
    }
    marker(0x2f, 0)?;
    marker(0x2e, 0)?;
    *pending = false;
    Ok(())
}

/// A complete frame only advances its ledger when every required presentation succeeds.
fn deliver_presentations(
    count: u32,
    first: u32,
    mut send: impl FnMut(u32) -> Result<(), String>,
) -> Result<u32, String> {
    for repeat in 0..count {
        send(first.wrapping_add(repeat))?;
    }
    Ok(first.wrapping_add(count))
}

#[cfg(test)]
mod delivery_tests {
    use super::*;

    #[test]
    fn ella_unblanks_after_two_successful_pictures_and_only_once() {
        let mut pending = true;
        let mut events = Vec::new();
        // Sequence 0 was the activation carrier. The first keyframe fills three buffers.
        deliver_presentations(3, 1, |seq| {
            events.push((seq, None));
            finish_ella_activation(&mut pending, seq, |sub, state| {
                events.push((seq, Some((sub, state))));
                Ok(())
            })
        })
        .unwrap();
        assert_eq!(
            events,
            vec![
                (1, None),
                (2, None),
                (2, Some((0x2f, 0))),
                (2, Some((0x2e, 0))),
                (3, None),
            ]
        );
        assert!(!pending);
    }

    #[test]
    fn ella_keeps_blanking_obligation_after_a_failed_frame_or_marker() {
        for failure in [0, 1, 2] {
            let mut pending = true;
            let mut markers = 0;
            let result = deliver_presentations(3, 1, |seq| {
                if failure == 0 && seq == 2 {
                    return Err("injected frame failure".into());
                }
                finish_ella_activation(&mut pending, seq, |_, _| {
                    markers += 1;
                    if markers == failure {
                        Err("injected marker failure".into())
                    } else {
                        Ok(())
                    }
                })
            });
            assert!(result.is_err());
            assert!(pending);
            assert_eq!(markers, failure);
        }
        // The carrier alone must not unblank the output.
        finish_ella_activation(&mut true, 0, |_, _| panic!("early unblank")).unwrap();
        // Other dock families have no deferred Ella obligation.
        finish_ella_activation(&mut false, 2, |_, _| panic!("non-Ella marker")).unwrap();
    }

    #[test]
    fn ella_setup_matches_native_phase_order_and_vendor_poll_spacing() {
        use SharedSetupStep::*;
        let ella = DockProfile::for_family(kvino::Family::Ella).unwrap();
        assert_eq!(
            shared_setup_steps(ella),
            vec![
                Fetch(0),
                Fetch(1),
                Hello,
                Engage(0),
                Engage(1),
                Poll,
                Poll,
                Open(0),
                Poll,
                Poll,
                Poll,
                Open(1),
                Poll,
                Poll,
                Poll,
                Capability(0),
                Capability(1),
            ],
        );
        // Ella's native/vendor transaction has no 0x16/0x4c or 0x15/0x4a finalizers.
        assert_eq!(setup_finalizers(ella, &[true; MAX_CONNECTORS]).count(), 0);
        // Dedicated-pipe docks still finalize exactly the heads which authenticated.
        let ridge = DockProfile::for_family(kvino::Family::Ridge).unwrap();
        let mut authenticated = [false; MAX_CONNECTORS];
        authenticated[0] = true;
        assert_eq!(
            setup_finalizers(ridge, &authenticated).collect::<Vec<_>>(),
            kvino::CP_SETUP_FINALIZE_STEPS
                .into_iter()
                .map(|(id, sub)| (id, sub, 0))
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn ella_full_packet_frames_split_the_tail_without_changing_the_wire_bytes() {
        let ella = DockProfile::for_family(kvino::Family::Ella).unwrap();
        let mut packets = FramePackets::default();
        for bytes in [1024, 65536, 66560, 131072] {
            let payload = vec![0x5a; bytes - 48];
            let mut expected = payload.clone();
            expected.extend_from_slice(&kvino::frame_trailer(ella, 0, 7));
            packets.rebuild(ella, &[], &[], &[payload], 0, 7);
            let chunks = packets.chunks();
            assert_eq!(chunks.concat(), expected);
            assert_eq!(chunks.last().unwrap().len(), 16);
            assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), bytes);
            assert!(chunks.iter().all(|c| !c.is_empty() && c.len() <= 65536));
        }
        // The split is harmful on Ridge, whose profile deliberately opts out.
        let ridge = DockProfile::for_family(kvino::Family::Ridge).unwrap();
        packets.rebuild(ridge, &[], &[], &[vec![0; 1024 - 96]], 0, 7);
        assert_eq!(
            packets.chunks().iter().map(Vec::len).collect::<Vec<_>>(),
            vec![1024]
        );
    }

    /// The original allocate/concatenate/split implementation, retained only as an independent
    /// byte-and-boundary oracle and component benchmark baseline.
    fn legacy_prefix_frame(
        dock: DockProfile,
        prefix: &[u8],
        report: &[u8],
        frames: &[Vec<u8>],
        head: u8,
        seq: u32,
    ) -> Vec<Vec<u8>> {
        let opener = kvino::frame_opener(dock, head, seq).unwrap_or_default();
        let trailer = kvino::frame_trailer(dock, head, seq);
        let payload_len = frames.iter().map(Vec::len).sum::<usize>();
        let mut stream = Vec::with_capacity(
            prefix.len() + opener.len() + report.len() + payload_len + trailer.len(),
        );
        stream.extend_from_slice(prefix);
        stream.extend_from_slice(&opener);
        stream.extend_from_slice(report);
        for frame in frames {
            stream.extend_from_slice(frame);
        }
        stream.extend_from_slice(&trailer);
        let mut chunks: Vec<Vec<u8>> = stream.chunks(65_536).map(<[u8]>::to_vec).collect();
        if dock.split_full_packet_frame() {
            if let Some(last) = chunks.last_mut() {
                if last.len().is_multiple_of(1024) {
                    let tail = last.split_off(last.len() - 16);
                    chunks.push(tail);
                }
            }
        }
        chunks
    }

    #[test]
    fn retained_packets_match_original_bytes_and_boundaries_for_every_profile() {
        use kvino::Family::*;
        let mut packets = FramePackets::default();
        for family in [Ella, Ridge, Navarro, Firefly] {
            let Some(profile) = DockProfile::for_family(family) else {
                continue;
            };
            for profile in [profile, profile.opening()] {
                for head in 0..profile.connectors() {
                    for seq in [0, 1, 2, 3, 254, 255, 256, u32::MAX - 1, 0] {
                        for wire_bytes in
                            [1023, 1024, 1025, 65535, 65536, 65537, 66560, 131072, 257]
                        {
                            let prefix = [0x13; 13];
                            let report = [0x37; 23];
                            let headers = prefix.len()
                                + kvino::frame_opener(profile, head, seq)
                                    .unwrap_or_default()
                                    .len()
                                + report.len()
                                + kvino::frame_trailer(profile, head, seq).len();
                            let payload: Vec<u8> =
                                (0..wire_bytes - headers).map(|i| (i * 17) as u8).collect();
                            // Fragment records independently of USB boundaries, with empty records.
                            let mut frames = vec![Vec::new()];
                            frames.extend(payload.chunks(997).map(<[u8]>::to_vec));
                            frames.push(Vec::new());
                            let expected =
                                legacy_prefix_frame(profile, &prefix, &report, &frames, head, seq);
                            packets.rebuild(profile, &prefix, &report, &frames, head, seq);
                            assert_eq!(
                                packets.chunks(),
                                expected,
                                "{family:?} head={head} seq={seq} wire_bytes={wire_bytes}"
                            );
                            assert_eq!(packets.bytes, wire_bytes);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn retained_packets_reuse_large_buffers_without_sending_stale_bytes() {
        let profile = DockProfile::for_family(kvino::Family::Ella).unwrap();
        let large = vec![vec![0xa5; 3 * 65_536 - 48]];
        let small = vec![vec![0x3c; 17]];
        let mut packets = FramePackets::default();
        packets.rebuild(profile, &[], &[], &large, 0, 255);
        let storage = packets.storage.as_ptr();
        let buffers: Vec<_> = packets
            .storage
            .iter()
            .map(|chunk| (chunk.as_ptr(), chunk.capacity()))
            .collect();
        // The largest frame ends with a split tail. Smaller frames must not leave that chunk,
        // or any other retained large-frame bytes, in the submitted slice.
        for (seq, frames) in [(0, &small), (1, &large), (2, &small), (3, &large)] {
            packets.rebuild(profile, &[], &[], frames, 1, seq);
            assert_eq!(
                packets.chunks(),
                legacy_prefix_frame(profile, &[], &[], frames, 1, seq)
            );
            assert_eq!(packets.storage.as_ptr(), storage);
            assert_eq!(
                packets
                    .storage
                    .iter()
                    .map(|chunk| (chunk.as_ptr(), chunk.capacity()))
                    .collect::<Vec<_>>(),
                buffers
            );
        }
        // A report/trailer-only presentation must also discard all prior image bytes.
        packets.rebuild(profile, &[], &[], &[], 0, 4);
        assert_eq!(
            packets.chunks(),
            legacy_prefix_frame(profile, &[], &[], &[], 0, 4)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "manual packet assembly component benchmark; no USB or display access"]
    fn packet_assembly_component_benchmark() {
        use std::hint::black_box;
        assert!(!cfg!(debug_assertions), "benchmark requires --release");
        // /proc uses CLK_TCK units. Report raw ticks rather than assuming the host's frequency;
        // CPU time belongs only to this test thread and excludes other test/compiler activity.
        fn cpu_ticks() -> u64 {
            let stat = std::fs::read_to_string("/proc/thread-self/stat").unwrap();
            stat.rsplit_once(") ")
                .unwrap()
                .1
                .split_whitespace()
                .skip(11)
                .take(2)
                .map(|field| field.parse::<u64>().unwrap())
                .sum()
        }
        let profile = DockProfile::for_family(kvino::Family::Ella).unwrap();
        for (payload, iterations) in [(4096, 100_000), (65_536, 20_000), (3_300_000, 1024)] {
            let data: Vec<u8> = (0..payload).map(|i| (i * 37) as u8).collect();
            let frames: Vec<_> = data.chunks(12_017).map(<[u8]>::to_vec).collect();
            let mut packets = FramePackets::default();
            packets.rebuild(profile, &[], &[], &frames, 0, 7);
            assert_eq!(
                packets.chunks(),
                legacy_prefix_frame(profile, &[], &[], &frames, 0, 7)
            );
            for round in 0..3 {
                for retained in if round % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                } {
                    for seq in 0..8 {
                        if retained {
                            packets.rebuild(profile, &[], &[], black_box(&frames), 0, seq);
                            black_box(packets.chunks());
                        } else {
                            black_box(legacy_prefix_frame(
                                profile,
                                &[],
                                &[],
                                black_box(&frames),
                                0,
                                seq,
                            ));
                        }
                    }
                    let cpu_start = cpu_ticks();
                    let start = Instant::now();
                    for seq in 0..iterations {
                        if retained {
                            packets.rebuild(profile, &[], &[], black_box(&frames), 0, seq);
                            black_box(packets.chunks());
                        } else {
                            black_box(legacy_prefix_frame(
                                profile,
                                &[],
                                &[],
                                black_box(&frames),
                                0,
                                seq,
                            ));
                        }
                    }
                    let elapsed = start.elapsed();
                    let cpu = cpu_ticks() - cpu_start;
                    println!(
                        "packet_bench payload={payload} frames={iterations} round={round} \
                         retained={retained} cpu_ticks={cpu} wall_ns={}",
                        elapsed.as_nanos()
                    );
                }
            }
        }
    }

    #[test]
    fn failed_submission_does_not_commit_or_continue_the_frame() {
        let profile = DockProfile::for_family(kvino::Family::Ella).unwrap();
        let mut scanout = crate::scanout::HeadScanout::new(profile);
        let rgb = vec![42; 64 * 16 * 3];
        let plan = scanout.plan(64, 16, &rgb);
        let frame = scanout.encode(&plan, &rgb, 0).unwrap().unwrap();
        let mut sequence = 7;
        let mut sent = vec![];
        let result = deliver_presentations(frame.presentations, sequence, |n| {
            sent.push(n);
            if n == 8 {
                Err("injected USB error".into())
            } else {
                Ok(())
            }
        });
        if let Ok(next) = result {
            sequence = next;
            scanout.presented(&plan);
        }
        assert_eq!(sequence, 7);
        assert_eq!(sent, vec![7, 8]);
        assert!(scanout.owes_retransmission());
        assert_eq!(scanout.plan(64, 16, &rgb), crate::scanout::Plan::Keyframe);
    }
    #[test]
    fn all_copies_advance_the_wrapping_sequence() {
        let mut sent = vec![];
        let next = deliver_presentations(3, u32::MAX - 1, |n| {
            sent.push(n);
            Ok(())
        })
        .unwrap();
        assert_eq!(sent, vec![u32::MAX - 1, u32::MAX, 0]);
        assert_eq!(next, 1);
    }
}
