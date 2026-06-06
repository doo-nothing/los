use std::ffi::CString;
use std::ptr;
use std::sync::atomic::{compiler_fence, Ordering};

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
    slot_len: usize,  // total floats per slot
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
            let fd = libc::shm_open(
                cname.as_ptr(),
                libc::O_CREAT | libc::O_RDWR,
                0o644,
            );
            if fd < 0 {
                anyhow::bail!("shm_open failed for {name}: {}", std::io::Error::last_os_error());
            }
            // Set the size
            if libc::ftruncate(fd, total_size as libc::off_t) < 0 {
                libc::close(fd);
                libc::shm_unlink(cname.as_ptr());
                anyhow::bail!("ftruncate failed for {name}: {}", std::io::Error::last_os_error());
            }
            fd
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
                anyhow::bail!("mmap failed for {name}: {}", std::io::Error::last_os_error());
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
            ptr::write_unaligned(ptr as *mut u64, 0);        // write_index
            ptr::write_unaligned(ptr.add(8) as *mut u64, 0); // read_index
            ptr::write_unaligned(ptr.add(16) as *mut u32, channels as u32);
            ptr::write_unaligned(ptr.add(20) as *mut u32, slot_frames);
            ptr::write_unaligned(ptr.add(24) as *mut u32, num_slots);
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
                anyhow::bail!("shm_open failed for {name}: {}", std::io::Error::last_os_error());
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
                anyhow::bail!("mmap failed for {name}: {}", std::io::Error::last_os_error());
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

    /// Current write index (total slots produced so far).
    pub fn write_index(&self) -> u64 {
        atomic_load_acquire(self.write_idx_ptr())
    }
}

// ── ShmTransport ────────────────────────────────────────────────────────────

/// Shared transport state: clock counter, sample rate, play flag.
///
/// Layout:
///   [0..8)    clock       : u64  — total samples consumed (mixer writes, voices read)
///   [8..12)   sample_rate : u32  — sample rate in Hz
///   [12..16)  flags       : u32  — bit 0 = playing
///   [16..32)  reserved
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
            let cname = CString::new("/los_transport").unwrap();
            unsafe { libc::shm_unlink(cname.as_ptr()) };
        }
    }
}

impl ShmTransport {
    fn name() -> &'static str {
        "/los_transport"
    }

    pub fn create(sample_rate: u32) -> Result<Self> {
        let total = 64usize;
        let cname = CString::new(Self::name()).unwrap();

        let fd = unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644);
            if fd < 0 {
                anyhow::bail!("shm_open({}) failed: {}", Self::name(), std::io::Error::last_os_error());
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
                anyhow::bail!("mmap({}) failed: {}", Self::name(), std::io::Error::last_os_error());
            }
            p as *mut u8
        };

        unsafe {
            ptr::write_unaligned(ptr as *mut u64, 0);                    // clock
            ptr::write_unaligned(ptr.add(8) as *mut u32, sample_rate);   // sample_rate
            ptr::write_unaligned(ptr.add(12) as *mut u32, 1);            // playing
        }

        Ok(Self { ptr, fd, owned: true })
    }

    pub fn open() -> Result<Self> {
        let total = 64usize;
        let cname = CString::new(Self::name()).unwrap();

        let fd = unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0);
            if fd < 0 {
                anyhow::bail!("shm_open({}) failed: {}", Self::name(), std::io::Error::last_os_error());
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
                anyhow::bail!("mmap({}) failed: {}", Self::name(), std::io::Error::last_os_error());
            }
            p as *mut u8
        };

        Ok(Self { ptr, fd, owned: false })
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

    pub fn playing(&self) -> bool {
        let flags: u32 = unsafe { ptr::read_unaligned(self.ptr.add(12) as *const u32) };
        flags & 1 != 0
    }

    pub fn add_clock_frames(&mut self, frames: u64) {
        let cur = self.clock();
        self.set_clock(cur + frames);
    }
}

// ── AudioEvent ──────────────────────────────────────────────────────────────

/// A single event message (32 bytes fixed size) in shared memory.
#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct AudioEvent {
    pub event_type: u8,  // 0 = note_on, 1 = note_off
    pub note: u8,        // MIDI note 0–127
    pub velocity: u8,    // 0–127
    _pad: [u8; 1],
    pub step: u32,       // step index that triggered this
    _reserved: [u8; 24],
}

const _: [(); 1] = [(); (core::mem::size_of::<AudioEvent>() == 32) as usize];

impl AudioEvent {
    pub fn note_on(note: u8, velocity: u8, step: u32) -> Self {
        Self {
            event_type: 0,
            note,
            velocity,
            step,
            ..Default::default()
        }
    }

    pub fn note_off(note: u8, step: u32) -> Self {
        Self {
            event_type: 1,
            note,
            step,
            ..Default::default()
        }
    }

    pub fn is_note_on(&self) -> bool {
        self.event_type == 0
    }

    pub fn is_note_off(&self) -> bool {
        self.event_type == 1
    }
}

// ── EventRingbuf ───────────────────────────────────────────────────────────

const EVENT_SIZE: usize = 32;

/// Lock-free SPSC ringbuffer for fixed-size events backed by POSIX SHM.
///
/// Layout:
///   [0..8)    write_index : u64
///   [8..16)   read_index  : u64
///   [16..64)  reserved
///   [64..)    event data  (EVENT_SIZE bytes each)
pub struct EventRingbuf {
    fd: i32,
    ptr: *mut u8,
    num_slots: u32,
    total_size: usize,
    owned: bool,
}

unsafe impl Send for EventRingbuf {}

impl Drop for EventRingbuf {
    fn drop(&mut self) {
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
        "/los_events"
    }

    fn write_idx_ptr(&self) -> *mut u64 {
        self.ptr as *mut u64
    }

    fn read_idx_ptr(&self) -> *mut u64 {
        unsafe { self.ptr.add(8) as *mut u64 }
    }

    fn slot_ptr(&self, index: u64) -> *mut u8 {
        let slot = index as usize % self.num_slots as usize;
        let offset = DATA_OFFSET + slot * EVENT_SIZE;
        unsafe { self.ptr.add(offset) }
    }

    pub fn create() -> Result<Self> {
        let num_slots = 256u32;
        let data_bytes = num_slots as usize * EVENT_SIZE;
        let total_size = DATA_OFFSET + data_bytes;
        let cname = CString::new(Self::name()).unwrap();

        let fd = unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644);
            if fd < 0 {
                anyhow::bail!("shm_open({}) failed: {}", Self::name(), std::io::Error::last_os_error());
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
                anyhow::bail!("mmap({}) failed: {}", Self::name(), std::io::Error::last_os_error());
            }
            p as *mut u8
        };

        unsafe {
            ptr::write_unaligned(ptr as *mut u64, 0);
            ptr::write_unaligned(ptr.add(8) as *mut u64, 0);
        }

        Ok(Self { fd, ptr, num_slots, total_size, owned: true })
    }

    pub fn open() -> Result<Self> {
        let num_slots = 256u32;
        let data_bytes = num_slots as usize * EVENT_SIZE;
        let total_size = DATA_OFFSET + data_bytes;
        let cname = CString::new(Self::name()).unwrap();

        let fd = unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0);
            if fd < 0 {
                anyhow::bail!("shm_open({}) failed: {}", Self::name(), std::io::Error::last_os_error());
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
                anyhow::bail!("mmap({}) failed: {}", Self::name(), std::io::Error::last_os_error());
            }
            p as *mut u8
        };

        Ok(Self { fd, ptr, num_slots, total_size, owned: false })
    }

    pub fn write_event(&mut self, event: &AudioEvent) -> Result<()> {
        let w = atomic_load_acquire(self.write_idx_ptr());
        let r = atomic_load_acquire(self.read_idx_ptr());

        if w - r >= self.num_slots as u64 {
            anyhow::bail!("event buffer full");
        }

        let dest = self.slot_ptr(w);
        unsafe {
            ptr::copy_nonoverlapping(
                event as *const AudioEvent as *const u8,
                dest,
                EVENT_SIZE,
            );
        }
        atomic_store_release(self.write_idx_ptr(), w + 1);
        Ok(())
    }

    pub fn read_event(&mut self) -> Option<AudioEvent> {
        let w = atomic_load_acquire(self.write_idx_ptr());
        let r = atomic_load_acquire(self.read_idx_ptr());

        if w <= r {
            return None;
        }

        let src = self.slot_ptr(r);
        let mut event = AudioEvent::default();
        unsafe {
            ptr::copy_nonoverlapping(
                src,
                &mut event as *mut AudioEvent as *mut u8,
                EVENT_SIZE,
            );
        }
        atomic_store_release(self.read_idx_ptr(), r + 1);
        Some(event)
    }
}
