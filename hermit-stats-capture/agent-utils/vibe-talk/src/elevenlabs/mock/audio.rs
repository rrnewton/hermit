//! The audio the mock accepts and the audio it produces.
//!
//! ElevenLabs carries PCM over the conversation socket as base64 inside JSON events, so this
//! module is where base64 meets 16-bit little-endian samples. Two rules are enforced here rather
//! than at the call sites:
//!
//! * **A chunk that is not decodable is reported, never silently accepted.** Bad base64 and an
//!   odd byte count are both errors: an odd count cannot be 16-bit samples, and a mock that
//!   shrugged at it would let a page ship half a sample forever without anything noticing.
//! * **Outgoing audio is deterministic.** [`tone`] is a fixed sawtooth with no clock and no
//!   randomness in it, because two screenshot runs have to produce the same bytes and a trace
//!   assertion has to be able to state an exact length.

use base64::Engine as _;

/// Samples per second in every format this mock negotiates as playable.
pub const SAMPLE_RATE: u32 = 16_000;

/// Bytes per sample: 16-bit signed, little-endian.
pub const BYTES_PER_SAMPLE: usize = 2;

/// Why an uploaded chunk could not be taken as PCM.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PcmError {
    /// The text was not base64.
    #[error("not base64: {0}")]
    NotBase64(String),
    /// The decoded length cannot be whole 16-bit samples.
    #[error("{0} bytes is an odd number, so it cannot be 16-bit PCM")]
    OddLength(usize),
}

impl PcmError {
    /// The short label this failure is recorded under in the trace.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotBase64(_) => "malformed_base64",
            Self::OddLength(_) => "malformed_pcm",
        }
    }
}

fn engine() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// Base64-encode raw bytes for a `user_audio_chunk` or an `audio` event.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    engine().encode(bytes)
}

/// Decode one base64 audio chunk into whole 16-bit samples.
///
/// # Errors
///
/// [`PcmError::NotBase64`] when the text is not base64 at all, and [`PcmError::OddLength`] when
/// it decodes to a byte count that cannot be 16-bit samples.
pub fn decode(text: &str) -> Result<Vec<u8>, PcmError> {
    let bytes = engine()
        .decode(text.trim())
        .map_err(|e| PcmError::NotBase64(e.to_string()))?;
    if bytes.len() % BYTES_PER_SAMPLE != 0 {
        return Err(PcmError::OddLength(bytes.len()));
    }
    Ok(bytes)
}

/// How many 16-bit samples a decoded chunk holds.
#[must_use]
pub fn sample_count(bytes: &[u8]) -> usize {
    bytes.len() / BYTES_PER_SAMPLE
}

/// A deterministic sawtooth of `samples` 16-bit little-endian samples.
///
/// Audible enough to prove a page is playing something, and generated from the sample index alone
/// so it is byte-identical on every run.
#[must_use]
pub fn tone(samples: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples * BYTES_PER_SAMPLE);
    for index in 0..samples {
        // A 200 Hz ramp at 16 kHz: 80 samples per cycle, amplitude a quarter of full scale.
        let phase = (index % 80) as i32;
        let value = i16::try_from((phase - 40) * 200).unwrap_or(i16::MAX);
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_the_rfc4648_vectors() {
        for (raw, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(raw.as_bytes()), encoded, "encoding {raw:?}");
        }
    }

    #[test]
    fn a_chunk_that_is_not_base64_is_reported_rather_than_guessed_at() {
        let error = decode("not base64 at all!!").expect_err("must refuse");
        assert_eq!(error.kind(), "malformed_base64");
    }

    #[test]
    fn an_odd_byte_count_cannot_be_sixteen_bit_pcm_and_says_so() {
        // Five bytes is two and a half samples. A page that shipped this is truncating, and the
        // only way anyone finds out is if the receiver refuses it.
        let error = decode(&encode(&[1, 2, 3, 4, 5])).expect_err("must refuse");
        assert_eq!(error, PcmError::OddLength(5));
        assert_eq!(error.kind(), "malformed_pcm");
        assert!(error.to_string().contains("odd"), "{error}");
    }

    #[test]
    fn an_even_chunk_decodes_to_whole_samples() {
        let bytes = decode(&encode(&tone(32))).expect("decodes");
        assert_eq!(bytes.len(), 64);
        assert_eq!(sample_count(&bytes), 32);
    }

    #[test]
    fn the_tone_is_the_same_bytes_every_time_it_is_asked_for() {
        // Determinism is the product: a screenshot run and a trace assertion both depend on it.
        assert_eq!(tone(160), tone(160));
        assert_eq!(tone(160).len(), 320);
        assert_ne!(tone(160), tone(80));
    }

    #[test]
    fn whitespace_in_a_transported_chunk_is_tolerated() {
        assert_eq!(decode(" Zm9vYmFy ").expect("decodes"), b"foobar");
    }
}
