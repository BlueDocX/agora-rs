//! Voice errands: `POST /api/transcribe` with a WAV recording as the body (mono,
//! 16-bit PCM, 16 or 24 kHz, at most [`MAX_SECONDS`]) answers `{"text", "ms"}`,
//! transcribed by Muse Voice Transcribe on the office's key. The browser records and
//! encodes the WAV; the key never leaves the server. The names in the office (Asha's
//! contacts, the people, their bots) go along as keywords, so they come back spelled
//! right. What the office may transcribe while it runs is capped
//! ([`MAX_TOTAL_SECONDS`]; audio costs $0.18 an hour).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::{Hub, authorized, error, unauthorized};

pub const MAX_SECONDS: u64 = 60;
/// A minute at 24 kHz, plus room for the header.
pub const MAX_BYTES: usize = 24_000 * 2 * 60 + 4096;
pub const MAX_TOTAL_SECONDS: u64 = 2 * 3600;

/// How long a WAV of the supported kind runs, in milliseconds, or why it isn't one.
fn wav_ms(wav: &[u8]) -> Result<u64, &'static str> {
    let bad = "send a WAV: mono, 16-bit PCM, 16 or 24 kHz";
    if wav.len() < 12 || &wav[..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return Err(bad);
    }
    let u16_at = |i: usize| wav.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |i: usize| wav.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let (mut at, mut rate, mut data) = (12, None, None);
    while let (Some(id), Some(size)) = (wav.get(at..at + 4), u32_at(at + 4)) {
        let body = at + 8;
        match id {
            b"fmt " => {
                let (format, channels, hz, bits) = (u16_at(body), u16_at(body + 2), u32_at(body + 4), u16_at(body + 14));
                if format != Some(1) || channels != Some(1) || bits != Some(16) || !matches!(hz, Some(16_000 | 24_000)) {
                    return Err(bad);
                }
                rate = hz;
            }
            b"data" => data = Some(size),
            _ => {}
        }
        // Chunks are padded to an even length.
        at = body + usize::try_from(size).map_err(|_| bad)? + usize::try_from(size % 2).unwrap_or(0);
    }
    match (rate, data) {
        (Some(rate), Some(size)) => Ok(u64::from(size) * 1000 / u64::from(rate * 2)),
        _ => Err(bad),
    }
}

/// Names worth biasing toward: the sandbox's contacts, the people and their bots.
fn keywords(hub: &Hub) -> Vec<String> {
    let mut words: Vec<String> = agora_core::World::demo().contacts.iter().map(|c| c.name.to_string()).collect();
    for m in hub.company().members_view() {
        words.push(m.name);
        words.push(m.bot);
    }
    words.extend(["Kestrel", "Agora"].map(String::from));
    words.sort();
    words.dedup();
    words
}

pub(crate) async fn transcribe(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let ms = match wav_ms(&body) {
        Ok(ms) if ms > 200 && ms <= MAX_SECONDS * 1000 => ms,
        Ok(_) => return error(StatusCode::BAD_REQUEST, &format!("speak for up to {MAX_SECONDS} seconds")),
        Err(why) => return error(StatusCode::BAD_REQUEST, why),
    };
    // Whole seconds, rounded up, against the office's allowance.
    let charge = ms.div_ceil(1000);
    let used = hub.transcribed_s.fetch_add(charge, Ordering::Relaxed);
    if used + charge > MAX_TOTAL_SECONDS {
        hub.transcribed_s.fetch_sub(charge, Ordering::Relaxed);
        return error(StatusCode::PAYMENT_REQUIRED, "the office's voice allowance is used up; type the errand instead");
    }
    match hub.config.muse.transcribe(body.to_vec(), &keywords(&hub)).await {
        Ok(text) => Json(serde_json::json!({ "text": text, "ms": ms })).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, &format!("couldn't transcribe that: {e}")),
    }
}

#[cfg(test)]
pub(crate) fn wav(rate: u32, ms: u32) -> Vec<u8> {
    let samples = rate * ms / 1000;
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + samples * 2).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(samples * 2).to_le_bytes());
    w.resize(w.len() + samples as usize * 2, 0);
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_short_mono_16_bit_wavs_at_16_or_24_khz_are_sent() {
        assert_eq!(wav_ms(&wav(24_000, 2000)), Ok(2000));
        assert_eq!(wav_ms(&wav(16_000, 500)), Ok(500));
        assert!(wav_ms(&wav(44_100, 1000)).is_err());
        let mut stereo = wav(24_000, 1000);
        stereo[22] = 2;
        assert!(wav_ms(&stereo).is_err());
        assert!(wav_ms(b"OggS....").is_err());
        assert!(wav_ms(&wav(24_000, 1000)[..30]).is_err(), "a cut-off header");
    }
}
