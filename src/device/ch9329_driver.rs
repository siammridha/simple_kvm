//! `DeviceDriver` implementation for the CH9329: opening its serial port.
//! Lives here for the same reason `capture_driver` does - opening is a
//! path-touching call, and paths never leave this module.
//!
//! Unlike the capture card, which probes for supported resolutions, the
//! CH9329 has no separate capability worth learning beyond presence - so
//! `probe` is a no-op.

use std::time::Duration;

use serialport::SerialPort;

use super::{Device, DeviceDriver, OpenError};

const BAUD_RATE: u32 = 9600;
const OPEN_TIMEOUT: Duration = Duration::from_millis(500);

pub type Ch9329Device = Device<Ch9329Driver>;

pub struct Ch9329Driver;

impl DeviceDriver for Ch9329Driver {
    const UEVENT_SUBSYSTEM: &'static str = "tty";
    const PATH_ENV_VAR: &'static str = "SERIAL_PATH";
    const DEFAULT_PATH: &'static str = "/dev/ttyUSB0";

    type Info = ();
    type Settings = ();
    type Open = Box<dyn SerialPort>;

    fn probe(_device_path: &str) -> Option<Self::Info> {
        Some(())
    }

    fn open(device_path: &str, _settings: &Self::Settings) -> Result<Self::Open, OpenError> {
        serialport::new(device_path, BAUD_RATE).timeout(OPEN_TIMEOUT).open().map_err(|err| OpenError(format!("opening CH9329 serial port: {err}")))
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
