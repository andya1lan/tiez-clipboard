use crate::app_state::SettingsState;
use crate::database::DbState;
use crate::infrastructure::repository::settings_repo::SettingsRepository;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{AppHandle, Manager};

fn paste_sound_enabled(settings: &SettingsState, app_handle: &AppHandle) -> bool {
    if settings.paste_sound_enabled.load(Ordering::Relaxed) {
        return true;
    }
    // Memory can be stale if an older save_setting path ran; trust DB as fallback.
    app_handle
        .try_state::<DbState>()
        .and_then(|db| db.settings_repo.get("app.sound_paste_enabled").ok().flatten())
        .map(|v| v == "true")
        .unwrap_or(true)
}

/// Volume is a 0..1 fraction, but installs older than the 0..1 slider stored it
/// as a 0..100 percentage. Anything above 1 is one of those, so scale it back
/// down instead of letting the clamp turn every legacy value into full volume.
pub fn normalize_sound_volume(raw: f64) -> f64 {
    if !raw.is_finite() {
        return 1.0;
    }
    let fraction = if raw > 1.0 { raw / 100.0 } else { raw };
    fraction.clamp(0.0, 1.0)
}

pub fn play_ui_sound(app_handle: &AppHandle, kind: &str) {
    let settings = app_handle.state::<SettingsState>();
    if !settings.sound_enabled.load(Ordering::Relaxed) {
        return;
    }
    if kind == "paste" && !paste_sound_enabled(&settings, app_handle) {
        return;
    }

    let volume = settings
        .sound_volume
        .lock()
        .map(|v| *v)
        .unwrap_or(1.0);

    play_ui_sound_at_volume(kind, volume);
}

pub fn play_ui_sound_at_volume(kind: &str, volume: f64) {
    #[cfg(target_os = "macos")]
    {
        crate::infrastructure::macos_api::sound::play_clipboard_sound(kind, volume);
    }

    #[cfg(not(target_os = "macos"))]
    {
        crate::infrastructure::bundled_sound::play_clipboard_sound(kind, volume);
    }
}

/// Where the native playback path keeps its volume-scaled sound files.
pub fn set_sound_dir(dir: std::path::PathBuf) {
    #[cfg(target_os = "macos")]
    crate::infrastructure::macos_api::sound::set_sound_dir(dir);
    #[cfg(not(target_os = "macos"))]
    let _ = dir;
}

/// Register native sound assets ahead of the first copy when sound effects are on.
pub fn preload_ui_sounds(volume: f64) {
    #[cfg(target_os = "macos")]
    crate::infrastructure::macos_api::sound::preload_clipboard_sounds(volume);
    #[cfg(not(target_os = "macos"))]
    let _ = volume;
}

/// Play paste feedback after the synthetic Cmd+V (all native paste paths).
pub fn schedule_paste_sound(app_handle: &AppHandle) {
    let app_handle = app_handle.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        play_ui_sound(&app_handle, "paste");
    });
}
