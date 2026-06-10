//! Inter-process plumbing: how modules find each other and exchange signal.
//!
//! [`shm`] owns all POSIX shared-memory primitives — the module manifest,
//! lock-free audio ring buffers, the multi-consumer event ring, the
//! 64-channel modulation bus, and the global transport clock. [`routing`]
//! names modulation sources (`module/instance/output`) and resolves those
//! addresses to live modbus channels through the manifest.

pub mod routing;
pub mod shm;
