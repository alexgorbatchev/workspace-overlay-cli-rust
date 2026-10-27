---
created_on: 2026-10-14 20:15
last_modified: 2026-10-14 21:27
status: current
---

# Native FUSE patches

This directory contains fuser 0.18.0 from the Cargo registry, under its retained MIT license. The original crate archive SHA-256 is `b82b6597d216503555ead6b358f341ef748869bf5c6fbae6a0cb9dd231baecfd`.

Workspace Overlay requires ordinary unmount failures to remain recoverable. The pure Rust unmount helper uses `fusermount -u --` with native pathname bytes, reports a failed helper status, and omits lazy or forced detach. Mount handles are retained after failed explicit unmounts. `BackgroundSession::unmount(&mut self)` exposes a retryable operation before joining the server thread.

The filesystem trait's `setlk` receives the kernel's raw lock flags, including `FUSE_LK_FLOCK`; the low-level dispatcher forwards them without discarding their identity. This lets the overlay distinguish native `flock` locks from POSIX record locks. The `ioctl` trait method similarly receives the native argument value, so pointer-free operations retain their actual numeric argument. The corresponding ioctl example follows the extended signature.

`ReplyEntry::negative` emits a native negative entry with inode zero, generation zero, an explicit entry TTL, and zero attribute TTL. The ABI's integer-only `fuse_attr` derives `Default` to initialize its unused fields. Caller-invariant missing project paths can therefore use the kernel's actual negative-entry cache while caller-dependent paths remain uncached.

The parent project's native FUSE tests exercise ordinary busy-unmount retry and recovery, backing-description `flock`, supported ioctls, and metadata cache invalidation. These checks do not claim to run the upstream XFS or PJDFS suites. The retained upstream source, license, examples, and tests remain attributable to the fuser authors.
