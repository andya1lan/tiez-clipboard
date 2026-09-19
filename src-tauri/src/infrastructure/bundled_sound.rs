//! macOS-style clipboard sounds (Tink/Boop, Frog/Jump) embedded as 16-bit WAV.
//!
//! Windows plays them with `PlaySoundW` (no rodio/cpal) to keep the binary
//! small; macOS registers them with System Sound Services (`macos_api::sound`).
//! Neither API takes a per-sound volume, so both apply the volume slider by
//! scaling the samples with [`scale_wav_volume`].

use std::io::Cursor;

static COPY_SOUND: &[u8] = include_bytes!("../../resources/sounds/Tink.wav");
static PASTE_SOUND: &[u8] = include_bytes!("../../resources/sounds/Frog.wav");

/// Embedded WAV for a sound kind; anything other than "paste" is the copy sound.
pub fn wav_bytes(kind: &str) -> &'static [u8] {
    match kind {
        "paste" => PASTE_SOUND,
        _ => COPY_SOUND,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn play_clipboard_sound(kind: &str, volume: f64) {
    let vol = volume.clamp(0.0, 1.0) as f32;
    if vol <= 0.0 {
        return;
    }

    let wav = wav_bytes(kind);

    #[cfg(target_os = "windows")]
    play_wav_on_windows(wav, vol);
    #[cfg(not(target_os = "windows"))]
    let _ = wav;
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

/// Copy of a 16-bit PCM WAV with every sample multiplied by `volume` (0..1).
pub fn scale_wav_volume(wav: &[u8], volume: f32) -> Result<Vec<u8>, hound::Error> {
    let mut reader = hound::WavReader::new(Cursor::new(wav))?;
    let spec = reader.spec();
    let mut out = Vec::new();
    {
        let mut writer = hound::WavWriter::new(Cursor::new(&mut out), spec)?;
        for sample in reader.samples::<i16>() {
            let s = sample?;
            let scaled = (f32::from(s) * volume).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            writer.write_sample(scaled)?;
        }
        writer.finalize()?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak(wav: &[u8]) -> i32 {
        let mut reader = hound::WavReader::new(Cursor::new(wav)).unwrap();
        reader
            .samples::<i16>()
            .map(|s| i32::from(s.unwrap()).abs())
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn embedded_sounds_are_16_bit_pcm() {
        for kind in ["copy", "paste"] {
            let spec = hound::WavReader::new(Cursor::new(wav_bytes(kind)))
                .unwrap()
                .spec();
            assert_eq!(spec.bits_per_sample, 16);
            assert_eq!(spec.sample_format, hound::SampleFormat::Int);
        }
    }

    #[test]
    fn scaling_halves_the_peak_and_keeps_the_format() {
        let original = hound::WavReader::new(Cursor::new(COPY_SOUND)).unwrap();
        let scaled = scale_wav_volume(COPY_SOUND, 0.5).unwrap();
        let reader = hound::WavReader::new(Cursor::new(scaled.as_slice())).unwrap();

        assert_eq!(reader.spec(), original.spec());
        assert_eq!(reader.len(), original.len());
        assert!((peak(&scaled) - peak(COPY_SOUND) / 2).abs() <= 1);
    }
}
