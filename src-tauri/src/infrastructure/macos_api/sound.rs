//! Audible UI feedback on macOS through System Sound Services (AudioToolbox),
//! the path macOS itself uses for interface sounds.
//!
//! Why not NSSound: every `play()` builds an AudioQueue and opens an output IO
//! stream inside our process. Measured on macOS 27.0 (26A428) with AirPods,
//! that took 50–870 ms from `play()` until the output device started — the
//! audible "cold start" lag — and the stream then lingered ~2.3 s after the
//! sound ended, a window in which TieZ sits on the audio route (#174). Sounds
//! handed to the system sound server instead start in ~16 ms, keep no audio IO
//! in our process, are released ~90 ms after they end, and play through the
//! user's "sound effects" output device like every other UI sound.
//!
//! The service has no per-sound volume, so the volume slider is applied by
//! registering a volume-scaled copy of the embedded WAV. The sound server goes
//! back to that file on later plays — a registered sound whose file is gone
//! "completes" instantly and silently — so the copies stay in the app cache
//! directory for as long as they are registered, and a play that finds its file
//! missing re-registers first.
//!
//! Every AudioToolbox call runs on one worker thread. The calls are cheap
//! (~0.3 ms) but round-trip to the sound server, and a server that is slow to
//! answer — say while a Bluetooth device wakes up — would stall whichever
//! thread asked for feedback. The preview and toggle commands run on the main
//! thread, so that thread must never be the caller. The worker also owns the
//! registry, which keeps "look up the ID" and "play it" in one place: no other
//! thread can dispose an ID between the two. Requests that waited longer than
//! [`STALE_AFTER`] are dropped; feedback that late is noise.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::infrastructure::bundled_sound::{scale_wav_volume, wav_bytes};

type CFAllocatorRef = *const c_void;
type CFIndex = isize;
type CFURLRef = *const c_void;
type Boolean = u8;
type OSStatus = i32;
type SystemSoundID = u32;
type AudioServicesPropertyID = u32;

/// 'isui' — 0 keeps the sound audible even when the user disabled
/// "Play user interface sound effects"; TieZ has its own toggle for that.
const K_AUDIO_SERVICES_PROPERTY_IS_UI_SOUND: AudioServicesPropertyID = u32::from_be_bytes(*b"isui");

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: CFAllocatorRef,
        buffer: *const u8,
        buf_len: CFIndex,
        is_directory: Boolean,
    ) -> CFURLRef;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioServicesCreateSystemSoundID(
        in_file_url: CFURLRef,
        out_system_sound_id: *mut SystemSoundID,
    ) -> OSStatus;
    fn AudioServicesDisposeSystemSoundID(in_system_sound_id: SystemSoundID) -> OSStatus;
    fn AudioServicesPlaySystemSound(in_system_sound_id: SystemSoundID);
    fn AudioServicesSetProperty(
        in_property_id: AudioServicesPropertyID,
        in_specifier_size: u32,
        in_specifier: *const c_void,
        in_property_data_size: u32,
        in_property_data: *const c_void,
    ) -> OSStatus;
}

const COPY_KIND: &str = "copy";
const PASTE_KIND: &str = "paste";

/// A play request that waited this long is dropped instead of played late.
const STALE_AFTER: Duration = Duration::from_millis(1500);

fn normalize_kind(kind: &str) -> &'static str {
    if kind == PASTE_KIND {
        PASTE_KIND
    } else {
        COPY_KIND
    }
}

/// System sound with the same content, for the afplay fallback.
/// (System Settings lists Tink as "Boop" and Frog as "Jump".)
fn system_sound_name(kind: &str) -> &'static str {
    if kind == PASTE_KIND {
        "Frog"
    } else {
        "Tink"
    }
}

/// Volume in whole percent — the granularity of the settings slider.
fn volume_key(volume: f32) -> u32 {
    (volume * 100.0).round() as u32
}

static SOUND_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Directory that holds the registered WAV copies. Set once at startup from the
/// app cache dir; until then (and in tests) a folder under the temp dir is used.
pub fn set_sound_dir(dir: PathBuf) {
    let _ = SOUND_DIR.set(dir);
}

fn sound_dir() -> PathBuf {
    SOUND_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::temp_dir().join("tiez-sounds"))
}

/// A sound registered with the sound server at one volume, plus the file the
/// server reads it from.
struct RegisteredSound {
    volume_key: u32,
    sound_id: SystemSoundID,
    path: PathBuf,
}

impl Drop for RegisteredSound {
    fn drop(&mut self) {
        unsafe {
            AudioServicesDisposeSystemSoundID(self.sound_id);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The two registered sounds; owned by the worker thread.
#[derive(Default)]
struct Registry {
    copy: Option<RegisteredSound>,
    paste: Option<RegisteredSound>,
}

impl Registry {
    /// Sound ID for `kind` at `volume`, re-registering when the volume changed
    /// or the file the server reads from is gone.
    fn sound_id_for(&mut self, kind: &'static str, volume: f32) -> Option<SystemSoundID> {
        let slot = if kind == PASTE_KIND {
            &mut self.paste
        } else {
            &mut self.copy
        };

        let usable = slot
            .as_ref()
            .is_some_and(|s| s.volume_key == volume_key(volume) && s.path.is_file());
        if !usable {
            *slot = None; // dispose the previous ID before creating the next one
            *slot = register(kind, volume);
        }
        slot.as_ref().map(|s| s.sound_id)
    }

    fn play(&mut self, kind: &'static str, volume: f32) {
        match self.sound_id_for(kind, volume) {
            Some(sound_id) => unsafe { AudioServicesPlaySystemSound(sound_id) },
            None => play_via_afplay(system_sound_name(kind), volume),
        }
    }
}

fn scaled_wav(kind: &str, volume: f32) -> Vec<u8> {
    let source = wav_bytes(kind);
    if volume_key(volume) >= 100 {
        return source.to_vec();
    }
    scale_wav_volume(source, volume).unwrap_or_else(|err| {
        eprintln!("[macos-sound] scaling {kind} to {volume:.2} failed ({err}); using full volume");
        source.to_vec()
    })
}

fn create_system_sound_id(path: &Path) -> Option<SystemSoundID> {
    use std::os::unix::ffi::OsStrExt;

    let raw = path.as_os_str().as_bytes();
    let url = unsafe {
        CFURLCreateFromFileSystemRepresentation(
            std::ptr::null(),
            raw.as_ptr(),
            raw.len() as CFIndex,
            0,
        )
    };
    if url.is_null() {
        eprintln!("[macos-sound] could not build a URL for {}", path.display());
        return None;
    }

    let mut sound_id: SystemSoundID = 0;
    let status = unsafe { AudioServicesCreateSystemSoundID(url, &mut sound_id) };
    unsafe { CFRelease(url) };
    if status != 0 {
        eprintln!("[macos-sound] AudioServicesCreateSystemSoundID failed with status {status}");
        return None;
    }

    let always_audible: u32 = 0;
    let status = unsafe {
        AudioServicesSetProperty(
            K_AUDIO_SERVICES_PROPERTY_IS_UI_SOUND,
            std::mem::size_of::<SystemSoundID>() as u32,
            (&sound_id as *const SystemSoundID).cast(),
            std::mem::size_of::<u32>() as u32,
            (&always_audible as *const u32).cast(),
        )
    };
    if status != 0 {
        // Not fatal: the sound still plays unless UI sounds are off system-wide.
        eprintln!("[macos-sound] could not clear the UI-sound flag (status {status})");
    }

    Some(sound_id)
}

/// Copies of `kind` at other volumes, or left behind by an earlier run, are
/// dead weight once a new one is written.
fn remove_other_copies(dir: &Path, kind: &str, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{kind}-");
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path != keep && name.starts_with(&prefix) && name.ends_with(".wav") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Write the scaled WAV into the sound dir and hand it to the sound server.
fn register(kind: &str, volume: f32) -> Option<RegisteredSound> {
    let key = volume_key(volume);
    let dir = sound_dir();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        eprintln!("[macos-sound] could not create {}: {err}", dir.display());
        return None;
    }

    let path = dir.join(format!("{kind}-{key}.wav"));
    remove_other_copies(&dir, kind, &path);
    if let Err(err) = std::fs::write(&path, scaled_wav(kind, volume)) {
        eprintln!("[macos-sound] could not write {}: {err}", path.display());
        return None;
    }

    create_system_sound_id(&path).map(|sound_id| RegisteredSound {
        volume_key: key,
        sound_id,
        path,
    })
}

/// Fallback for when the sound server refuses the embedded file.
fn play_via_afplay(sound_name: &str, volume: f32) {
    let path = format!("/System/Library/Sounds/{}.aiff", sound_name);
    let vol_arg = format!("{:.3}", volume);

    let _ = std::thread::spawn(move || {
        let _ = std::process::Command::new("/usr/bin/afplay")
            .arg("-v")
            .arg(vol_arg)
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

enum Request {
    Play {
        kind: &'static str,
        volume: f32,
        requested_at: Instant,
    },
    Preload {
        volume: f32,
    },
}

/// Handle to the worker thread, started on first use. `None` if the thread
/// could not be spawned; callers then fall back to afplay.
fn worker() -> Option<&'static Sender<Request>> {
    static WORKER: OnceLock<Option<Sender<Request>>> = OnceLock::new();

    WORKER
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Request>();
            let spawned = std::thread::Builder::new()
                .name("tiez-ui-sound".to_string())
                .spawn(move || {
                    let mut registry = Registry::default();
                    while let Ok(request) = rx.recv() {
                        match request {
                            Request::Preload { volume } => {
                                for kind in [COPY_KIND, PASTE_KIND] {
                                    let _ = registry.sound_id_for(kind, volume);
                                }
                            }
                            Request::Play {
                                kind,
                                volume,
                                requested_at,
                            } => {
                                if requested_at.elapsed() > STALE_AFTER {
                                    continue;
                                }
                                registry.play(kind, volume);
                            }
                        }
                    }
                });

            match spawned {
                Ok(_) => Some(tx),
                Err(err) => {
                    eprintln!(
                        "[macos-sound] could not spawn the sound thread ({err}); using afplay"
                    );
                    None
                }
            }
        })
        .as_ref()
}

pub fn play_clipboard_sound(kind: &str, volume: f64) {
    let vol = volume.clamp(0.0, 1.0) as f32;
    if vol <= 0.0 {
        return;
    }

    let kind = normalize_kind(kind);
    let request = Request::Play {
        kind,
        volume: vol,
        requested_at: Instant::now(),
    };
    let queued = worker().is_some_and(|tx| tx.send(request).is_ok());
    if !queued {
        play_via_afplay(system_sound_name(kind), vol);
    }
}

/// Register both sounds ahead of the first copy so it does not pay for the
/// scaling and file round-trip.
pub fn preload_clipboard_sounds(volume: f64) {
    let vol = volume.clamp(0.0, 1.0) as f32;
    if vol <= 0.0 {
        return;
    }
    if let Some(tx) = worker() {
        let _ = tx.send(Request::Preload { volume: vol });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The two tests share the sound dir, so each sticks to one kind: cleanup
    // only ever touches files of the same kind.

    #[test]
    fn registers_the_paste_sound_and_keeps_its_file_until_dropped() {
        for volume in [1.0f32, 0.4] {
            let registered = register(PASTE_KIND, volume).expect("sound server accepted the WAV");
            assert_eq!(registered.volume_key, volume_key(volume));
            assert_ne!(registered.sound_id, 0);
            assert!(
                registered.path.is_file(),
                "the server re-reads the file on later plays"
            );

            let path = registered.path.clone();
            drop(registered);
            assert!(!path.exists());
        }
    }

    #[test]
    fn re_registers_the_copy_sound_when_volume_changes_or_the_file_vanishes() {
        let mut registry = Registry::default();

        let first = registry
            .sound_id_for(COPY_KIND, 0.7)
            .expect("registered at 70%");
        assert_eq!(registry.sound_id_for(COPY_KIND, 0.7), Some(first));

        let second = registry
            .sound_id_for(COPY_KIND, 0.3)
            .expect("registered at 30%");
        assert_ne!(second, first);

        let path = registry
            .copy
            .as_ref()
            .map(|s| s.path.clone())
            .expect("copy sound registered");
        std::fs::remove_file(&path).unwrap();

        let third = registry
            .sound_id_for(COPY_KIND, 0.3)
            .expect("re-registered");
        assert_ne!(third, second);
        assert!(path.is_file());
    }
}
