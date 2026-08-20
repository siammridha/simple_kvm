//! Persists the live-adjustable settings (video mode, resolution, mouse
//! mode) to a JSON file, so a dropdown change survives a service restart
//! instead of resetting to the capture card's defaults every time.

use std::path::{Path, PathBuf};

use tokio::sync::watch;

use crate::config::{CaptureSettings, MouseMode, PersistedSettings};

/// Reads the settings file, if any. Returns `None` if it doesn't exist yet
/// or can't be parsed — either way, the caller falls back to the capture
/// card's own defaults, never fails startup.
pub fn load(path: &Path) -> Option<PersistedSettings> {
    let data = std::fs::read(path).ok()?;
    match serde_json::from_slice(&data) {
        Ok(settings) => Some(settings),
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "ignoring unreadable settings file");
            None
        }
    }
}

fn save(path: &Path, settings: PersistedSettings) {
    let data = match serde_json::to_vec_pretty(&settings) {
        Ok(data) => data,
        Err(err) => {
            tracing::warn!(%err, "failed to serialize settings");
            return;
        }
    };
    if let Err(err) = std::fs::write(path, data) {
        tracing::warn!(%err, path = %path.display(), "failed to save settings");
    }
}

/// Runs forever, writing the settings file whenever the capture settings
/// or mouse mode change. A single task owns every write so two sessions
/// changing settings at once can't interleave partial writes.
pub async fn run(path: PathBuf, mut capture_rx: watch::Receiver<CaptureSettings>, mut mouse_mode_rx: watch::Receiver<MouseMode>) {
    loop {
        tokio::select! {
            result = capture_rx.changed() => {
                if result.is_err() { return; }
            }
            result = mouse_mode_rx.changed() => {
                if result.is_err() { return; }
            }
        }
        let settings = PersistedSettings { capture: *capture_rx.borrow(), mouse_mode: *mouse_mode_rx.borrow() };
        let path = path.clone();
        tokio::task::spawn_blocking(move || save(&path, settings));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::v4l2::Resolution;
    use crate::config::VideoMode;

    #[test]
    fn round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("simple_kvm_settings_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        let settings = PersistedSettings {
            capture: CaptureSettings { video_mode: VideoMode::H264, resolution: Resolution { width: 1280, height: 720 } },
            mouse_mode: MouseMode::Relative,
        };
        save(&path, settings);

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.capture.video_mode, VideoMode::H264);
        assert_eq!(loaded.capture.resolution, Resolution { width: 1280, height: 720 });
        assert_eq!(loaded.mouse_mode, MouseMode::Relative);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_returns_none() {
        let path = std::env::temp_dir().join("simple_kvm_settings_test_missing_definitely_not_here.json");
        assert!(load(&path).is_none());
    }

    #[test]
    fn garbage_file_returns_none_instead_of_failing() {
        let dir = std::env::temp_dir().join(format!("simple_kvm_settings_test_garbage_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, b"not json").unwrap();

        assert!(load(&path).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
