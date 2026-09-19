//! macOS-style clipboard sounds (Tink/Boop, Frog/Jump) as embedded WAV on Windows.
//!
//! Uses `PlaySoundW` (no rodio/cpal) to keep the Windows binary small.

static COPY_SOUND: &[u8] = include_bytes!("../../resources/sounds/Tink.wav");
static PASTE_SOUND: &[u8] = include_bytes!("../../resources/sounds/Frog.wav");

#[cfg(target_os = "windows")]
use std::io::Cursor;

pub fn play_clipboard_sound(kind: &str, volume: f64) {
    let vol = volume.clamp(0.0, 1.0) as f32;
    if vol <= 0.0 {
        return;
    }

    let wav = match kind {
        "paste" => PASTE_SOUND,
        _ => COPY_SOUND,
    };

    #[cfg(target_os = "windows")]
    play_wav_on_windows(wav, vol);
}

#[cfg(target_os = "windows")]
fn play_wav_on_windows(wav: &'static [u8], volume: f32) {
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

    if volume >= 0.999 {
        // Full-volume path: play asynchronously on the caller thread (no thread spawn).
        unsafe {
            let _ = PlaySoundW(
                PCWSTR(wav.as_ptr() as *const u16),
                None,
                SND_MEMORY | SND_NODEFAULT | SND_ASYNC,
            );
        }
        return;
    }

    std::thread::spawn(move || {
        let buffer = scale_wav_volume(wav, volume).unwrap_or_else(|_| wav.to_vec());

        unsafe {
            // Deliberately synchronous (the Win32 default, SND_SYNC == 0): with
            // SND_MEMORY the audio system keeps reading from `buffer` for the whole
            // sound, so returning early would let this thread drop the buffer
            // mid-playback. Blocking costs nothing here — that is what the thread
            // is for. The full-volume path above can stay async because it hands
            // over a &'static slice.
            let _ = PlaySoundW(
                PCWSTR(buffer.as_ptr() as *const u16),
                None,
                SND_MEMORY | SND_NODEFAULT,
            );
        }
    });
}

#[cfg(target_os = "windows")]
fn scale_wav_volume(wav: &[u8], volume: f32) -> Result<Vec<u8>, hound::Error> {
    let mut reader = hound::WavReader::new(Cursor::new(wav))?;
    let spec = reader.spec();
    let mut out = Vec::new();
    {
        let mut writer = hound::WavWriter::new(Cursor::new(&mut out), spec)?;
        for sample in reader.samples::<i16>() {
            let s = sample?;
            let scaled = (f32::from(s) * volume)
                .clamp(i16::MIN as f32, i16::MAX as f32)
                as i16;
            writer.write_sample(scaled)?;
        }
        writer.finalize()?;
    }
    Ok(out)
}
