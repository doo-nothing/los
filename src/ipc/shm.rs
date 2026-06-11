use std::ffi::CString;
use std::ptr;
use std::sync::atomic::{compiler_fence, AtomicU32, Ordering};

use anyhow::{Context, Result};

// ── constants ──────────────────────────────────────────────────────────────

const DEFAULT_CHANNELS: u16 = 2;
const DEFAULT_SLOT_FRAMES: u32 = 64;
const DEFAULT_NUM_SLOTS: u32 = 128;

/// Total frames buffered: 64 × 128 = 8192 ≈ 170ms at 48kHz
const SHM_DATA_SIZE: usize = DEFAULT_SLOT_FRAMES as usize
    * DEFAULT_CHANNELS as usize
    * size_of::<f32>()
    * DEFAULT_NUM_SLOTS as usize;

const DATA_OFFSET: usize = 64;
const EVENT_DATA_OFFSET: usize = 256; // larger header for 16 consumer read indices

// Names of the global SHM objects. Test builds get distinct `/los_test_*`
// names so `cargo test` can never unlink or overwrite the shared memory of a
// live session — the unit tests below create and destroy these objects
// freely, which once wiped the manifest out from under a running mixer.
#[cfg(not(test))]
const SHM_TRANSPORT_NAME: &str = "/los_transport";
#[cfg(not(test))]
const SHM_EVENTS_NAME: &str = "/los_events_v2";
#[cfg(not(test))]
const SHM_MODBUS_NAME: &str = "/los_mod";
#[cfg(not(test))]
const SHM_MANIFEST_NAME: &str = "/los_manifest";

#[cfg(test)]
const SHM_TRANSPORT_NAME: &str = "/los_test_transport";
#[cfg(test)]
const SHM_EVENTS_NAME: &str = "/los_test_events_v2";
#[cfg(test)]
const SHM_MODBUS_NAME: &str = "/los_test_mod";
#[cfg(test)]
const SHM_MANIFEST_NAME: &str = "/los_test_manifest";

// ── helpers ────────────────────────────────────────────────────────────────

/// Atomically load a u64 from shared memory.
/// Safe because aligned u64 reads are atomic on x86_64 / aarch64.
/// The `compiler_fence(Acquire)` prevents reordering with subsequent reads.
#[inline]
fn atomic_load_acquire(ptr: *const u64) -> u64 {
    let val = unsafe { ptr::read_volatile(ptr) };
    compiler_fence(Ordering::Acquire);
    val
}

/// Atomically store a u64 to shared memory with release ordering.
#[inline]
fn atomic_store_release(ptr: *mut u64, val: u64) {
    compiler_fence(Ordering::Release);
    unsafe { ptr::write_volatile(ptr, val) };
}

// ── AudioRingbuf ───────────────────────────────────────────────────────────

/// Lock-free single-producer single-consumer ringbuffer backed by POSIX SHM.
///
/// Layout in shared memory:
///   [0..8)    write_index : u64  (producer advances this)
///   [8..16)   read_index  : u64  (consumer advances this)
///   [16..64)  reserved / config padding
///   [64..)    slot data as flat f32 array
///
/// Producer (voice) writes slots at write_index, then advances it.
/// Consumer (mixer) reads slots at read_index, then advances it.
#[derive(Debug)]
pub struct AudioRingbuf {
    fd: i32,
    ptr: *mut u8,
    channels: u16,
    slot_frames: u32,
    num_slots: u32,
    slot_len: usize, // total floats per slot
    total_size: usize,
    owned: bool,
}

// The struct holds raw pointers but is single-threaded in practice.
// cpal requires the callback capture to be Send.
unsafe impl Send for AudioRingbuf {}

impl Drop for AudioRingbuf {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.total_size) };
        }
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
        if self.owned {
            let cname = CString::new(self.name()).unwrap();
            unsafe { libc::shm_unlink(cname.as_ptr()) };
        }
    }
}

impl AudioRingbuf {
    fn name(&self) -> String {
        format!("/los_audio_{}", self.fd) // placeholder; callers set via create
    }

    fn write_idx_ptr(&self) -> *mut u64 {
        self.ptr as *mut u64
    }

    fn read_idx_ptr(&self) -> *mut u64 {
        unsafe { self.ptr.add(8) as *mut u64 }
    }

    fn slot_ptr(&self, index: u64) -> *mut f32 {
        let slot = index as usize % self.num_slots as usize;
        let offset = DATA_OFFSET + slot * self.slot_len * size_of::<f32>();
        unsafe { self.ptr.add(offset) as *mut f32 }
    }

    /// Create a new shared-memory ringbuffer.
    pub fn create(name: &str) -> Result<Self> {
        let channels = DEFAULT_CHANNELS;
        let slot_frames = DEFAULT_SLOT_FRAMES;
        let num_slots = DEFAULT_NUM_SLOTS;
        let slot_len = channels as usize * slot_frames as usize;
        let data_bytes = slot_len * num_slots as usize * size_of::<f32>();
        let total_size = DATA_OFFSET + data_bytes;

        let cname = CString::new(name).context("invalid SHM name")?;

        // Create or open the shared memory object
        let fd = unsafe {
            let mut use_fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644);
            if use_fd < 0 {
                anyhow::bail!(
                    "shm_open failed for {name}: {}",
                    std::io::Error::last_os_error()
                );
            }
            // Set the size. macOS sizes a POSIX shm object exactly once:
            // re-truncating a leftover ring from a killed session can
            // fail EINVAL even at the same size. If the existing object
            // is big enough (it always is — sizes are page-rounded and
            // the layout hasn't shrunk), claim it: the header re-init
            // below makes it ours. Otherwise unlink and start over.
            if libc::ftruncate(use_fd, total_size as libc::off_t) < 0 {
                let mut st: libc::stat = std::mem::zeroed();
                let big_enough =
                    libc::fstat(use_fd, &mut st) == 0 && st.st_size >= total_size as libc::off_t;
                if !big_enough {
                    libc::close(use_fd);
                    libc::shm_unlink(cname.as_ptr());
                    use_fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644);
                    if use_fd < 0 || libc::ftruncate(use_fd, total_size as libc::off_t) < 0 {
                        if use_fd >= 0 {
                            libc::close(use_fd);
                        }
                        anyhow::bail!(
                            "recreate failed for {name}: {}",
                            std::io::Error::last_os_error()
                        );
                    }
                }
            }
            use_fd
        };

        // Memory-map the SHM
        let ptr = unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                total_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                libc::close(fd);
                libc::shm_unlink(cname.as_ptr());
                anyhow::bail!(
                    "mmap failed for {name}: {}",
                    std::io::Error::last_os_error()
                );
            }
            p as *mut u8
        };

        // Initialize header.
        // Header layout (all offsets are from ptr):
        //   0: write_index (u64, aligned 8)
        //   8: read_index  (u64, aligned 8)
        //  16: channels    (u32, aligned 4) — stored as u32 for alignment
        //  20: slot_frames (u32, aligned 4)
        //  24: num_slots   (u32, aligned 4)
        unsafe {
            ptr::write_unaligned(ptr as *mut u64, 0); // write_index
            ptr::write_unaligned(ptr.add(8) as *mut u64, 0); // read_index
            ptr::write_unaligned(ptr.add(16) as *mut u32, channels as u32);
            ptr::write_unaligned(ptr.add(20) as *mut u32, slot_frames);
            ptr::write_unaligned(ptr.add(24) as *mut u32, num_slots);
            // zero the slot data too: a claimed leftover ring from a
            // killed session holds that session's audio, and any index
            // bug would replay it — stale slots must be silence
            std::ptr::write_bytes(ptr.add(DATA_OFFSET), 0, data_bytes);
        }

        Ok(Self {
            fd,
            ptr,
            channels,
            slot_frames,
            num_slots,
            slot_len,
            total_size,
            owned: true,
        })
    }

    /// Open an existing shared-memory ringbuffer.
    pub fn open(name: &str) -> Result<Self> {
        let cname = CString::new(name).context("invalid SHM name")?;

        let fd = unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0);
            if fd < 0 {
                anyhow::bail!(
                    "shm_open failed for {name}: {}",
                    std::io::Error::last_os_error()
                );
            }
            fd
        };

        // Read config from the header
        // We need to mmap enough to read the header first, then remap
        let total_size = SHM_DATA_SIZE + DATA_OFFSET;

        let ptr = unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                total_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                libc::close(fd);
                anyhow::bail!(
                    "mmap failed for {name}: {}",
                    std::io::Error::last_os_error()
                );
            }
            p as *mut u8
        };

        let channels = unsafe { ptr::read_unaligned(ptr.add(16) as *const u32) as u16 };
        let slot_frames = unsafe { ptr::read_unaligned(ptr.add(20) as *const u32) };
        let num_slots = unsafe { ptr::read_unaligned(ptr.add(24) as *const u32) };
        let slot_len = channels as usize * slot_frames as usize;

        Ok(Self {
            fd,
            ptr,
            channels,
            slot_frames,
            num_slots,
            slot_len,
            total_size,
            owned: false,
        })
    }

    /// Return the number of slots available to read.
    pub fn available(&self) -> u64 {
        let w = atomic_load_acquire(self.write_idx_ptr());
        let r = atomic_load_acquire(self.read_idx_ptr());
        w.saturating_sub(r)
    }

    /// Write one slot of audio data. Blocks/spins if the ringbuffer is full.
    /// `data` must have exactly `channels * slot_frames` elements.
    pub fn write(&mut self, data: &[f32]) -> Result<()> {
        let w = atomic_load_acquire(self.write_idx_ptr());
        let r = atomic_load_acquire(self.read_idx_ptr());

        if w - r >= self.num_slots as u64 {
            anyhow::bail!("ringbuffer full ({} slots available)", self.num_slots);
        }

        let dest = self.slot_ptr(w);
        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr(), dest, self.slot_len);
        }
        atomic_store_release(self.write_idx_ptr(), w + 1);
        Ok(())
    }

    /// Read one slot of audio data if available. Returns `Ok(false)` if empty.
    pub fn read(&mut self, data: &mut [f32]) -> Result<bool> {
        let w = atomic_load_acquire(self.write_idx_ptr());
        let r = atomic_load_acquire(self.read_idx_ptr());

        if w <= r {
            return Ok(false);
        }

        let src = self.slot_ptr(r);
        unsafe {
            ptr::copy_nonoverlapping(src, data.as_mut_ptr(), self.slot_len);
        }
        atomic_store_release(self.read_idx_ptr(), r + 1);
        Ok(true)
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn slot_frames(&self) -> u32 {
        self.slot_frames
    }

    pub fn slot_len(&self) -> usize {
        self.slot_len
    }

    pub fn num_slots(&self) -> u32 {
        self.num_slots
    }

    /// Current write index (total slots produced so far).
    pub fn write_index(&self) -> u64 {
        atomic_load_acquire(self.write_idx_ptr())
    }

    /// Non-destructively read the most recently written slot.
    /// Returns `Ok(false)` if no slots have been written yet.
    pub fn peek_latest(&self, data: &mut [f32]) -> Result<bool> {
        let w = atomic_load_acquire(self.write_idx_ptr());
        if w == 0 {
            return Ok(false);
        }
        let src = self.slot_ptr(w - 1);
        unsafe {
            ptr::copy_nonoverlapping(src, data.as_mut_ptr(), self.slot_len);
        }
        Ok(true)
    }
}

// ── ShmTransport ────────────────────────────────────────────────────────────

/// Shared transport state: clock counter, sample rate, play flag.
///
/// Layout:
///   [0..8)    clock       : u64  — total samples consumed (mixer writes, voices read)
///   [8..12)   sample_rate : u32  — sample rate in Hz
///   [12..16)  flags       : u32  — bit 0 = playing
///   [16..20)  bpm         : f32  — published by the sequencer (0 = unset)
///   [20..32)  reserved
pub struct ShmTransport {
    ptr: *mut u8,
    fd: i32,
    owned: bool,
}

unsafe impl Send for ShmTransport {}

impl Drop for ShmTransport {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, 64) };
        }
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
        if self.owned {
            let cname = CString::new(SHM_TRANSPORT_NAME).unwrap();
            unsafe { libc::shm_unlink(cname.as_ptr()) };
        }
    }
}

impl ShmTransport {
    fn name() -> &'static str {
        SHM_TRANSPORT_NAME
    }

    pub fn create(sample_rate: u32) -> Result<Self> {
        let total = 64usize;
        let cname = CString::new(Self::name()).unwrap();

        let fd = unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644);
            if fd < 0 {
                anyhow::bail!(
                    "shm_open({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            let r = libc::ftruncate(fd, total as libc::off_t);
            if r < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EINVAL) {
                    libc::close(fd);
                    libc::shm_unlink(cname.as_ptr());
                    anyhow::bail!("ftruncate({}) failed: {}", Self::name(), err);
                }
                // EINVAL means already sized — no problem
            }
            fd
        };

        let ptr = unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                libc::close(fd);
                libc::shm_unlink(cname.as_ptr());
                anyhow::bail!(
                    "mmap({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            p as *mut u8
        };

        unsafe {
            ptr::write_unaligned(ptr as *mut u64, 0); // clock
            ptr::write_unaligned(ptr.add(8) as *mut u32, sample_rate); // sample_rate
            ptr::write_unaligned(ptr.add(12) as *mut u32, 1); // playing
        }

        Ok(Self {
            ptr,
            fd,
            owned: true,
        })
    }

    pub fn open() -> Result<Self> {
        let total = 64usize;
        let cname = CString::new(Self::name()).unwrap();

        let fd = unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0);
            if fd < 0 {
                anyhow::bail!(
                    "shm_open({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            fd
        };

        let ptr = unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                libc::close(fd);
                anyhow::bail!(
                    "mmap({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            p as *mut u8
        };

        Ok(Self {
            ptr,
            fd,
            owned: false,
        })
    }

    pub fn clock(&self) -> u64 {
        unsafe { ptr::read_unaligned(self.ptr as *const u64) }
    }

    pub fn set_clock(&mut self, val: u64) {
        unsafe { ptr::write_unaligned(self.ptr as *mut u64, val) };
        compiler_fence(Ordering::Release);
    }

    pub fn sample_rate(&self) -> u32 {
        unsafe { ptr::read_unaligned(self.ptr.add(8) as *const u32) }
    }

    /// Publish the REAL device rate (the mixer calls this once its cpal
    /// config is known). The clock counts device frames, so a stale rate
    /// here skews every bpm-derived duration in wall-clock terms.
    pub fn set_sample_rate(&mut self, rate: u32) {
        if rate > 0 {
            unsafe { ptr::write_unaligned(self.ptr.add(8) as *mut u32, rate) };
            compiler_fence(Ordering::Release);
        }
    }

    pub fn playing(&self) -> bool {
        let flags: u32 = unsafe { ptr::read_unaligned(self.ptr.add(12) as *const u32) };
        flags & 1 != 0
    }

    /// Set the global play flag (bit 0 of flags; other bits preserved).
    pub fn set_playing(&mut self, playing: bool) {
        let mut flags: u32 = unsafe { ptr::read_unaligned(self.ptr.add(12) as *const u32) };
        if playing {
            flags |= 1;
        } else {
            flags &= !1;
        }
        compiler_fence(Ordering::Release);
        unsafe { ptr::write_unaligned(self.ptr.add(12) as *mut u32, flags) };
    }

    /// Flip the global play flag, returning the new state.
    pub fn toggle_playing(&mut self) -> bool {
        let new = !self.playing();
        self.set_playing(new);
        new
    }

    /// Session BPM, published by the sequencer (offset 16; 0 = unset → 120).
    pub fn bpm(&self) -> f32 {
        let v: f32 = unsafe { ptr::read_unaligned(self.ptr.add(16) as *const f32) };
        if v > 0.0 {
            v
        } else {
            120.0
        }
    }

    pub fn set_bpm(&mut self, bpm: f32) {
        compiler_fence(Ordering::Release);
        unsafe { ptr::write_unaligned(self.ptr.add(16) as *mut f32, bpm) };
    }

    pub fn add_clock_frames(&mut self, frames: u64) {
        let cur = self.clock();
        self.set_clock(cur + frames);
    }
}

// ── AudioEvent v2 ────────────────────────────────────────────────────────────

pub const EVENT_NOTE_ON: u8 = 0;
pub const EVENT_NOTE_OFF: u8 = 1;
pub const EVENT_PARAM: u8 = 2;
pub const EVENT_MOD: u8 = 3;
pub const EVENT_TRIGGER: u8 = 4;

pub const PARAM_SHAPE: u8 = 0;
pub const PARAM_SUB: u8 = 1;
pub const PARAM_FM: u8 = 2;
pub const PARAM_OUTPUT: u8 = 3;
pub const PARAM_LEVEL: u8 = 4;
pub const PARAM_RISE: u8 = 5;
pub const PARAM_FALL: u8 = 6;

/// A single event message (32 bytes fixed size) in shared memory.
///
/// Layout:
///   event_type: u8   — 0=note_on, 1=note_off, 2=param, 3=mod, 4=trigger
///   source:     u8   — encoded source module + instance
///   target:     u8   — encoded target module + instance
///   param:      u8   — target parameter ID
///   value:      f32  — note frequency, modulation amount, or trigger level
///   step:       u32  — step index / timestamp
///   reserved:   [u8; 16]
#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct AudioEvent {
    pub event_type: u8,
    pub source: u8,
    pub target: u8,
    pub param: u8,
    pub value: f32,
    pub step: u32,
    _reserved: [u8; 20],
}

const _: [(); 1] = [(); (core::mem::size_of::<AudioEvent>() == 32) as usize];

impl AudioEvent {
    pub fn note_on(note: u8, velocity: u8, step: u32) -> Self {
        let freq = 440.0 * 2.0f32.powf((note as f32 - 69.0) / 12.0);
        Self {
            event_type: EVENT_NOTE_ON,
            source: 0,
            target: 0,
            param: velocity,
            value: freq,
            step,
            ..Default::default()
        }
    }

    pub fn note_on_source(note: u8, velocity: u8, source: u8, step: u32) -> Self {
        let freq = 440.0 * 2.0f32.powf((note as f32 - 69.0) / 12.0);
        Self::note_on_hz(freq, velocity, source, step)
    }

    /// Note-on with an explicit frequency — the microtonal path (scaled
    /// sequencer tracks compute Hz through the cents engine). Same wire
    /// layout as [`AudioEvent::note_on_source`]; note-offs match by
    /// source, so no note id is needed here.
    pub fn note_on_hz(freq: f32, velocity: u8, source: u8, step: u32) -> Self {
        Self {
            event_type: EVENT_NOTE_ON,
            source,
            target: 0,
            param: velocity,
            value: freq,
            step,
            ..Default::default()
        }
    }

    pub fn note_off(note: u8, step: u32) -> Self {
        Self {
            event_type: EVENT_NOTE_OFF,
            source: 0,
            target: 0,
            param: note,
            step,
            ..Default::default()
        }
    }

    pub fn note_off_source(note: u8, source: u8, step: u32) -> Self {
        Self {
            event_type: EVENT_NOTE_OFF,
            source,
            target: 0,
            param: note,
            step,
            ..Default::default()
        }
    }

    pub fn param(id: u8, value: f32) -> Self {
        Self {
            event_type: EVENT_PARAM,
            source: 0,
            target: 0,
            param: id,
            value,
            ..Default::default()
        }
    }

    pub fn modulation(target: u8, param: u8, value: f32, step: u32) -> Self {
        Self {
            event_type: EVENT_MOD,
            source: 0,
            target,
            param,
            value,
            step,
            ..Default::default()
        }
    }

    pub fn trigger(source: u8, target: u8, value: f32, step: u32) -> Self {
        Self {
            event_type: EVENT_TRIGGER,
            source,
            target,
            value,
            step,
            ..Default::default()
        }
    }

    pub fn is_note_on(&self) -> bool {
        self.event_type == EVENT_NOTE_ON
    }

    pub fn is_note_off(&self) -> bool {
        self.event_type == EVENT_NOTE_OFF
    }

    pub fn is_mod(&self) -> bool {
        self.event_type == EVENT_MOD
    }

    pub fn is_trigger(&self) -> bool {
        self.event_type == EVENT_TRIGGER
    }
}

// ── EventRingbuf (MPMC) ────────────────────────────────────────────────────

const EVENT_SIZE: usize = 32;
const NUM_CONSUMERS: usize = 16;

/// Lock-free multi-producer multi-consumer ringbuffer for fixed-size events
/// backed by POSIX SHM.
///
/// Layout:
///   [0..8)     write_index    : u64
///   [8..16)    reserved
///   [16..N*8)  read_index_0..N : u64  (one per consumer)
///   [256..)    event data  (EVENT_SIZE bytes each)
pub struct EventRingbuf {
    fd: i32,
    ptr: *mut u8,
    consumer_id: usize,
    num_slots: u32,
    total_size: usize,
    owned: bool,
}

unsafe impl Send for EventRingbuf {}

impl Drop for EventRingbuf {
    fn drop(&mut self) {
        // Consumers vacate their slot so they never block the producer
        // after a clean exit (unclean deaths are reaped by the producer).
        if !self.ptr.is_null() && self.consumer_id < NUM_CONSUMERS {
            atomic_store_release(self.read_idx_ptr(self.consumer_id), u64::MAX);
            unsafe { ptr::write_volatile(self.consumer_pid_ptr(self.consumer_id), 0) };
        }
        if !self.ptr.is_null() {
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.total_size) };
        }
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
        if self.owned {
            let cname = CString::new(Self::name()).unwrap();
            unsafe { libc::shm_unlink(cname.as_ptr()) };
        }
    }
}

impl EventRingbuf {
    fn name() -> &'static str {
        SHM_EVENTS_NAME
    }

    fn write_idx_ptr(&self) -> *mut u64 {
        self.ptr as *mut u64
    }

    fn read_idx_ptr(&self, consumer_id: usize) -> *mut u64 {
        unsafe { self.ptr.add(16 + consumer_id * 8) as *mut u64 }
    }

    fn slot_ptr(&self, index: u64) -> *mut u8 {
        let slot = index as usize % self.num_slots as usize;
        let offset = EVENT_DATA_OFFSET + slot * EVENT_SIZE;
        unsafe { self.ptr.add(offset) }
    }

    /// Pid registry: one u32 per consumer slot, written on open so the
    /// producer can tell a dead consumer from a slow one.
    fn consumer_pid_ptr(&self, consumer_id: usize) -> *mut u32 {
        unsafe { self.ptr.add(144 + consumer_id * 4) as *mut u32 }
    }

    /// Smallest read index over *joined* consumers (None = no consumers, no
    /// backpressure). u64::MAX marks an unjoined/vacated slot.
    fn min_read_index(&self) -> Option<u64> {
        let mut min = None;
        for i in 0..NUM_CONSUMERS {
            let r = atomic_load_acquire(self.read_idx_ptr(i));
            if r != u64::MAX && min.is_none_or(|m| r < m) {
                min = Some(r);
            }
        }
        min
    }

    /// Vacate consumer slots whose process is gone. A killed module (tmux,
    /// crash) leaves its read index frozen; once the writer laps it the ring
    /// blocks for everyone, forever — this is what froze the sequencer's
    /// note events. pid 0 covers rings created before the registry existed.
    fn reap_dead_consumers(&self) {
        for i in 0..NUM_CONSUMERS {
            let r = atomic_load_acquire(self.read_idx_ptr(i));
            if r == u64::MAX {
                continue;
            }
            let pid = unsafe { ptr::read_volatile(self.consumer_pid_ptr(i)) };
            let dead = pid == 0
                || (unsafe { libc::kill(pid as i32, 0) } != 0
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH));
            if dead {
                atomic_store_release(self.read_idx_ptr(i), u64::MAX);
                unsafe { ptr::write_volatile(self.consumer_pid_ptr(i), 0) };
            }
        }
    }

    pub fn create() -> Result<Self> {
        let num_slots = 256u32;
        let data_bytes = num_slots as usize * EVENT_SIZE;
        let total_size = EVENT_DATA_OFFSET + data_bytes;
        let cname = CString::new(Self::name()).unwrap();

        let fd = unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644);
            if fd < 0 {
                anyhow::bail!(
                    "shm_open({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            let r = libc::ftruncate(fd, total_size as libc::off_t);
            if r < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EINVAL) {
                    libc::close(fd);
                    libc::shm_unlink(cname.as_ptr());
                    anyhow::bail!("ftruncate({}) failed: {}", Self::name(), err);
                }
            }
            fd
        };

        let ptr = unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                total_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                libc::close(fd);
                libc::shm_unlink(cname.as_ptr());
                anyhow::bail!(
                    "mmap({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            p as *mut u8
        };

        unsafe {
            ptr::write_unaligned(ptr as *mut u64, 0); // write_index
            for i in 0..NUM_CONSUMERS {
                ptr::write_unaligned(ptr.add(16 + i * 8) as *mut u64, u64::MAX);
                ptr::write_unaligned(ptr.add(144 + i * 4) as *mut u32, 0); // pid registry
            }
        }

        Ok(Self {
            fd,
            ptr,
            consumer_id: NUM_CONSUMERS, // creator is producer, not a consumer
            num_slots,
            total_size,
            owned: true,
        })
    }

    /// Open as a producer (no consumer read pointer — write only).
    pub fn open_producer() -> Result<Self> {
        let num_slots = 256u32;
        let data_bytes = num_slots as usize * EVENT_SIZE;
        let total_size = EVENT_DATA_OFFSET + data_bytes;
        let cname = CString::new(Self::name()).unwrap();

        let fd = unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0);
            if fd < 0 {
                anyhow::bail!(
                    "shm_open({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            fd
        };

        let ptr = unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                total_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                libc::close(fd);
                anyhow::bail!(
                    "mmap({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            p as *mut u8
        };

        Ok(Self {
            fd,
            ptr,
            consumer_id: NUM_CONSUMERS, // sentinel: producer
            num_slots,
            total_size,
            owned: false,
        })
    }

    pub fn open(consumer_id: usize) -> Result<Self> {
        anyhow::ensure!(
            consumer_id < NUM_CONSUMERS,
            "consumer_id must be < {}",
            NUM_CONSUMERS
        );
        let num_slots = 256u32;
        let data_bytes = num_slots as usize * EVENT_SIZE;
        let total_size = EVENT_DATA_OFFSET + data_bytes;
        let cname = CString::new(Self::name()).unwrap();

        let fd = unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0);
            if fd < 0 {
                anyhow::bail!(
                    "shm_open({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            fd
        };

        let ptr = unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                total_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                libc::close(fd);
                anyhow::bail!(
                    "mmap({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            p as *mut u8
        };

        // Initialize this consumer's read index to the current write index
        // so it only sees events written after it joins, and doesn't block
        // the producer with stale state. Register our pid so the producer
        // can vacate this slot if we die uncleanly.
        unsafe {
            let w = ptr::read_volatile(ptr as *const u64);
            ptr::write_volatile(ptr.add(16 + consumer_id * 8) as *mut u64, w);
            ptr::write_volatile(
                ptr.add(144 + consumer_id * 4) as *mut u32,
                std::process::id(),
            );
        }

        Ok(Self {
            fd,
            ptr,
            consumer_id,
            num_slots,
            total_size,
            owned: false,
        })
    }

    /// One-line diagnostic: write index + every joined consumer's lag.
    pub fn debug_status(&self) -> String {
        let w = atomic_load_acquire(self.write_idx_ptr());
        let mut parts = vec![format!("w={}", w)];
        for i in 0..NUM_CONSUMERS {
            let r = atomic_load_acquire(self.read_idx_ptr(i));
            if r != u64::MAX {
                parts.push(format!("c{}={} (lag {})", i, r, w.saturating_sub(r)));
            }
        }
        parts.join("  ")
    }

    pub fn write_event(&mut self, event: &AudioEvent) -> Result<()> {
        let w = atomic_load_acquire(self.write_idx_ptr());

        let full = |min_r: Option<u64>| min_r.is_some_and(|r| w - r >= self.num_slots as u64);
        if full(self.min_read_index()) {
            // a dead consumer may be the blocker — vacate corpses and retry
            self.reap_dead_consumers();
            if full(self.min_read_index()) {
                anyhow::bail!("event buffer full (live consumer lagging)");
            }
        }

        let dest = self.slot_ptr(w);
        unsafe {
            ptr::copy_nonoverlapping(event as *const AudioEvent as *const u8, dest, EVENT_SIZE);
        }
        atomic_store_release(self.write_idx_ptr(), w + 1);
        Ok(())
    }

    pub fn read_event(&mut self) -> Option<AudioEvent> {
        let w = atomic_load_acquire(self.write_idx_ptr());
        let r = atomic_load_acquire(self.read_idx_ptr(self.consumer_id));

        if w <= r {
            return None;
        }

        let src = self.slot_ptr(r);
        let mut event = AudioEvent::default();
        unsafe {
            ptr::copy_nonoverlapping(src, &mut event as *mut AudioEvent as *mut u8, EVENT_SIZE);
        }
        atomic_store_release(self.read_idx_ptr(self.consumer_id), r + 1);
        Some(event)
    }

    pub fn available(&self) -> u64 {
        let w = atomic_load_acquire(self.write_idx_ptr());
        let r = atomic_load_acquire(self.read_idx_ptr(self.consumer_id));
        w.saturating_sub(r)
    }
}

// ── ModulationBus ────────────────────────────────────────────────────────────

/// Shared modulation values: 32 atomic f32 channels backed by POSIX SHM.
///
/// Layout:
///   [0..4)     version      : u32 = 1
///   [4..8)     num_channels : u32 = 32
///   [8..64)    reserved
///   [64..4160) 32 x f32 channels (aligned 4)
/// v2: grew 64 → 128 so a full fx rack (two sequencers, three
/// envelopes, the delay's 9 followers, the filterbank's 16) fits with
/// headroom. Note: `consumes_channels` listening markers are a u64
/// bitmap, so channels ≥ 64 simply don't display who's-listening hints
/// — display-only, nothing functional.
const MODBUS_NUM_CHANNELS: usize = 128;
const MODBUS_VERSION: u32 = 2;
const MODBUS_DATA_OFFSET: usize = 64;
const MODBUS_TOTAL_SIZE: usize = MODBUS_DATA_OFFSET + MODBUS_NUM_CHANNELS * size_of::<f32>();

pub struct ModulationBus {
    ptr: *mut u8,
    fd: i32,
    owned: bool,
}

unsafe impl Send for ModulationBus {}

impl Drop for ModulationBus {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, MODBUS_TOTAL_SIZE) };
        }
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
        if self.owned {
            let cname = CString::new(SHM_MODBUS_NAME).unwrap();
            unsafe { libc::shm_unlink(cname.as_ptr()) };
        }
    }
}

impl ModulationBus {
    fn name() -> &'static str {
        SHM_MODBUS_NAME
    }

    fn channel_ptr(&self, channel: usize) -> *mut f32 {
        unsafe {
            self.ptr
                .add(MODBUS_DATA_OFFSET + channel * size_of::<f32>()) as *mut f32
        }
    }

    pub fn create() -> Result<Self> {
        let cname = CString::new(Self::name()).unwrap();
        let total = MODBUS_TOTAL_SIZE;

        let fd = unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644);
            if fd < 0 {
                anyhow::bail!(
                    "shm_open({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            // NOTE: no size validation here — macOS rounds SHM objects
            // up to page size (16 KB), so fstat can't distinguish
            // layouts and any leftover comfortably fits our mapping.
            // The version header below is the real discriminator; the
            // unconditional re-init makes a stale v1 bus into a v2 one.
            let r = libc::ftruncate(fd, total as libc::off_t);
            if r < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EINVAL) {
                    libc::close(fd);
                    libc::shm_unlink(cname.as_ptr());
                    anyhow::bail!("ftruncate({}) failed: {}", Self::name(), err);
                }
            }
            fd
        };

        let ptr = unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                libc::close(fd);
                libc::shm_unlink(cname.as_ptr());
                anyhow::bail!(
                    "mmap({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            p as *mut u8
        };

        unsafe {
            ptr::write_unaligned(ptr as *mut u32, MODBUS_VERSION);
            ptr::write_unaligned(ptr.add(4) as *mut u32, MODBUS_NUM_CHANNELS as u32);
            // Zero all channel values
            for i in 0..MODBUS_NUM_CHANNELS {
                ptr::write_unaligned(
                    ptr.add(MODBUS_DATA_OFFSET + i * size_of::<f32>()) as *mut f32,
                    0.0f32,
                );
            }
        }

        Ok(Self {
            ptr,
            fd,
            owned: true,
        })
    }

    pub fn open() -> Result<Self> {
        let cname = CString::new(Self::name()).unwrap();
        let total = MODBUS_TOTAL_SIZE;

        let fd = unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0);
            if fd < 0 {
                anyhow::bail!(
                    "shm_open({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            fd
        };

        let ptr = unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                libc::close(fd);
                anyhow::bail!(
                    "mmap({}) failed: {}",
                    Self::name(),
                    std::io::Error::last_os_error()
                );
            }
            p as *mut u8
        };

        // A stale bus from an older binary (v1: 64 channels) declares
        // itself in the header — refuse it so open().or_else(create())
        // re-initializes the object instead of misreading it. (Size
        // checks don't work here: macOS page-rounds SHM objects.)
        let version = unsafe { ptr::read_unaligned(ptr as *const u32) };
        let channels = unsafe { ptr::read_unaligned(ptr.add(4) as *const u32) };
        if version != MODBUS_VERSION || channels != MODBUS_NUM_CHANNELS as u32 {
            unsafe {
                libc::munmap(ptr as *mut libc::c_void, total);
                libc::close(fd);
            }
            anyhow::bail!(
                "modbus v{} ({} ch) != v{} ({} ch) — stale SHM from an old binary",
                version,
                channels,
                MODBUS_VERSION,
                MODBUS_NUM_CHANNELS
            );
        }

        Ok(Self {
            ptr,
            fd,
            owned: false,
        })
    }

    /// Read a channel value (volatile, atomic on aligned f32).
    pub fn get(&self, channel: usize) -> f32 {
        if channel >= MODBUS_NUM_CHANNELS {
            return 0.0;
        }
        unsafe { ptr::read_volatile(self.channel_ptr(channel)) }
    }

    /// Write a channel value (volatile, atomic on aligned f32).
    pub fn set(&mut self, channel: usize, value: f32) {
        if channel >= MODBUS_NUM_CHANNELS {
            return;
        }
        unsafe {
            ptr::write_volatile(self.channel_ptr(channel), value);
        }
        compiler_fence(Ordering::Release);
    }
}

// ── Manifest ────────────────────────────────────────────────────────────

const MANIFEST_MAX_ENTRIES: usize = 16;
const MANIFEST_ENTRY_SIZE: usize = 128; // v3: grew from 96 for fx input claims
const MANIFEST_HEADER_SIZE: usize = 64;
const MANIFEST_VERSION: u32 = 3;
/// Total modbus channels available to the allocator.
pub const MODBUS_CHANNELS: usize = MODBUS_NUM_CHANNELS;

/// Event-ringbuf consumer slot assignment (16 slots):
/// voices 0–7, envelopes 8–11, 12–15 reserved.
/// One documented scheme instead of per-module arithmetic — a module that
/// reconnects MUST use the same slot it started with or it steals another
/// module's events.
pub fn consumer_id(module: &str, instance: usize) -> usize {
    match module {
        "voice" => instance.min(7),
        "envelope" => 8 + instance.min(3),
        // `los tap` gets its own cursor so draining the backlog can't
        // starve whichever module shares the default slot
        "tap" => 14,
        _ => 15,
    }
}
const MANIFEST_TOTAL_SIZE: usize =
    MANIFEST_HEADER_SIZE + MANIFEST_MAX_ENTRIES * MANIFEST_ENTRY_SIZE;

/// Shared module registry: each module registers itself on startup.
///
/// Lock-free fixed-size array. Entries are claimed atomically via CAS.
/// See `Manifest::entries()` for the reader-safe protocol.
pub struct Manifest {
    ptr: *mut u8,
    fd: i32,
    owned: bool,
    my_slot: Option<usize>,
    my_mod_base: Option<usize>,
}

unsafe impl Send for Manifest {}

impl Drop for Manifest {
    fn drop(&mut self) {
        self.unregister();
        if !self.ptr.is_null() {
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, MANIFEST_TOTAL_SIZE) };
        }
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
        if self.owned {
            let cname = CString::new(SHM_MANIFEST_NAME).unwrap();
            unsafe { libc::shm_unlink(cname.as_ptr()) };
        }
    }
}

impl Manifest {
    fn entry_valid_ptr(&self, slot: usize) -> *const AtomicU32 {
        unsafe {
            self.ptr
                .add(MANIFEST_HEADER_SIZE + slot * MANIFEST_ENTRY_SIZE)
                as *const AtomicU32
        }
    }

    fn entry_valid_mut_ptr(&self, slot: usize) -> *mut AtomicU32 {
        unsafe {
            self.ptr
                .add(MANIFEST_HEADER_SIZE + slot * MANIFEST_ENTRY_SIZE)
                as *mut AtomicU32
        }
    }

    fn entry_data_ptr(&self, slot: usize) -> *mut u8 {
        unsafe {
            self.ptr
                .add(MANIFEST_HEADER_SIZE + slot * MANIFEST_ENTRY_SIZE + 4)
        }
    }

    pub fn create() -> Result<Self> {
        let cname = CString::new(SHM_MANIFEST_NAME).unwrap();
        let total = MANIFEST_TOTAL_SIZE;

        let fd = unsafe {
            let mut fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644);
            if fd < 0 {
                anyhow::bail!(
                    "shm_open({}) failed: {}",
                    SHM_MANIFEST_NAME,
                    std::io::Error::last_os_error()
                );
            }
            // POSIX shm objects can only be sized once on macOS. A wrong-sized
            // object is a leftover from an older layout: unlink it and start
            // over rather than mmapping past its end.
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut st) < 0 {
                libc::close(fd);
                anyhow::bail!(
                    "fstat({}) failed: {}",
                    SHM_MANIFEST_NAME,
                    std::io::Error::last_os_error()
                );
            }
            if st.st_size != total as libc::off_t {
                if st.st_size == 0 {
                    if libc::ftruncate(fd, total as libc::off_t) < 0 {
                        libc::close(fd);
                        libc::shm_unlink(cname.as_ptr());
                        anyhow::bail!(
                            "ftruncate({}) failed: {}",
                            SHM_MANIFEST_NAME,
                            std::io::Error::last_os_error()
                        );
                    }
                } else {
                    libc::close(fd);
                    libc::shm_unlink(cname.as_ptr());
                    fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644);
                    if fd < 0 || libc::ftruncate(fd, total as libc::off_t) < 0 {
                        if fd >= 0 {
                            libc::close(fd);
                        }
                        anyhow::bail!(
                            "recreate({}) failed: {}",
                            SHM_MANIFEST_NAME,
                            std::io::Error::last_os_error()
                        );
                    }
                }
            }
            fd
        };

        let ptr = unsafe {
            let p = libc::mmap(
                std::ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                libc::close(fd);
                libc::shm_unlink(cname.as_ptr());
                anyhow::bail!(
                    "mmap({}) failed: {}",
                    SHM_MANIFEST_NAME,
                    std::io::Error::last_os_error()
                );
            }
            p as *mut u8
        };

        // Initialize only when the header isn't already a live current-version
        // manifest — concurrent open().or_else(create()) callers must not
        // wipe each other's registrations.
        unsafe {
            let version = ptr::read_unaligned(ptr as *const u32);
            if version != MANIFEST_VERSION {
                ptr::write_unaligned(ptr as *mut u32, MANIFEST_VERSION);
                ptr::write_unaligned(ptr.add(4) as *mut u32, MANIFEST_MAX_ENTRIES as u32);
                ptr::write_unaligned(ptr.add(8) as *mut u32, MANIFEST_ENTRY_SIZE as u32);
                ptr::write_unaligned(ptr.add(12) as *mut u32, 0); // next free modbus channel
                std::ptr::write_bytes(
                    ptr.add(MANIFEST_HEADER_SIZE),
                    0,
                    MANIFEST_MAX_ENTRIES * MANIFEST_ENTRY_SIZE,
                );
            }
        }

        Ok(Self {
            ptr,
            fd,
            owned: true,
            my_slot: None,
            my_mod_base: None,
        })
    }

    pub fn open() -> Result<Self> {
        let cname = CString::new(SHM_MANIFEST_NAME).unwrap();

        let fd = unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0);
            if fd < 0 {
                anyhow::bail!(
                    "shm_open({}) failed: {}",
                    SHM_MANIFEST_NAME,
                    std::io::Error::last_os_error()
                );
            }
            fd
        };

        let ptr = unsafe {
            let p = libc::mmap(
                std::ptr::null_mut(),
                MANIFEST_TOTAL_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                libc::close(fd);
                anyhow::bail!(
                    "mmap({}) failed: {}",
                    SHM_MANIFEST_NAME,
                    std::io::Error::last_os_error()
                );
            }
            p as *mut u8
        };

        // A leftover manifest from an older binary has a different entry
        // layout — refuse it so the open().or_else(create()) chain
        // re-initializes the object instead of misreading it.
        let version = unsafe { ptr::read_unaligned(ptr as *const u32) };
        if version != MANIFEST_VERSION {
            unsafe {
                libc::munmap(ptr as *mut libc::c_void, MANIFEST_TOTAL_SIZE);
                libc::close(fd);
            }
            anyhow::bail!(
                "manifest version {} != {} (stale SHM from an old binary)",
                version,
                MANIFEST_VERSION
            );
        }

        Ok(Self {
            ptr,
            fd,
            owned: false,
            my_slot: None,
            my_mod_base: None,
        })
    }

    /// Atomically claim `count` modbus channels; returns the base index.
    /// The allocator is monotonic (channels are not reclaimed on exit) — a
    /// restarted module simply claims a fresh range and bindings re-resolve
    /// through the manifest. 64 channels is plenty for one session.
    fn claim_channels(&mut self, count: u32) -> Result<u32> {
        let alloc = unsafe { &*(self.ptr.add(12) as *const AtomicU32) };
        let base = alloc.fetch_add(count, Ordering::AcqRel);
        anyhow::ensure!(
            (base + count) as usize <= MODBUS_NUM_CHANNELS,
            "modbus channels exhausted ({} max)",
            MODBUS_NUM_CHANNELS
        );
        Ok(base)
    }

    /// Free manifest slots held by dead processes (a module killed by tmux
    /// or a crash never runs Drop, so its entry — and its channel claim —
    /// would otherwise leak forever). Reclaims allocator space where safe:
    /// a full reset when no live claimers remain, otherwise dead ranges are
    /// popped off the top of the allocator. Returns the number reaped.
    pub fn reap_dead(&mut self) -> usize {
        let mut reaped = 0usize;
        let mut live_end: u32 = 0;
        let mut dead_ranges: Vec<(u32, u32)> = Vec::new();
        for slot in 0..MANIFEST_MAX_ENTRIES {
            let valid = unsafe { &*self.entry_valid_mut_ptr(slot) };
            if valid.load(Ordering::Acquire) != 1 {
                continue;
            }
            let data = self.entry_data_ptr(slot);
            let pid = unsafe { ptr::read_unaligned(data.add(20) as *const u32) };
            let base = unsafe { ptr::read_unaligned(data.add(56) as *const u32) };
            let count = unsafe { ptr::read_unaligned(data.add(60) as *const u32) };
            // signal 0 = existence check; ESRCH = dead (EPERM still means alive)
            let dead = pid != 0
                && unsafe { libc::kill(pid as i32, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if dead {
                if base != u32::MAX && count > 0 {
                    dead_ranges.push((base, count));
                }
                valid.store(0, Ordering::Release);
                reaped += 1;
            } else if base != u32::MAX && count > 0 {
                live_end = live_end.max(base + count);
            }
        }

        let alloc = unsafe { &*(self.ptr.add(12) as *const AtomicU32) };
        if live_end == 0 {
            // no live claimers: reset the allocator (CAS so a concurrent
            // claimer can't be clobbered)
            let cur = alloc.load(Ordering::Acquire);
            if cur > 0 {
                let _ = alloc.compare_exchange(cur, 0, Ordering::AcqRel, Ordering::Relaxed);
            }
        } else {
            // pop dead ranges that sit exactly at the top of the allocator
            dead_ranges.sort_by_key(|(base, _)| std::cmp::Reverse(*base));
            let mut cur = alloc.load(Ordering::Acquire);
            for (base, count) in dead_ranges {
                if base >= live_end && base + count == cur {
                    match alloc.compare_exchange(cur, base, Ordering::AcqRel, Ordering::Relaxed) {
                        Ok(_) => cur = base,
                        Err(_) => break,
                    }
                }
            }
        }
        reaped
    }

    /// Register this module in the manifest. Returns the slot index.
    /// `mod_channels` modbus channels are claimed for this module's outputs
    /// (0 for modules that produce none).
    /// Dead entries are reaped first, so leaked slots/channels from killed
    /// sessions self-heal here.
    /// Uses two-phase claim protocol: 0 → CLAIMING → 1.
    /// Readers only read entries with valid == 1 (data fully written).
    pub fn register(
        &mut self,
        module_name: &str,
        instance: usize,
        audio_shm: Option<&str>,
        mod_channels: u32,
    ) -> Result<usize> {
        anyhow::ensure!(
            module_name.len() < 16,
            "module name too long (max 15 chars)"
        );
        if let Some(shm) = audio_shm {
            anyhow::ensure!(shm.len() < 32, "audio SHM name too long (max 31 chars)");
        }
        self.reap_dead();
        let mod_base = if mod_channels > 0 {
            self.claim_channels(mod_channels)?
        } else {
            u32::MAX
        };

        for slot in 0..MANIFEST_MAX_ENTRIES {
            let valid = unsafe { &*self.entry_valid_mut_ptr(slot) };
            match valid.compare_exchange(0, 2, Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => {
                    let data = self.entry_data_ptr(slot);
                    unsafe {
                        let name_bytes = module_name.as_bytes();
                        std::ptr::copy_nonoverlapping(name_bytes.as_ptr(), data, name_bytes.len());
                        std::ptr::write(data.add(name_bytes.len()), 0u8);

                        ptr::write_unaligned(data.add(16) as *mut u32, instance as u32);
                        ptr::write_unaligned(data.add(20) as *mut u32, std::process::id());

                        // a reused slot must not inherit the previous
                        // occupant's audio ring: a ring-less module (badge,
                        // sequencer) registering into a dead voice's slot
                        // showed up in the mixer as a ghost channel playing
                        // someone else's audio
                        std::ptr::write_bytes(data.add(24), 0u8, 32);
                        if let Some(shm) = audio_shm {
                            let shm_bytes = shm.as_bytes();
                            let dst = data.add(24);
                            std::ptr::copy_nonoverlapping(shm_bytes.as_ptr(), dst, shm_bytes.len());
                            std::ptr::write(dst.add(shm_bytes.len()), 0u8);
                        }

                        ptr::write_unaligned(data.add(56) as *mut u32, mod_base);
                        ptr::write_unaligned(data.add(60) as *mut u32, mod_channels);
                        // a reused slot must not inherit the previous
                        // occupant's listening claims
                        ptr::write_unaligned(data.add(64) as *mut u64, 0u64);
                        ptr::write_unaligned(data.add(72), 0u8);
                        // …or its audio-input claim (v3): a stale claim
                        // would make the mixer skip a source nobody is
                        // actually consuming
                        std::ptr::write_bytes(data.add(80), 0u8, 32);
                    }
                    valid.store(1, Ordering::Release);
                    self.my_slot = Some(slot);
                    self.my_mod_base = if mod_channels > 0 {
                        Some(mod_base as usize)
                    } else {
                        None
                    };
                    return Ok(slot);
                }
                _ => continue,
            }
        }
        anyhow::bail!("manifest is full ({} max entries)", MANIFEST_MAX_ENTRIES);
    }

    /// The modbus base channel this module claimed at registration.
    pub fn claimed_base(&self) -> Option<usize> {
        self.my_mod_base
    }

    /// Publish what this module LISTENS to: a bitmap of consumed modbus
    /// channels and a bitmap of sequencer note tracks. Display-only data
    /// (the sequencer's who's-listening markers); torn reads are harmless.
    pub fn publish_consumes(&mut self, channels: u64, note_tracks: u8) {
        if let Some(slot) = self.my_slot {
            let data = self.entry_data_ptr(slot);
            unsafe {
                ptr::write_unaligned(data.add(64) as *mut u64, channels);
                ptr::write_unaligned(data.add(72), note_tracks);
            }
        }
    }

    /// Publish (or clear) this module's audio-input claim: the SHM name
    /// of the audio ringbuffer it is consuming (v3, fx modules). The mixer
    /// skips claimed sources — the cable has left the console — and
    /// re-adopts them when the claim clears or the claimant dies.
    pub fn publish_input(&mut self, input_shm: Option<&str>) {
        let Some(slot) = self.my_slot else { return };
        let data = self.entry_data_ptr(slot);
        unsafe {
            std::ptr::write_bytes(data.add(80), 0u8, 32);
            if let Some(shm) = input_shm {
                let bytes = shm.as_bytes();
                let n = bytes.len().min(31);
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.add(80), n);
            }
        }
    }

    /// Current value of the channel allocator (next free modbus channel).
    pub fn next_channel(&self) -> u32 {
        unsafe { (*(self.ptr.add(12) as *const AtomicU32)).load(Ordering::Acquire) }
    }

    /// Forge an entry's pid (tests only — to simulate a dead process).
    #[cfg(test)]
    fn force_entry_pid(&mut self, slot: usize, pid: u32) {
        let data = self.entry_data_ptr(slot);
        unsafe { ptr::write_unaligned(data.add(20) as *mut u32, pid) };
    }

    /// Unregister from our slot (called on Drop, but can be explicit too).
    pub fn unregister(&mut self) {
        if let Some(slot) = self.my_slot.take() {
            let valid = unsafe { &*self.entry_valid_mut_ptr(slot) };
            valid.store(0, Ordering::Release);
        }
    }

    /// Read all valid entries from the manifest.
    /// Only reads entries where valid == 1 (data fully written by producer).
    pub fn entries(&self) -> Vec<ManifestEntry> {
        let mut result = Vec::new();
        for slot in 0..MANIFEST_MAX_ENTRIES {
            let valid = unsafe { &*self.entry_valid_ptr(slot) };
            if valid.load(Ordering::Acquire) != 1 {
                continue;
            }
            let data = unsafe {
                std::slice::from_raw_parts(self.entry_data_ptr(slot), MANIFEST_ENTRY_SIZE - 4)
            };
            let name_end = data[..16].iter().position(|&b| b == 0).unwrap_or(16);
            let module_name = String::from_utf8_lossy(&data[..name_end]).to_string();
            let instance =
                unsafe { ptr::read_unaligned(data.as_ptr().add(16) as *const u32) as usize };
            let pid = unsafe { ptr::read_unaligned(data.as_ptr().add(20) as *const u32) };
            let audio_shm = {
                let shm_end = data[24..56].iter().position(|&b| b == 0).unwrap_or(32);
                if shm_end == 0 {
                    None
                } else {
                    Some(String::from_utf8_lossy(&data[24..24 + shm_end]).to_string())
                }
            };
            let raw_base = unsafe { ptr::read_unaligned(data.as_ptr().add(56) as *const u32) };
            let mod_count = unsafe { ptr::read_unaligned(data.as_ptr().add(60) as *const u32) };
            let mod_base = if raw_base == u32::MAX || mod_count == 0 {
                None
            } else {
                Some(raw_base as usize)
            };
            let consumes_channels =
                unsafe { ptr::read_unaligned(data.as_ptr().add(64) as *const u64) };
            let consumes_notes = data[72];
            let input_shm = {
                let end = data[80..112].iter().position(|&b| b == 0).unwrap_or(32);
                if end == 0 {
                    None
                } else {
                    Some(String::from_utf8_lossy(&data[80..80 + end]).to_string())
                }
            };
            result.push(ManifestEntry {
                module_name,
                instance,
                pid,
                audio_shm,
                mod_base,
                mod_count: mod_count as usize,
                consumes_channels,
                consumes_notes,
                input_shm,
            });
        }
        result
    }
}

#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub module_name: String,
    pub instance: usize,
    pub pid: u32,
    pub audio_shm: Option<String>,
    /// First modbus channel claimed by this module (None = claims none).
    pub mod_base: Option<usize>,
    pub mod_count: usize,
    /// Bitmap of modbus channels this module reads (its bound sources).
    pub consumes_channels: u64,
    /// Bitmap of sequencer note tracks this module plays.
    pub consumes_notes: u8,
    /// Audio ringbuffer this module is consuming (fx modules, v3) — the
    /// mixer leaves claimed sources alone.
    pub input_shm: Option<String>,
}

#[cfg(test)]
mod shm_tests {
    use super::*;
    use std::sync::Mutex;

    // All SHM tests must run serially because they use fixed SHM names.
    static SHM_TEST_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn shm_names_are_test_isolated() {
        // Tests unlink and recreate these objects freely; if any of them
        // resolves to a production name, `cargo test` destroys the shared
        // memory of a live los session (this wiped the manifest out from
        // under a running mixer once).
        for name in [
            SHM_TRANSPORT_NAME,
            SHM_EVENTS_NAME,
            SHM_MODBUS_NAME,
            SHM_MANIFEST_NAME,
        ] {
            assert!(
                name.starts_with("/los_test_"),
                "{name} would clobber a live session's SHM"
            );
        }
    }

    #[test]
    fn transport_play_flag_shared_across_handles() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_TRANSPORT_NAME).unwrap().as_ptr()) };

        let owner = ShmTransport::create(48000).expect("create transport");
        assert!(owner.playing(), "transport should default to playing");

        // A second handle (another process in real life) toggles the flag
        let mut other = ShmTransport::open().expect("open transport");
        assert!(
            !other.toggle_playing(),
            "toggle from playing should return stopped"
        );
        assert!(!owner.playing(), "owner must see the stop");

        other.set_playing(true);
        assert!(owner.playing(), "owner must see play again");
    }

    #[test]
    fn audio_event_size_is_32_bytes() {
        assert_eq!(core::mem::size_of::<AudioEvent>(), 32);
    }

    #[test]
    fn audio_event_note_on_computes_frequency() {
        let ev = AudioEvent::note_on(69, 100, 0);
        assert_eq!(ev.event_type, EVENT_NOTE_ON);
        assert!(
            (ev.value - 440.0).abs() < 0.01,
            "A4 should be ~440 Hz, got {}",
            ev.value
        );
        assert_eq!(ev.param, 100);
        assert_eq!(ev.step, 0);
    }

    #[test]
    fn audio_event_note_on_c4_frequency() {
        let ev = AudioEvent::note_on(60, 127, 5);
        assert!(
            (ev.value - 261.63).abs() < 0.1,
            "C4 should be ~261.63 Hz, got {}",
            ev.value
        );
    }

    #[test]
    fn audio_event_modulation_carries_f32_value() {
        let ev = AudioEvent::modulation(1, 2, 0.75, 10);
        assert_eq!(ev.event_type, EVENT_MOD);
        assert_eq!(ev.target, 1);
        assert_eq!(ev.param, 2);
        assert!((ev.value - 0.75).abs() < 0.001);
        assert_eq!(ev.step, 10);
    }

    #[test]
    fn audio_event_trigger_fields() {
        let ev = AudioEvent::trigger(0, 1, 1.0, 3);
        assert_eq!(ev.event_type, EVENT_TRIGGER);
        assert_eq!(ev.source, 0);
        assert_eq!(ev.target, 1);
        assert!((ev.value - 1.0).abs() < 0.001);
        assert_eq!(ev.step, 3);
    }

    #[test]
    fn dead_consumer_does_not_block_the_ring() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_EVENTS_NAME).unwrap().as_ptr()) };
        let mut producer = EventRingbuf::create().expect("create");

        // a consumer joins, reads nothing, and "dies" (frozen index, dead pid)
        let consumer = EventRingbuf::open(4).expect("join slot 4");
        let dead_pid = {
            let c = std::process::Command::new("true").spawn().unwrap();
            let pid = c.id();
            let _ = c.wait_with_output();
            pid
        };
        unsafe { ptr::write_volatile(consumer.consumer_pid_ptr(4), dead_pid) };
        std::mem::forget(consumer); // skip Drop — simulate an unclean death

        // fill the ring past the corpse's horizon: must keep working
        for i in 0..600u32 {
            producer
                .write_event(&AudioEvent::note_on(60, 100, i))
                .unwrap_or_else(|e| panic!("write {} failed: {}", i, e));
        }

        // a live consumer joining now still works
        let mut live = EventRingbuf::open(0).expect("join live");
        producer
            .write_event(&AudioEvent::note_on(61, 100, 0))
            .expect("write after join");
        assert!(live.read_event().is_some(), "live consumer sees new events");
    }

    #[test]
    fn live_slow_consumer_still_applies_backpressure() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_EVENTS_NAME).unwrap().as_ptr()) };
        let mut producer = EventRingbuf::create().expect("create");
        let _live = EventRingbuf::open(0).expect("join"); // our own (live) pid, never reads

        let mut wrote = 0u32;
        for i in 0..600u32 {
            if producer
                .write_event(&AudioEvent::note_on(60, 100, i))
                .is_err()
            {
                break;
            }
            wrote += 1;
        }
        assert_eq!(
            wrote, 256,
            "a live lagging consumer must still bound the ring"
        );
    }

    #[test]
    fn consumer_drop_vacates_slot() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_EVENTS_NAME).unwrap().as_ptr()) };
        let mut producer = EventRingbuf::create().expect("create");
        {
            let _c = EventRingbuf::open(3).expect("join");
        } // clean Drop here
        for i in 0..600u32 {
            producer
                .write_event(&AudioEvent::note_on(60, 100, i))
                .expect("vacated slot must not block");
        }
    }

    #[test]
    fn no_consumers_means_no_backpressure() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_EVENTS_NAME).unwrap().as_ptr()) };
        let mut producer = EventRingbuf::create().expect("create");
        for i in 0..600u32 {
            producer
                .write_event(&AudioEvent::note_on(60, 100, i))
                .expect("writes must not stall with zero consumers");
        }
    }

    #[test]
    fn event_ringbuf_create_and_single_consumer() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(EventRingbuf::name()).unwrap().as_ptr()) };
        let mut rb = EventRingbuf::create().expect("create failed");
        let mut consumer = EventRingbuf::open(0).expect("open consumer 0 failed");

        let ev = AudioEvent::note_on(60, 100, 0);
        rb.write_event(&ev).expect("write failed");

        let read = consumer.read_event();
        assert!(read.is_some(), "consumer should see the event");
        let read_ev = read.unwrap();
        assert_eq!(read_ev.event_type, EVENT_NOTE_ON);
        assert_eq!(read_ev.param, 100);
    }

    #[test]
    fn event_ringbuf_multi_consumer_independent_reads() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(EventRingbuf::name()).unwrap().as_ptr()) };
        let mut rb = EventRingbuf::create().expect("create failed");
        let mut c0 = EventRingbuf::open(0).expect("open consumer 0");
        let mut c1 = EventRingbuf::open(1).expect("open consumer 1");

        let ev1 = AudioEvent::note_on(60, 100, 0);
        let ev2 = AudioEvent::note_on(64, 80, 1);
        rb.write_event(&ev1).unwrap();
        rb.write_event(&ev2).unwrap();

        // Both consumers should independently read both events
        let c0_ev1 = c0.read_event().expect("c0 first event");
        let c1_ev1 = c1.read_event().expect("c1 first event");
        assert_eq!(c0_ev1.param, 100);
        assert_eq!(c1_ev1.param, 100);

        let c0_ev2 = c0.read_event().expect("c0 second event");
        let c1_ev2 = c1.read_event().expect("c1 second event");
        assert_eq!(c0_ev2.param, 80);
        assert_eq!(c1_ev2.param, 80);

        // No more events for either
        assert!(c0.read_event().is_none());
        assert!(c1.read_event().is_none());
    }

    #[test]
    fn event_ringbuf_producer_blocks_on_full() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(EventRingbuf::name()).unwrap().as_ptr()) };
        let mut rb = EventRingbuf::create().expect("create failed");
        let _c0 = EventRingbuf::open(0).expect("open consumer 0");

        // Consumer 0 doesn't read. Fill the buffer.
        for i in 0..256u64 {
            let ev = AudioEvent::note_on(60, 100, i as u32);
            rb.write_event(&ev).expect("write should succeed");
        }

        let ev = AudioEvent::note_on(60, 100, 256);
        assert!(
            rb.write_event(&ev).is_err(),
            "producer should block when buffer full"
        );
    }

    #[test]
    fn event_ringbuf_producer_unblocks_after_consumer_reads() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(EventRingbuf::name()).unwrap().as_ptr()) };
        let mut rb = EventRingbuf::create().expect("create failed");
        let mut c0 = EventRingbuf::open(0).expect("open consumer 0");

        for i in 0..256u64 {
            let ev = AudioEvent::note_on(60, 100, i as u32);
            rb.write_event(&ev).unwrap();
        }

        let _ = c0.read_event();

        let ev = AudioEvent::note_on(60, 100, 256);
        assert!(
            rb.write_event(&ev).is_ok(),
            "producer should unblock after consumer reads"
        );
    }

    #[test]
    fn modulation_bus_create_and_rw() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MODBUS_NAME).unwrap().as_ptr()) };
        let mut bus = ModulationBus::create().expect("create modbus");

        bus.set(0, 0.75);
        bus.set(31, -0.5);

        assert!((bus.get(0) - 0.75).abs() < 0.001);
        assert!((bus.get(31) - (-0.5)).abs() < 0.001);
    }

    #[test]
    fn modulation_bus_open_reads_existing() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MODBUS_NAME).unwrap().as_ptr()) };
        let mut bus1 = ModulationBus::create().expect("create");
        bus1.set(5, 0.42);

        let bus2 = ModulationBus::open().expect("open");
        assert!((bus2.get(5) - 0.42).abs() < 0.001);
    }

    #[test]
    fn modulation_bus_out_of_bounds_returns_zero() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MODBUS_NAME).unwrap().as_ptr()) };
        let bus = ModulationBus::create().expect("create");
        assert_eq!(bus.get(32), 0.0);
        assert_eq!(bus.get(1000), 0.0);
    }

    #[test]
    fn modulation_bus_set_out_of_bounds_is_noop() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MODBUS_NAME).unwrap().as_ptr()) };
        let mut bus = ModulationBus::create().expect("create");
        bus.set(32, 1.0);
        bus.set(100, 1.0);
    }

    #[test]
    fn modulation_bus_initially_zero() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MODBUS_NAME).unwrap().as_ptr()) };
        let bus = ModulationBus::create().expect("create");
        for ch in 0..MODBUS_NUM_CHANNELS {
            assert_eq!(bus.get(ch), 0.0, "channel {} should be zero", ch);
        }
    }

    #[test]
    fn full_signal_chain_note_on_to_modbus() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();

        // Clean up both SHM objects
        let _ = unsafe { libc::shm_unlink(CString::new(EventRingbuf::name()).unwrap().as_ptr()) };
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MODBUS_NAME).unwrap().as_ptr()) };

        // Set up IPC
        let mut producer = EventRingbuf::create().expect("create events");
        let mut voice_consumer = EventRingbuf::open(0).expect("open voice consumer");
        let mut env_consumer = EventRingbuf::open(1).expect("open envelope consumer");
        let mut modbus = ModulationBus::create().expect("create modbus");

        // Step 1: Sequencer sends note_on (pitch + velocity)
        let note_ev = AudioEvent::note_on(60, 100, 0);
        producer.write_event(&note_ev).unwrap();

        // Step 2: Both voice and envelope receive the note_on
        let voice_ev = voice_consumer
            .read_event()
            .expect("voice should receive note_on");
        let env_ev = env_consumer
            .read_event()
            .expect("envelope should receive note_on");

        assert_eq!(voice_ev.event_type, EVENT_NOTE_ON);
        assert!(
            (voice_ev.value - 261.63).abs() < 0.1,
            "voice gets C4 frequency"
        );
        assert_eq!(voice_ev.param, 100, "voice gets velocity");

        assert_eq!(env_ev.event_type, EVENT_NOTE_ON);

        // Step 3: Envelope generates output and writes to modbus ch0
        modbus.set(0, 0.75); // envelope at 75%

        // Step 4: Voice reads envelope from modbus ch0
        let envelope_level = modbus.get(0);
        assert!(
            (envelope_level - 0.75).abs() < 0.001,
            "voice reads envelope level"
        );

        // Step 5: Voice amplitude = envelope × velocity
        let velocity = voice_ev.param as f32 / 127.0;
        let level = envelope_level * velocity;
        assert!(
            (level - (0.75 * 100.0 / 127.0)).abs() < 0.01,
            "amplitude = envelope * velocity"
        );

        // Step 6: Sequencer sends note_off
        let off_ev = AudioEvent::note_off(60, 1);
        producer.write_event(&off_ev).unwrap();

        let voice_off = voice_consumer.read_event().expect("voice gets note_off");
        let env_off = env_consumer.read_event().expect("envelope gets note_off");
        assert_eq!(voice_off.event_type, EVENT_NOTE_OFF);
        assert_eq!(env_off.event_type, EVENT_NOTE_OFF);
    }

    #[test]
    fn modulation_bus_open_then_create_fallback() {
        // This test verifies the bug fix: modules that only call open()
        // fail silently when modbus doesn't exist. They MUST fall back to create().
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MODBUS_NAME).unwrap().as_ptr()) };

        // open() without create() should fail
        assert!(
            ModulationBus::open().is_err(),
            "open() should fail when modbus doesn't exist"
        );

        // But create() should succeed
        let mut bus = ModulationBus::create().expect("create should succeed");
        bus.set(0, 0.42);

        // And now open() should succeed
        let bus2 = ModulationBus::open().expect("open should succeed after create");
        assert!((bus2.get(0) - 0.42).abs() < 0.001);

        // And the fallback pattern used in modules: open().or_else(|_| create())
        let bus3 = ModulationBus::open()
            .or_else(|_| ModulationBus::create())
            .expect("fallback works");
        assert!((bus3.get(0) - 0.42).abs() < 0.001);
    }

    // ── Manifest tests ──────────────────────────────────────────────────

    #[test]
    fn manifest_create_and_register() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MANIFEST_NAME).unwrap().as_ptr()) };
        let mut m = Manifest::create().expect("create manifest");

        let slot = m
            .register("voice", 0, Some("/los_audio_voice_0"), 0)
            .expect("register");
        assert_eq!(slot, 0, "first registration should get slot 0");

        let entries = m.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].module_name, "voice");
        assert_eq!(entries[0].instance, 0);
        assert_eq!(entries[0].audio_shm.as_deref(), Some("/los_audio_voice_0"));

        m.unregister();
        assert!(
            m.entries().is_empty(),
            "after unregister, entries should be empty"
        );
    }

    #[test]
    fn manifest_input_claim_round_trips() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MANIFEST_NAME).unwrap().as_ptr()) };
        let mut m = Manifest::create().expect("create manifest");
        m.register("delay", 7, Some("/los_audio_delay_7"), 0)
            .expect("register");

        let find = |m: &Manifest, name: &str| {
            m.entries()
                .into_iter()
                .find(|e| e.module_name == name)
                .unwrap()
        };
        assert_eq!(
            find(&m, "delay").input_shm,
            None,
            "no claim at registration"
        );

        m.publish_input(Some("/los_audio_voice_0"));
        assert_eq!(
            find(&m, "delay").input_shm.as_deref(),
            Some("/los_audio_voice_0")
        );
        m.publish_input(Some("/los_audio_tone_1"));
        assert_eq!(
            find(&m, "delay").input_shm.as_deref(),
            Some("/los_audio_tone_1"),
            "re-claim overwrites"
        );
        m.publish_input(None);
        assert_eq!(find(&m, "delay").input_shm, None, "claim cleared");

        // A reused slot must not leak the previous occupant's claim.
        m.publish_input(Some("/los_audio_voice_0"));
        let slot = m.my_slot.expect("registered");
        m.force_entry_pid(slot, 4_000_000); // simulate the claimant dying
        m.my_slot = None; // abandon without unregistering (no Drop cleanup)
        let mut m2 = Manifest::open().expect("open");
        m2.reap_dead();
        m2.register("scope", 0, None, 0)
            .expect("register into reused slot");
        assert_eq!(
            find(&m2, "scope").input_shm,
            None,
            "reused slot starts clean"
        );
    }

    #[test]
    fn manifest_multiple_modules() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MANIFEST_NAME).unwrap().as_ptr()) };
        let mut m = Manifest::create().expect("create manifest");

        m.register("sequencer", 0, None, 8)
            .expect("register sequencer");
        m.register("voice", 0, Some("/los_audio_voice_0"), 0)
            .expect("register voice 0");
        m.register("voice", 1, Some("/los_audio_voice_1"), 0)
            .expect("register voice 1");
        m.register("envelope", 0, None, 8)
            .expect("register envelope");

        let entries = m.entries();
        assert_eq!(entries.len(), 4);

        let voice_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.module_name == "voice")
            .collect();
        assert_eq!(voice_entries.len(), 2);
    }

    #[test]
    fn manifest_open_from_another_process() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MANIFEST_NAME).unwrap().as_ptr()) };
        let mut m1 = Manifest::create().expect("create manifest");
        m1.register("voice", 0, Some("/los_audio_voice_0"), 0)
            .expect("register");

        // Simulate another process opening the same manifest
        let m2 = Manifest::open().expect("open manifest");
        let entries = m2.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].module_name, "voice");
    }

    #[test]
    fn manifest_open_refuses_stale_version() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MANIFEST_NAME).unwrap().as_ptr()) };
        let m = Manifest::create().expect("create");
        // forge an old version in the header
        unsafe { ptr::write_unaligned(m.ptr as *mut u32, 1) };
        assert!(Manifest::open().is_err(), "v1 manifest must be refused");
        // the standard fallback chain recovers by re-initializing
        let m2 = Manifest::open()
            .or_else(|_| Manifest::create())
            .expect("recreate");
        assert!(m2.entries().is_empty());
        drop(m2);
        std::mem::forget(m); // owner drop would unlink mid-test otherwise
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MANIFEST_NAME).unwrap().as_ptr()) };
    }

    #[test]
    fn manifest_claims_modbus_channels() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MANIFEST_NAME).unwrap().as_ptr()) };
        let mut m = Manifest::create().expect("create manifest");

        m.register("sequencer", 0, None, 8).expect("register seq");
        assert_eq!(m.claimed_base(), Some(0), "first claimer gets base 0");

        let mut m2 = Manifest::open().expect("open");
        m2.register("envelope", 0, None, 8).expect("register env");
        assert_eq!(m2.claimed_base(), Some(8), "second claimer follows");

        let mut m3 = Manifest::open().expect("open");
        m3.register("voice", 0, None, 0).expect("register voice");
        assert_eq!(m3.claimed_base(), None, "no channels claimed");

        let entries = m.entries();
        let seq = entries
            .iter()
            .find(|e| e.module_name == "sequencer")
            .unwrap();
        assert_eq!((seq.mod_base, seq.mod_count), (Some(0), 8));
        let env = entries
            .iter()
            .find(|e| e.module_name == "envelope")
            .unwrap();
        assert_eq!((env.mod_base, env.mod_count), (Some(8), 8));
        let voice = entries.iter().find(|e| e.module_name == "voice").unwrap();
        assert_eq!((voice.mod_base, voice.mod_count), (None, 0));
    }

    #[test]
    fn modbus_channels_exhaust() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MANIFEST_NAME).unwrap().as_ptr()) };
        let mut m = Manifest::create().expect("create manifest");
        for i in 0..(MODBUS_CHANNELS / 8) {
            m.register("mod", i, None, 8).expect("claim");
            m.my_slot = None; // keep registering from the same handle
        }
        assert!(
            m.register("over", 0, None, 8).is_err(),
            "65th channel must fail"
        );
    }

    #[test]
    fn consumer_ids_are_disjoint() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..8 {
            assert!(seen.insert(consumer_id("voice", i)), "voice {} collides", i);
        }
        for i in 0..4 {
            assert!(
                seen.insert(consumer_id("envelope", i)),
                "envelope {} collides",
                i
            );
        }
    }

    #[test]
    fn reap_frees_dead_slots_and_resets_allocator() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MANIFEST_NAME).unwrap().as_ptr()) };
        let mut m = Manifest::create().expect("create");

        // a definitely-dead pid: spawn a child and wait for it
        let dead_pid = std::process::Command::new("true")
            .status()
            .map(|_| {
                let c = std::process::Command::new("true").spawn().unwrap();
                let pid = c.id();
                let _ = c.wait_with_output();
                pid
            })
            .unwrap();

        let s1 = m.register("sequencer", 0, None, 8).expect("seq");
        m.my_slot = None;
        let s2 = m.register("envelope", 0, None, 8).expect("env");
        m.my_slot = None;
        assert_eq!(m.next_channel(), 16);
        m.force_entry_pid(s1, dead_pid);
        m.force_entry_pid(s2, dead_pid);

        assert_eq!(m.reap_dead(), 2, "both dead entries reaped");
        assert!(m.entries().is_empty());
        assert_eq!(
            m.next_channel(),
            0,
            "allocator fully reset with no live claimers"
        );

        // and registration works again in the freed space
        m.register("sequencer", 0, None, 8).expect("re-register");
        assert_eq!(m.claimed_base(), Some(0));
    }

    #[test]
    fn reap_pops_dead_top_ranges_but_keeps_live() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MANIFEST_NAME).unwrap().as_ptr()) };
        let mut m = Manifest::create().expect("create");
        let dead_pid = {
            let c = std::process::Command::new("true").spawn().unwrap();
            let pid = c.id();
            let _ = c.wait_with_output();
            pid
        };

        m.register("sequencer", 0, None, 8).expect("live seq"); // live, ch0-7
        m.my_slot = None;
        let s2 = m.register("envelope", 0, None, 8).expect("dead env"); // ch8-15
        m.my_slot = None;
        m.force_entry_pid(s2, dead_pid);

        m.reap_dead();
        assert_eq!(m.entries().len(), 1, "live entry survives");
        assert_eq!(
            m.next_channel(),
            8,
            "dead top range reclaimed, live claim kept"
        );
    }

    #[test]
    fn register_self_heals_a_full_manifest_of_dead_entries() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MANIFEST_NAME).unwrap().as_ptr()) };
        let mut m = Manifest::create().expect("create");
        let dead_pid = {
            let c = std::process::Command::new("true").spawn().unwrap();
            let pid = c.id();
            let _ = c.wait_with_output();
            pid
        };
        for i in 0..MANIFEST_MAX_ENTRIES {
            let slot = m.register("mod", i, None, 0).expect("fill");
            m.my_slot = None;
            m.force_entry_pid(slot, dead_pid);
        }
        // full of corpses — registration must reap and succeed
        m.register("voice", 0, None, 0)
            .expect("register over corpses");
        assert_eq!(m.entries().len(), 1);
    }

    #[test]
    fn manifest_full() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let _ = unsafe { libc::shm_unlink(CString::new(SHM_MANIFEST_NAME).unwrap().as_ptr()) };
        let mut m = Manifest::create().expect("create manifest");

        // Fill all 16 slots
        for i in 0..16 {
            m.register("mod", i, None, 0).expect("register");
        }

        // Next registration should fail
        assert!(m.register("overflow", 0, None, 0).is_err());
    }

    #[test]
    fn audio_ringbuf_write_and_read_separate_handles() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let name = "/los_audio_test";
        let _ = unsafe { libc::shm_unlink(CString::new(name).unwrap().as_ptr()) };

        let mut writer = AudioRingbuf::create(name).expect("create ringbuf");
        let mut reader = AudioRingbuf::open(name).expect("open ringbuf");

        // Write a known pattern
        let data: Vec<f32> = (0..writer.slot_len()).map(|i| i as f32 * 0.01).collect();
        writer.write(&data).expect("write should succeed");

        // Read it back from the other handle
        let mut buf = vec![0.0f32; reader.slot_len()];
        let result = reader.read(&mut buf).expect("read should succeed");
        assert!(result, "read should return true (data available)");
        assert_eq!(buf, data, "read data should match written data");

        // Second read should return false (no more data)
        let result = reader.read(&mut buf).expect("second read should succeed");
        assert!(!result, "second read should return false (empty)");
    }

    #[test]
    fn audio_ringbuf_multiple_slots() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let name = "/los_audio_test_multi";
        let _ = unsafe { libc::shm_unlink(CString::new(name).unwrap().as_ptr()) };

        let mut writer = AudioRingbuf::create(name).expect("create ringbuf");
        let mut reader = AudioRingbuf::open(name).expect("open ringbuf");

        // Write multiple slots
        for slot in 0..5u32 {
            let data: Vec<f32> = (0..writer.slot_len())
                .map(|i| (slot as f32 * 100.0 + i as f32) * 0.01)
                .collect();
            writer.write(&data).expect("write should succeed");
        }

        // Read them back
        let mut buf = vec![0.0f32; reader.slot_len()];
        for slot in 0..5u32 {
            let result = reader.read(&mut buf).expect("read should succeed");
            assert!(result, "read {} should return true", slot);
            let expected: Vec<f32> = (0..reader.slot_len())
                .map(|i| (slot as f32 * 100.0 + i as f32) * 0.01)
                .collect();
            assert_eq!(buf, expected, "read {} data mismatch", slot);
        }
    }

    #[test]
    fn audio_ringbuf_empty_read_returns_false() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let name = "/los_audio_test_empty";
        let _ = unsafe { libc::shm_unlink(CString::new(name).unwrap().as_ptr()) };

        let _writer = AudioRingbuf::create(name).expect("create ringbuf");
        let mut reader = AudioRingbuf::open(name).expect("open ringbuf");

        let mut buf = vec![0.0f32; reader.slot_len()];
        let result = reader.read(&mut buf).expect("read should succeed");
        assert!(!result, "read should return false (empty)");
    }

    #[test]
    fn audio_ringbuf_full_blocks_writer() {
        let _guard = SHM_TEST_MUTEX.lock().unwrap();
        let name = "/los_audio_test_full";
        let _ = unsafe { libc::shm_unlink(CString::new(name).unwrap().as_ptr()) };

        let mut writer = AudioRingbuf::create(name).expect("create ringbuf");
        let _reader = AudioRingbuf::open(name).expect("open ringbuf");

        // Fill all slots
        let data = vec![1.0f32; writer.slot_len()];
        for i in 0..writer.num_slots() {
            writer
                .write(&data)
                .unwrap_or_else(|_| panic!("write {} should succeed", i));
        }

        // Next write should fail (full)
        assert!(
            writer.write(&data).is_err(),
            "write should fail when ringbuf is full"
        );
    }
}
