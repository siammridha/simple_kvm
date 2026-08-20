//! Listens directly on `NETLINK_KOBJECT_UEVENT` — the kernel's own
//! broadcast of device add/remove events, the same channel udev listens
//! on. Used directly here instead of going through libudev: the device
//! this runs on has no udev daemon (it uses `mdev` instead), so
//! libudev's event source never fires. The kernel's raw broadcast works
//! no matter what (if anything) is managing devices, so it's the one
//! approach guaranteed to work here.
//!
//! A received event is treated purely as a "something changed, go check
//! reality" signal — callers re-check the actual device path themselves
//! rather than trusting the event's own contents, so there's no need to
//! parse anything beyond which subsystem it's about.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use tokio::io::unix::AsyncFd;

const NETLINK_KOBJECT_UEVENT: libc::c_int = 15;
/// The kernel's own multicast group, as opposed to the (unused here)
/// group `2` udev daemons use to re-broadcast events after tagging them.
const KOBJECT_UEVENT_GROUP: u32 = 1;

pub struct UeventListener {
    fd: AsyncFd<OwnedFd>,
}

impl UeventListener {
    pub fn open() -> io::Result<Self> {
        // SAFETY: standard socket(2)/bind(2) setup; every call's return
        // value is checked below before the raw fd is trusted.
        let raw = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, NETLINK_KOBJECT_UEVENT) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let mut addr: libc::sockaddr_nl = unsafe { mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_pid = 0;
        addr.nl_groups = KOBJECT_UEVENT_GROUP;

        // SAFETY: `addr` is a valid, fully-initialized sockaddr_nl for the
        // lifetime of this call, and its size matches what's passed.
        let ret = unsafe { libc::bind(fd.as_raw_fd(), (&raw const addr).cast(), mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { fd: AsyncFd::new(fd)? })
    }

    /// Waits for the next uevent belonging to `subsystem` (e.g.
    /// `"video4linux"`), ignoring every other event in the meantime.
    pub async fn wait_for_subsystem(&mut self, subsystem: &str) {
        let needle = format!("SUBSYSTEM={subsystem}");
        loop {
            let Ok(mut guard) = self.fd.readable().await else { return };
            let mut buf = [0u8; 4096];
            let received = guard.try_io(|fd| {
                // SAFETY: `buf` is valid for `buf.len()` bytes for the
                // duration of this call.
                let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
                if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
            });
            match received {
                Ok(Ok(n)) if contains_field(&buf[..n], needle.as_bytes()) => return,
                Ok(Ok(_)) => {}   // unrelated event - keep waiting
                Ok(Err(_)) => return, // real socket error - caller falls back to its own polling
                Err(_would_block) => {}
            }
        }
    }
}

/// Uevent messages are ASCII text fields separated by NUL bytes (e.g.
/// `ACTION=add\0SUBSYSTEM=video4linux\0DEVNAME=video0\0...`); a plain
/// substring search over the raw bytes is enough to tell whether one of
/// those fields is present, without needing a real parser.
fn contains_field(buf: &[u8], needle: &[u8]) -> bool {
    buf.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_field_finds_a_nul_separated_field() {
        let buf = b"add@/devices/foo\0ACTION=add\0SUBSYSTEM=video4linux\0DEVNAME=video0\0";
        assert!(contains_field(buf, b"SUBSYSTEM=video4linux"));
        assert!(!contains_field(buf, b"SUBSYSTEM=usb"));
    }
}
