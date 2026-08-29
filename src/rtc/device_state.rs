//! `DeviceState`/`ResolutionFrameRates` and the pure function that computes
//! them - moved here from `capture` (issue #026): `rtc` is now the only
//! thing holding both the capture card's probed capabilities (via its own
//! `CaptureDevice` handle, see `super::session`) and its currently-applied
//! settings (`capture.settings()`), so it's the one place that can compute
//! this without either side subscribing to something it doesn't otherwise
//! need.

use serde::Serialize;

use crate::capture::{CaptureSettings, Resolution, SupportedFormat};

/// Live state of the capture card itself — whether it's plugged in right
/// now, and what resolutions/frame rates it supports. Computed by `rtc`
/// (see `session::current_device_state`), the only thing holding both the
/// card's reported capabilities (via its `CaptureDevice` handle) and the
/// currently-applied settings, and pushed to the web page over the
/// `control` data channel (see `rtc::session::handle`) so an already-open
/// tab reflects a hot-plug/unplug instead of being frozen at
/// server-startup values.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DeviceState {
    pub available: bool,
    pub resolutions: Vec<Resolution>,
    pub default_resolution: Option<Resolution>,
    pub frame_rates: Vec<ResolutionFrameRates>,
}

/// One resolution's discrete frame-rate list — `Vec` rather than a
/// `Resolution`-keyed map, since JSON object keys must be strings and
/// `Resolution` isn't one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolutionFrameRates {
    pub resolution: Resolution,
    pub rates: Vec<u32>,
}

/// Cheap and ioctl-free, so it's safe to run on every presence event or
/// settings change, unlike an actual probe. Kept as a free function over
/// `&Option<SupportedFormat>` rather than tied to any particular struct.
pub(super) fn device_state_for(format: &Option<SupportedFormat>, settings: &CaptureSettings) -> DeviceState {
    let Some(format) = format else {
        return DeviceState::default();
    };
    let default_resolution = if format.resolutions.contains(&settings.resolution) { Some(settings.resolution) } else { format.resolutions.first().copied() };
    let frame_rates = format
        .resolutions
        .iter()
        .map(|&resolution| {
            // Falls back to just the currently-applied fps if the card
            // didn't report a discrete list for this resolution — the
            // dropdown should never be empty, and the applied value is
            // always a valid option.
            let rates = format.frame_rates.get(&resolution).cloned().unwrap_or_else(|| vec![settings.fps]);
            ResolutionFrameRates { resolution, rates }
        })
        .collect();
    DeviceState { available: true, resolutions: format.resolutions.clone(), default_resolution, frame_rates }
}

/// The default settings to apply once the device's capabilities are known
/// and nobody has picked any by hand yet: its own first reported
/// resolution, and that resolution's own first reported frame rate. Never
/// a value baked into this codebase (issue #032) - `None` only if the
/// device reports a resolution with no frame rates listed for it at all,
/// which nothing in this codebase produces today, but this stays honest
/// about it rather than inventing one.
pub(super) fn first_reported_settings(format: &SupportedFormat) -> Option<CaptureSettings> {
    let resolution = *format.resolutions.first()?;
    let fps = format.frame_rates.get(&resolution)?.first().copied()?;
    Some(CaptureSettings { resolution, fps })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn settings(width: u32, height: u32, fps: u32) -> CaptureSettings {
        CaptureSettings { resolution: Resolution { width, height }, fps }
    }

    #[test]
    fn no_format_yet_reports_unavailable() {
        assert_eq!(device_state_for(&None, &settings(1280, 720, 5)), DeviceState::default());
    }

    #[test]
    fn reports_the_card_s_resolutions_and_falls_back_to_the_applied_fps_when_undiscovered() {
        let resolution = Resolution { width: 1920, height: 1080 };
        let format = SupportedFormat { resolutions: vec![resolution], frame_rates: HashMap::new() };

        let state = device_state_for(&Some(format), &settings(1920, 1080, 30));

        assert!(state.available);
        assert_eq!(state.resolutions, vec![resolution]);
        assert_eq!(state.default_resolution, Some(resolution));
        assert_eq!(state.frame_rates, vec![ResolutionFrameRates { resolution, rates: vec![30] }]);
    }

    #[test]
    fn falls_back_to_the_first_reported_resolution_when_the_applied_one_is_unsupported() {
        let first = Resolution { width: 640, height: 480 };
        let format = SupportedFormat { resolutions: vec![first, Resolution { width: 1920, height: 1080 }], frame_rates: HashMap::new() };

        // Nothing in `format.resolutions` matches this - the applied
        // setting is stale relative to what the card actually reports.
        let state = device_state_for(&Some(format), &settings(3840, 2160, 30));

        assert_eq!(state.default_resolution, Some(first), "applied resolution isn't supported, so the first reported one wins");
    }

    #[test]
    fn first_reported_settings_uses_the_only_resolution_and_fps_reported() {
        let resolution = Resolution { width: 1920, height: 1080 };
        let format = SupportedFormat { resolutions: vec![resolution], frame_rates: HashMap::from([(resolution, vec![30])]) };

        assert_eq!(first_reported_settings(&format), Some(settings(1920, 1080, 30)));
    }

    #[test]
    fn first_reported_settings_picks_the_first_resolution_and_its_own_first_fps() {
        let first = Resolution { width: 640, height: 480 };
        let second = Resolution { width: 1920, height: 1080 };
        let format = SupportedFormat { resolutions: vec![first, second], frame_rates: HashMap::from([(first, vec![15, 30]), (second, vec![60])]) };

        assert_eq!(first_reported_settings(&format), Some(settings(640, 480, 15)), "must use the first resolution's own first fps, not some other resolution's");
    }

    #[test]
    fn first_reported_settings_is_none_when_the_first_resolution_has_no_frame_rates_listed() {
        let resolution = Resolution { width: 1920, height: 1080 };
        let format = SupportedFormat { resolutions: vec![resolution], frame_rates: HashMap::new() };

        assert_eq!(first_reported_settings(&format), None);
    }
}
