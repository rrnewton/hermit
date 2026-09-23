/* SPDX-License-Identifier: BSD-3-Clause */
//! Assertions for an omitted epoch captured once by the actual CLI invocation.

use detcore_model::config::Epoch;
use detcore_model::time::DetTime;
use serde_json::Value;

pub const CLOCK_PROBE: &str = r#"
import json, os, sys, time

def sample():
    first = time.clock_gettime_ns(time.CLOCK_REALTIME)
    mono = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
    last = time.clock_gettime_ns(time.CLOCK_REALTIME)
    rtc = {}
    if sys.argv[2] == 'rtc':
        for name in ['date', 'time', 'since_epoch']:
            before = time.clock_gettime_ns(time.CLOCK_REALTIME)
            with open('/sys/class/rtc/rtc0/' + name) as stream:
                value = stream.read()
            after = time.clock_gettime_ns(time.CLOCK_REALTIME)
            rtc[name] = [before, value, after]
    return {'clock': [first, mono, last], 'rtc': rtc}

if len(sys.argv) == 3:
    samples = {'before': sample()}
    time.sleep(2)
    samples['after_sleep'] = sample()
    os.execv(sys.executable, [sys.executable, '-c', sys.argv[1], sys.argv[1], sys.argv[2], json.dumps(samples)])
else:
    samples = json.loads(sys.argv[3])
    samples['after_exec'] = sample()
    print(json.dumps(samples, sort_keys=True))
"#;

pub fn captured_epoch(stderr: &[u8]) -> i64 {
    let text = std::str::from_utf8(stderr).expect("Hermit epoch report must be UTF-8");
    let reports = text
        .lines()
        .filter_map(|line| line.strip_prefix("hermit: virtual-time epoch="))
        .collect::<Vec<_>>();
    assert_eq!(reports.len(), 1, "expected one captured run epoch: {text}");
    let (epoch, rest) = reports[0].split_once(" source=").expect("epoch source");
    let (source, reproduce) = rest
        .split_once("; reproduce with --epoch=")
        .expect("epoch reproducer");
    assert_eq!(source, "host-now", "omitted epoch was not captured: {text}");
    assert_eq!(epoch, reproduce, "reproducer changed the captured epoch");
    epoch
        .parse::<Epoch>()
        .expect("reported epoch must be RFC3339")
        .timestamp_nanos_opt()
        .expect("captured epoch must fit nanoseconds")
}

pub fn initialized_guest_epoch(captured_nanos: i64) -> i64 {
    let epoch = Epoch::from_timestamp(
        captured_nanos.div_euclid(1_000_000_000),
        captured_nanos.rem_euclid(1_000_000_000) as u32,
    )
    .expect("captured epoch");
    // Use the runtime's public conversion, including its exact microsecond
    // initialization. Keep the full nanosecond capture/reproducer check above.
    i64::try_from(DetTime::from(&epoch).as_nanos().as_nanos())
        .expect("initialized guest epoch must fit nanoseconds")
}

pub fn assert_start(epoch: i64, first: i64) {
    let epoch = initialized_guest_epoch(epoch);
    assert!(first >= epoch, "guest clock preceded initialized epoch");
    // Keep the existing 60-second virtual-startup bound; this is relative to
    // the actual run epoch, never a tolerance around the test host wall clock.
    assert!(
        first - epoch < 60_000_000_000,
        "virtual startup exceeded 60s"
    );
}

pub fn assert_rtc_bracket(name: &str, value: &Value) {
    let values = value.as_array().expect("RTC bracket array");
    assert_eq!(values.len(), 3);
    let first = values[0].as_i64().expect("RTC before nanoseconds");
    let last = values[2].as_i64().expect("RTC after nanoseconds");
    assert!(last >= first, "RTC read moved CLOCK_REALTIME backwards");
    let value = values[1].as_str().expect("RTC text");
    let text = value
        .strip_suffix('\n')
        .expect("RTC attribute ends with newline");
    assert!(!text.contains('\n'));
    let first = first.div_euclid(1_000_000_000);
    let last = last.div_euclid(1_000_000_000);
    match name {
        "since_epoch" => {
            let seconds = text.parse::<i64>().expect("RTC seconds");
            assert_eq!(text, seconds.to_string());
            assert!(
                (first..=last).contains(&seconds),
                "RTC seconds outside guest bracket"
            );
        }
        "date" => {
            let parsed = format!("{text}T00:00:00Z")
                .parse::<Epoch>()
                .expect("RTC date");
            assert_eq!(text, parsed.format("%Y-%m-%d").to_string());
            let before = Epoch::from_timestamp(first, 0)
                .unwrap()
                .format("%Y-%m-%d")
                .to_string();
            let after = Epoch::from_timestamp(last, 0)
                .unwrap()
                .format("%Y-%m-%d")
                .to_string();
            assert!(
                before.as_str() <= text && text <= after.as_str(),
                "RTC date outside guest bracket"
            );
        }
        "time" => {
            let parsed = format!("2000-01-01T{text}Z")
                .parse::<Epoch>()
                .expect("RTC time");
            assert_eq!(text, parsed.format("%H:%M:%S").to_string());
            let within_day = parsed.timestamp().rem_euclid(86_400);
            let mut candidate = first.div_euclid(86_400) * 86_400 + within_day;
            if candidate < first {
                candidate += 86_400;
            }
            assert!(candidate <= last, "RTC time outside guest bracket");
        }
        _ => panic!("unknown RTC attribute {name}"),
    }
}

pub fn assert_clock_progress(stdout: &[u8], epoch: i64, rtc: bool) {
    let samples: Value = serde_json::from_slice(stdout).expect("clock probe JSON");
    assert_eq!(samples.as_object().unwrap().len(), 3);
    let mut previous: Option<[i64; 3]> = None;
    for name in ["before", "after_sleep", "after_exec"] {
        let sample = &samples[name];
        assert_eq!(sample.as_object().unwrap().len(), 2);
        let clock = sample["clock"].as_array().expect("clock triple");
        assert_eq!(clock.len(), 3);
        let now = [
            clock[0].as_i64().unwrap(),
            clock[1].as_i64().unwrap(),
            clock[2].as_i64().unwrap(),
        ];
        assert!(now[2] >= now[0], "{name}: CLOCK_REALTIME regressed");
        if let Some(previous) = previous {
            let minimum = if name == "after_sleep" {
                2_000_000_000
            } else {
                0
            };
            assert!(
                now[0] >= previous[2] + minimum,
                "{name}: realtime reset or lost sleep"
            );
            assert!(
                now[1] >= previous[1] + minimum,
                "{name}: monotonic reset or lost sleep"
            );
        } else {
            assert_start(epoch, now[0]);
        }
        let attributes = sample["rtc"].as_object().expect("RTC object");
        assert_eq!(attributes.len(), if rtc { 3 } else { 0 });
        if rtc {
            for attribute in ["date", "time", "since_epoch"] {
                assert_rtc_bracket(attribute, &sample["rtc"][attribute]);
            }
        }
        let mut last_realtime = now[2];
        if rtc {
            for attribute in ["date", "time", "since_epoch"] {
                let bracket = &sample["rtc"][attribute];
                assert!(
                    bracket[0].as_i64().unwrap() >= last_realtime,
                    "{name}: clock regressed between RTC reads"
                );
                last_realtime = bracket[2].as_i64().unwrap();
            }
        }
        previous = Some([now[0], now[1], last_realtime]);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn report_preserves_fractional_epoch_and_reproducer() {
        let text = b"hermit: virtual-time epoch=2026-01-01T00:00:00.123456789+00:00 source=host-now; reproduce with --epoch=2026-01-01T00:00:00.123456789+00:00\n";
        assert_eq!(captured_epoch(text), 1_767_225_600_123_456_789);
    }

    #[test]
    fn fractional_capture_uses_exact_runtime_initialization() {
        let captured = 1_767_225_600_123_456_789_i64;
        let initialized = 1_767_225_600_123_456_000_i64;
        assert_eq!(initialized_guest_epoch(captured), initialized);
        assert_start(captured, initialized);
    }

    #[test]
    #[should_panic(expected = "guest clock preceded initialized epoch")]
    fn even_one_nanosecond_before_initialized_epoch_is_rejected() {
        assert_start(1_767_225_600_123_456_789, 1_767_225_600_123_455_999);
    }

    #[test]
    #[should_panic(expected = "expected one captured run epoch")]
    fn missing_capture_is_not_satisfied_by_a_guest_clock() {
        captured_epoch(b"");
    }

    #[test]
    fn rtc_brackets_cross_midnight_without_host_clock_tolerance() {
        let first = 1_767_225_599_900_000_000_i64;
        let last = 1_767_225_600_100_000_000_i64;
        assert_rtc_bracket("date", &json!([first, "2026-01-01\n", last]));
        assert_rtc_bracket("time", &json!([first, "00:00:00\n", last]));
        assert_rtc_bracket("since_epoch", &json!([first, "1767225600\n", last]));
    }

    #[test]
    #[should_panic(expected = "RTC seconds outside guest bracket")]
    fn rtc_outside_the_actual_guest_bracket_is_rejected() {
        assert_rtc_bracket("since_epoch", &json!([1_000_000_000, "2\n", 1_999_999_999]));
    }

    #[test]
    #[should_panic(expected = "realtime reset or lost sleep")]
    fn reset_on_exec_is_not_accepted_as_continuity() {
        let samples = json!({
            "before": {"clock": [100, 200, 300], "rtc": {}},
            "after_sleep": {"clock": [2_000_000_400_i64, 2_000_000_500_i64, 2_000_000_600_i64], "rtc": {}},
            "after_exec": {"clock": [100, 200, 300], "rtc": {}}
        });
        assert_clock_progress(&serde_json::to_vec(&samples).unwrap(), 0, false);
    }

    #[test]
    #[should_panic(expected = "expected one captured run epoch")]
    fn a_second_capture_is_rejected() {
        let one = "hermit: virtual-time epoch=2026-01-01T00:00:00Z source=host-now; reproduce with --epoch=2026-01-01T00:00:00Z\n";
        captured_epoch(format!("{one}{one}").as_bytes());
    }

    #[test]
    #[should_panic(expected = "omitted epoch was not captured")]
    fn explicit_epoch_cannot_masquerade_as_the_default() {
        captured_epoch(b"hermit: virtual-time epoch=2026-01-01T00:00:00Z source=explicit; reproduce with --epoch=2026-01-01T00:00:00Z\n");
    }

    #[test]
    #[should_panic(expected = "reproducer changed the captured epoch")]
    fn changed_reproducer_is_rejected() {
        captured_epoch(b"hermit: virtual-time epoch=2026-01-01T00:00:00Z source=host-now; reproduce with --epoch=2025-01-01T00:00:00Z\n");
    }

    #[test]
    fn positive_sleep_and_exec_continuity_uses_the_captured_epoch() {
        let samples = json!({
            "before": {"clock": [100, 200, 300], "rtc": {}},
            "after_sleep": {"clock": [2_000_000_400_i64, 2_000_000_500_i64, 2_000_000_600_i64], "rtc": {}},
            "after_exec": {"clock": [2_000_000_700_i64, 2_000_000_800_i64, 2_000_000_900_i64], "rtc": {}}
        });
        assert_clock_progress(&serde_json::to_vec(&samples).unwrap(), 0, false);
    }
}
