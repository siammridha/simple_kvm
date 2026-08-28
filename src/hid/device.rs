//! `Ch9329Driver`: the CH9329's `DeviceDriver` implementation (see
//! `crate::device`), wrapping today's serial-port open logic
//! (`super::writer::open`) rather than reimplementing it. Presence
//! detection itself - the `Path::exists` check, kernel `tty` uevent
//! watching, first-check-skips-probe-at-boot behavior - is fully owned by
//! the generic `Device<D>` core; this module only supplies what's specific
//! to the CH9329: how to open a serial port at a given path.
//!
//! Unlike the capture card, which probes for supported resolutions, the
//! CH9329 has no separate capability worth learning beyond presence - so
//! `probe` is a no-op. The actual serial-port open still only happens once
//! `SerialWriter` (`super::writer`) has a command to write, same as today.

use serialport::SerialPort;

use crate::device::{Device, DeviceDriver, OpenError};

pub type Ch9329Device = Device<Ch9329Driver>;

pub struct Ch9329Driver;

impl DeviceDriver for Ch9329Driver {
    type Info = ();
    type Settings = ();
    type Open = Box<dyn SerialPort>;

    fn probe(_device_path: &str) -> Option<Self::Info> {
        Some(())
    }

    fn open(device_path: &str, _settings: &Self::Settings) -> Result<Self::Open, OpenError> {
        match super::writer::open(device_path) {
            Ok(Some(port)) => Ok(port),
            Ok(None) => Err(OpenError("CH9329 not present".to_string())),
            Err(err) => Err(OpenError(err.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_always_reports_present_with_no_extra_info() {
        assert_eq!(Ch9329Driver::probe("/dev/does-not-matter"), Some(()));
    }

    #[test]
    fn open_fails_when_no_device_at_path() {
        let result = Ch9329Driver::open("/dev/definitely-not-a-real-ch9329-path", &());
        assert!(result.is_err());
    }
}
