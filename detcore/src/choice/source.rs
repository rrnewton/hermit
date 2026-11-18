/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The choice sources: a byte tape and a controller socket, and the request's
//! wire form. Each owns its own I/O; neither knows how a run uses its answers.

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;

use super::ChoiceKind;
use super::ChoiceRefusal;
use super::ChoiceRequest;
use super::ChoiceSource;
use super::SCHEMA;

/// Decodes answers from a byte tape, one byte per choice, front to back, in the
/// manner of libFuzzer's FuzzedDataProvider. Byte 0, or an exhausted tape,
/// takes the default, so the empty tape is the seeded run, and a mutation of
/// byte `j` changes about choice `j` only.
pub struct TapeSource {
    bytes: Vec<u8>,
    pos: usize,
}

impl TapeSource {
    /// A source that reads `bytes` front to back.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, pos: 0 }
    }

    /// The value byte `b` (nonzero) selects for `req`.
    fn decode(req: &ChoiceRequest, b: u8) -> u64 {
        let v = match req.kind {
            // Scale relative and logarithmic: b = 128 is the mean slice, each
            // 16 steps doubles or halves it (mean/256 .. ~mean*256).
            ChoiceKind::SchedSlice => tape_slice(req.scale.max(1), b),
            // 255 evenly spaced levels over the priority range.
            ChoiceKind::SchedPriority => {
                let span = req.hi - req.lo;
                req.lo + span * u64::from(b - 1) / 254
            }
        };
        v.clamp(req.lo, req.hi)
    }
}

/// `2^(j/16)` for `j` in `0..16`, as fixed-point numbers with 60 fractional
/// bits, each rounded to nearest (computed at 100 decimal digits). Entry 0 is
/// exactly 1 and entry 8 is the square root of 2.
const SIXTEENTH_POWERS_Q60: [u64; 16] = [
    0x1000_0000_0000_0000,
    0x10b5_586c_f989_0f63,
    0x1172_b83c_7d51_7add,
    0x1238_7a6e_7562_3867,
    0x1306_fe0a_31b7_152e,
    0x13de_a64c_1234_2236,
    0x14bf_dad5_362a_271d,
    0x15ab_07dd_4854_2959,
    0x16a0_9e66_7f3b_cc91,
    0x17a1_1473_eb01_86d8,
    0x18ac_e542_2aa0_db5c,
    0x19c4_9182_a3f0_901c,
    0x1ae8_9f99_5ad3_ad5f,
    0x1c19_9bdd_8552_9c22,
    0x1d58_18dc_fba4_8726,
    0x1ea4_afa2_a490_d986,
];

/// The slice a nonzero tape byte `b` selects around `scale`:
/// `scale * 2^((b-128)/16)`, rounded half up. Integer arithmetic only, so a
/// tape decodes to the same slice on every host and Rust version (the reason
/// `scheduler::timed_wait_spread` avoids `f64::powf`, whose precision Rust
/// leaves unspecified). The only approximation is the table's 2^-61 relative
/// error, which changes no result for scales below 2^40 except within that
/// distance of a half.
fn tape_slice(scale: u64, b: u8) -> u64 {
    let k = i32::from(b) - 128;
    let octaves = k.div_euclid(16);
    let sixteenths = k.rem_euclid(16) as usize;
    // 2^(k/16) * 2^60 * 2^-shift = 2^(k/16) when shift = 60 - octaves, which
    // lies in 53..=68 for k in -128..=127.
    let shift = (60 - octaves) as u32;
    let product = u128::from(scale) * u128::from(SIXTEENTH_POWERS_Q60[sixteenths]);
    let rounded = (product + (1u128 << (shift - 1))) >> shift;
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

impl ChoiceSource for TapeSource {
    fn answer(&mut self, req: &ChoiceRequest) -> Result<u64, ChoiceRefusal> {
        let b = self.bytes.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        Ok(if b == 0 {
            req.default
        } else {
            Self::decode(req, b)
        })
    }
}

/// Asks a controller process over a Unix stream socket, one JSON line per
/// request and one per answer: `{"i":N,"v":V}` or `{"i":N,"default":true}`.
pub struct SocketSource {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl SocketSource {
    /// Connect to the controller listening at `path`.
    pub fn connect(path: &Path) -> std::io::Result<Self> {
        let writer = UnixStream::connect(path)?;
        let reader = BufReader::new(writer.try_clone()?);
        Ok(Self { writer, reader })
    }
}

impl ChoiceSource for SocketSource {
    fn answer(&mut self, req: &ChoiceRequest) -> Result<u64, ChoiceRefusal> {
        let refuse = |reason: String| ChoiceRefusal {
            index: req.index,
            reason,
        };
        let mut line = request_json(req);
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .map_err(|e| refuse(format!("controller write failed: {e}")))?;
        let mut reply = String::new();
        let n = self
            .reader
            .read_line(&mut reply)
            .map_err(|e| refuse(format!("controller read failed: {e}")))?;
        if n == 0 {
            return Err(refuse("controller closed the socket".into()));
        }
        let parsed: serde_json::Value = serde_json::from_str(reply.trim())
            .map_err(|e| refuse(format!("controller reply is not JSON ({e}): {reply:?}")))?;
        if parsed.get("i").and_then(|i| i.as_u64()) != Some(req.index) {
            return Err(refuse(format!(
                "controller answered another index: {reply:?}"
            )));
        }
        // A key outside this protocol revision, such as a "capture" command,
        // is refused by name rather than ignored: this build captures nothing.
        if let Some(key) = parsed.as_object().and_then(|reply| {
            reply
                .keys()
                .find(|key| !["i", "v", "default", "refuse"].contains(&key.as_str()))
        }) {
            return Err(refuse(format!(
                "controller reply carries {key:?}, which this protocol revision does not \
                 support: {reply:?}"
            )));
        }
        // Exactly one of "v", "default" or "refuse", each of its documented
        // type. A reply naming two, or one of another type, is ambiguous and
        // is refused rather than read one way.
        let named: Vec<&str> = ["v", "default", "refuse"]
            .into_iter()
            .filter(|key| parsed.get(*key).is_some())
            .collect();
        if named.len() != 1 {
            return Err(refuse(format!(
                "controller reply must carry exactly one of \"v\", \"default\" or \"refuse\": \
                 {reply:?}"
            )));
        }
        if named[0] == "refuse" {
            return match parsed["refuse"].as_str() {
                Some(reason) => Err(refuse(format!("controller refused: {reason}"))),
                None => Err(refuse(format!(
                    "controller reply's \"refuse\" is not a string: {reply:?}"
                ))),
            };
        }
        if named[0] == "default" {
            return if parsed["default"] == serde_json::Value::Bool(true) {
                Ok(req.default)
            } else {
                Err(refuse(format!(
                    "controller reply's \"default\" is not true: {reply:?}"
                )))
            };
        }
        let v = parsed["v"].as_u64().ok_or_else(|| {
            refuse(format!(
                "controller reply's \"v\" is not an unsigned integer: {reply:?}"
            ))
        })?;
        if v < req.lo || v > req.hi {
            return Err(refuse(format!(
                "controller answer {v} is outside [{}, {}]",
                req.lo, req.hi
            )));
        }
        Ok(v)
    }
}

/// The request as one JSON object (no trailing newline).
pub fn request_json(req: &ChoiceRequest) -> String {
    format!(
        "{{\"schema\":{SCHEMA},\"i\":{},\"kind\":\"{}\",\"lo\":{},\"hi\":{},\"default\":{},\"scale\":{},\"ctx\":{{\"dtid\":{},\"thread_rcbs\":{},\"site\":\"{}\"}}}}",
        req.index,
        req.kind.name(),
        req.lo,
        req.hi,
        req.default,
        req.scale,
        req.dtid,
        req.thread_rcbs,
        req.site.name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::choice::tests::req;

    #[test]
    fn empty_and_zero_bytes_take_the_default() {
        let mut empty = TapeSource::new(vec![]);
        assert_eq!(empty.answer(&req(ChoiceKind::SchedSlice, 77)).unwrap(), 77);
        let mut zero = TapeSource::new(vec![0, 0]);
        assert_eq!(zero.answer(&req(ChoiceKind::SchedPriority, 5)).unwrap(), 5);
        assert_eq!(zero.answer(&req(ChoiceKind::SchedSlice, 9)).unwrap(), 9);
    }

    #[test]
    fn slice_bytes_are_scale_relative_and_monotone() {
        let r = req(ChoiceKind::SchedSlice, 1);
        assert_eq!(TapeSource::decode(&r, 128), 1000);
        assert_eq!(TapeSource::decode(&r, 144), 2000);
        assert_eq!(TapeSource::decode(&r, 112), 500);
        let mut prev = 0;
        for b in 1..=255u8 {
            let v = TapeSource::decode(&r, b);
            assert!(v >= prev && (r.lo..=r.hi).contains(&v));
            prev = v;
        }
    }

    /// The slice decode is exact fixed point: the documented value rounded
    /// half up, the same on every host. 1000/16 = 62.5 rounds up. The two
    /// large scales are values a float decode (`f64::powf`, then `round`)
    /// gets one too high on glibc 2.34, measured: 6722268617794.4996 and
    /// 2226918284313.4999972 round down.
    #[test]
    fn slice_decode_is_exact_fixed_point() {
        assert_eq!(SIXTEENTH_POWERS_Q60[0], 1 << 60);
        for pair in SIXTEENTH_POWERS_Q60.windows(2) {
            assert!(pair[0] < pair[1]);
        }
        for (scale, b, slice) in [
            (1000, 128, 1000),
            (1000, 144, 2000),
            (1000, 112, 500),
            (1000, 136, 1414),
            (1000, 64, 63),
            (1000, 1, 4),
            (1000, 255, 245_146),
            (200_000_000, 116, 118_920_712),
            (200_000_000, 179, 1_822_061_816),
            (1, 255, 245),
            (35_561_209_233, 249, 6_722_268_617_794),
            (43_211_185_441, 219, 2_226_918_284_313),
            (u64::MAX, 255, u64::MAX),
        ] {
            assert_eq!(tape_slice(scale, b), slice, "scale {scale}, byte {b}");
        }
    }

    #[test]
    fn priority_bytes_span_the_range() {
        let r = req(ChoiceKind::SchedPriority, 1);
        assert_eq!(TapeSource::decode(&r, 1), r.lo);
        assert_eq!(TapeSource::decode(&r, 255), r.hi);
    }

    #[test]
    fn request_json_is_one_object() {
        let line = request_json(&req(ChoiceKind::SchedSlice, 7));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["schema"], SCHEMA);
        assert_eq!(v["kind"], "sched.slice");
        assert_eq!(v["ctx"]["dtid"], 3);
    }
}
