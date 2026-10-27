//! Native operations on one backing open description.
use crate::{file::OpenFile, view::errno};
use std::{io, os::fd::AsRawFd};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Lock {
    pub start: u64,
    pub end: u64,
    pub typ: i32,
    pub pid: u32,
}
impl Lock {
    fn native(self) -> io::Result<libc::flock> {
        if self.start > i64::MAX as u64
            || self.end < self.start
            || ![libc::F_RDLCK, libc::F_WRLCK, libc::F_UNLCK].contains(&self.typ)
        {
            return Err(errno(libc::EINVAL));
        }
        let length = if self.end == u64::MAX || self.end == i64::MAX as u64 {
            0
        } else {
            i64::try_from(self.end - self.start + 1).map_err(|_| errno(libc::EOVERFLOW))?
        };
        Ok(libc::flock {
            l_type: self.typ as i16,
            l_whence: libc::SEEK_SET as i16,
            l_start: self.start as i64,
            l_len: length,
            l_pid: 0,
        })
    }
}
impl OpenFile {
    fn native_operation<T>(
        &self,
        operation: impl FnOnce(&std::fs::File) -> io::Result<T>,
    ) -> io::Result<T> {
        if self.direct_io() {
            return Err(errno(libc::ENOTSUP));
        }
        self.native(operation)
    }
    pub(crate) fn seek(&self, offset: i64, whence: i32) -> io::Result<i64> {
        self.native_operation(|file| {
            // SAFETY: the read guard in native_operation retains this descriptor.
            let result = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
            if result < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(result)
            }
        })
    }
    pub(crate) fn allocate(&self, offset: u64, length: u64, mode: i32) -> io::Result<()> {
        self.native_operation(|file| {
            let offset = i64::try_from(offset).map_err(|_| errno(libc::EINVAL))?;
            let length = i64::try_from(length).map_err(|_| errno(libc::EINVAL))?;
            // SAFETY: the descriptor is borrowed for this syscall; offsets are checked.
            if unsafe { libc::fallocate(file.as_raw_fd(), mode, offset, length) } < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        })
    }
    pub(crate) fn get_lock(&self, requested: Lock) -> io::Result<Lock> {
        self.native_operation(|file| {
            let mut lock = requested.native()?;
            // SAFETY: lock is a fully initialized flock writable for the syscall.
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_GETLK, &mut lock) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Lock {
                start: lock.l_start as u64,
                end: if lock.l_len == 0 {
                    i64::MAX as u64
                } else {
                    (lock.l_start + lock.l_len - 1) as u64
                },
                typ: i32::from(lock.l_type),
                pid: lock.l_pid as u32,
            })
        })
    }
    /// Always nonblocking; the FUSE owner schedules blocking requests separately.
    pub(crate) fn set_lock(&self, requested: Lock, flock: bool) -> io::Result<()> {
        self.native_operation(|file| {
            let result = if flock {
                let operation = match requested.typ {
                    libc::F_RDLCK => libc::LOCK_SH,
                    libc::F_WRLCK => libc::LOCK_EX,
                    libc::F_UNLCK => libc::LOCK_UN,
                    _ => return Err(errno(libc::EINVAL)),
                };
                // SAFETY: this syscall borrows the retained descriptor.
                unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) }
            } else {
                let lock = requested.native()?;
                // SAFETY: lock is initialized and retained for the syscall.
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &lock) }
            };
            if result < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        })
    }
    pub(crate) fn ioctl(
        &self,
        command: u32,
        _argument: u64,
        input: &[u8],
        output_size: u32,
    ) -> io::Result<(i32, Vec<u8>)> {
        self.native_operation(|file| {
            // Linux's restricted FUSE ioctl protocol copies _IOC buffers, never
            // the caller's virtual address into the server's address space.
            // Unknown ioctls may contain nested pointers or lengths extending
            // beyond _IOC_SIZE (for example FIEMAP); they are not safe to forward.
            if ![
                libc::FS_IOC_GETFLAGS,
                libc::FS_IOC_SETFLAGS,
                libc::FS_IOC_GETVERSION,
                libc::FS_IOC_SETVERSION,
                libc::FS_IOC32_GETFLAGS,
                libc::FS_IOC32_SETFLAGS,
                libc::FS_IOC32_GETVERSION,
                libc::FS_IOC32_SETVERSION,
            ]
            .contains(&(command as libc::c_ulong))
            {
                return Err(errno(libc::ENOTTY));
            }
            let direction = command >> 30;
            let size = ((command >> 16) & 0x3fff) as usize;
            if direction == 0 || size == 0 {
                return Err(errno(libc::ENOTTY));
            }
            let writes = direction & 1 != 0;
            let reads = direction & 2 != 0;
            // fuse_fileattr_get/set uses a native u32 even though GETFLAGS
            // encodes sizeof(long). Ordinary requests retain their encoded size.
            let transferred = if reads {
                output_size as usize
            } else {
                input.len()
            };
            if transferred != 4 && transferred != size {
                return Err(errno(libc::EINVAL));
            }
            let mut buffer = vec![0u8; size];
            if writes {
                buffer[..input.len()].copy_from_slice(input);
            }
            // SAFETY: restricted FUSE supplies an encoded, bounded byte buffer.
            // The kernel copies user memory; it does not dereference it as Rust data.
            let result = unsafe {
                libc::ioctl(
                    file.as_raw_fd(),
                    command as libc::c_ulong,
                    buffer.as_mut_ptr(),
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            if reads {
                buffer.truncate(transferred);
            } else {
                buffer.clear();
            }
            Ok((result, buffer))
        })
    }
}
