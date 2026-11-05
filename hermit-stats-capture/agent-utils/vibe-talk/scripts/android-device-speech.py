#!/usr/bin/env python3
"""Capture vibe-talk device speech on Android and judge what the phone actually emitted.

This is an opt-in physical-device regression, not a browser mock.  It drives the served /voice
page through Android Chrome's DevTools socket, records Android playback, converts that recording
to mono PCM, and requires both an audible-signal check and an STT language/text check to pass.

The write token is read only from VIBE_TALK_WRITE_TOKEN.  The transcriber is an adapter executable
kept outside this repository; it receives the WAV path as its sole argument and prints one JSON
object to stdout:

    {"text": "recognised words", "language": "en", "confidence": 0.93}

An adapter is deliberate: a local Whisper binary, an organization's speech service, and another
offline recognizer can all close the loop without putting a vendor credential or a private
deployment detail in this reusable repository.

Typical use:

    VIBE_TALK_WRITE_TOKEN=... scripts/android-device-speech.py \
      --url https://vibe-talk.example.invalid/voice \
      --serial DEVICE --transcriber /path/to/stt-adapter

By default Android's ``screenrecord`` captures playback.  Devices that do not expose playback
capture can use ``--capture-adapter``; that executable receives DURATION_SECONDS and OUTPUT_PATH,
starts recording immediately, and exits after writing media that ffmpeg can decode.  A microphone
pointed at the device speaker is a valid adapter and tests more of the physical path.

Artifacts contain channel speech and stay below vibe-talk/debug/, which is gitignored.
``--self-test`` checks the evaluator and needs no device, browser, or speech service.
"""

from __future__ import annotations

import argparse
import array
import json
import math
import os
import re
import subprocess
import sys
import tempfile
import time
import wave
from collections import Counter
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import TYPE_CHECKING, Sequence, cast
from urllib.parse import urlparse

if TYPE_CHECKING:
    from playwright.sync_api import Browser, Locator, Page, Playwright


EXIT_OK = 0
EXIT_USAGE = 2
EXIT_DEVICE = 30
EXIT_BROWSER = 31
EXIT_CAPTURE = 32
EXIT_SIGNAL = 33
EXIT_TRANSCRIPT = 34
EXIT_SELF_TEST = 35

MIN_ACTIVE_SECONDS = 0.30
MIN_RMS_DBFS = -48.0
MIN_PEAK_DBFS = -38.0
MAX_CLIPPED_FRACTION = 0.08
DEFAULT_WORD_PRECISION = 0.55


class HarnessError(Exception):
    """A named harness failure with a stable process exit code."""

    def __init__(self, message: str, exit_code: int) -> None:
        super().__init__(message)
        self.exit_code = exit_code


@dataclass(frozen=True)
class SignalMetrics:
    duration_seconds: float
    active_seconds: float
    rms_dbfs: float
    peak_dbfs: float
    clipped_fraction: float


@dataclass(frozen=True)
class Transcript:
    text: str
    language: str
    confidence: float | None


@dataclass(frozen=True)
class SpeechTarget:
    text: str
    browser_language: str
    voice_language: str
    voice_name: str


def run(
    argv: Sequence[str],
    *,
    check: bool = True,
    capture_output: bool = True,
    timeout: float | None = None,
    failure_code: int = EXIT_CAPTURE,
) -> subprocess.CompletedProcess[str]:
    """Run a command without a shell, so URLs and paths never become shell syntax."""
    try:
        return subprocess.run(
            list(argv),
            check=check,
            capture_output=capture_output,
            text=True,
            timeout=timeout,
        )
    except FileNotFoundError as error:
        raise HarnessError(f"required executable is missing: {argv[0]}", EXIT_USAGE) from error
    except subprocess.TimeoutExpired as error:
        raise HarnessError(f"command timed out: {argv[0]}", failure_code) from error
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.stdout or "").strip().splitlines()
        suffix = f": {detail[-1]}" if detail else ""
        raise HarnessError(
            f"{argv[0]} failed with exit {error.returncode}{suffix}", failure_code
        ) from error


def adb_prefix(serial: str) -> list[str]:
    return ["adb", "-s", serial]


def select_device(requested: str | None) -> str:
    result = run(["adb", "devices"], failure_code=EXIT_DEVICE)
    devices: list[str] = []
    unauthorized: list[str] = []
    for line in result.stdout.splitlines()[1:]:
        fields = line.split()
        if len(fields) < 2:
            continue
        if fields[1] == "device":
            devices.append(fields[0])
        elif fields[1] == "unauthorized":
            unauthorized.append(fields[0])
    if requested:
        if requested not in devices:
            reason = "unauthorized" if requested in unauthorized else "not connected"
            raise HarnessError(f"requested Android device is {reason}", EXIT_DEVICE)
        return requested
    if len(devices) != 1:
        raise HarnessError(
            f"expected exactly one authorized Android device, found {len(devices)}; pass --serial",
            EXIT_DEVICE,
        )
    return devices[0]


def playback_source(help_text: str) -> str | None:
    """Name the internal-playback source only when screenrecord documents one."""
    lowered = help_text.lower()
    if "--audio-source" not in lowered:
        return None
    if "playback" in lowered:
        return "playback"
    if "internal" in lowered:
        return "internal"
    return None


class Capture:
    """A running audio capture, finished exactly once."""

    def __init__(
        self,
        process: subprocess.Popen[str],
        local_path: Path,
        *,
        serial: str | None = None,
        remote_path: str | None = None,
    ) -> None:
        self.process = process
        self.local_path = local_path
        self.serial = serial
        self.remote_path = remote_path

    def abort(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self._remove_remote()

    def finish(self, timeout: float) -> Path:
        try:
            stdout, stderr = self.process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            self.abort()
            raise HarnessError("audio capture did not stop at its duration limit", EXIT_CAPTURE) from error
        if self.process.returncode != 0:
            detail = (stderr or stdout or "").strip().splitlines()
            suffix = f": {detail[-1]}" if detail else ""
            self._remove_remote()
            raise HarnessError(
                f"audio capture failed with exit {self.process.returncode}{suffix}", EXIT_CAPTURE
            )
        if self.serial and self.remote_path:
            try:
                run([*adb_prefix(self.serial), "pull", self.remote_path, str(self.local_path)])
            finally:
                self._remove_remote()
        if not self.local_path.is_file() or self.local_path.stat().st_size == 0:
            raise HarnessError("audio capture produced no media file", EXIT_CAPTURE)
        return self.local_path

    def _remove_remote(self) -> None:
        if self.serial and self.remote_path:
            run(
                [*adb_prefix(self.serial), "shell", "rm", "-f", self.remote_path],
                check=False,
            )
            self.remote_path = None


def start_capture(
    serial: str,
    duration: int,
    output: Path,
    adapter: str | None,
) -> Capture:
    if adapter:
        try:
            process = subprocess.Popen(
                [adapter, str(duration), str(output)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
        except FileNotFoundError as error:
            raise HarnessError(f"capture adapter is missing: {adapter}", EXIT_USAGE) from error
        time.sleep(0.5)
        if process.poll() is not None:
            stdout, stderr = process.communicate()
            detail = (stderr or stdout or "").strip().splitlines()
            suffix = f": {detail[-1]}" if detail else ""
            raise HarnessError(f"capture adapter exited before speech began{suffix}", EXIT_CAPTURE)
        return Capture(process, output)

    help_result = run(
        [*adb_prefix(serial), "shell", "screenrecord", "--help"],
        check=False,
    )
    source = playback_source(f"{help_result.stdout}\n{help_result.stderr}")
    if source is None:
        raise HarnessError(
            "this device's screenrecord does not document internal playback capture; "
            "supply --capture-adapter",
            EXIT_CAPTURE,
        )
    remote = f"/sdcard/Download/vibe-talk-device-speech-{os.getpid()}.mp4"
    command = [
        *adb_prefix(serial),
        "shell",
        "screenrecord",
        "--audio",
        "--audio-source",
        source,
        "--time-limit",
        str(duration),
        remote,
    ]
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    time.sleep(1.0)
    if process.poll() is not None:
        stdout, stderr = process.communicate()
        detail = (stderr or stdout or "").strip().splitlines()
        suffix = f": {detail[-1]}" if detail else ""
        raise HarnessError(f"Android playback capture could not start{suffix}", EXIT_CAPTURE)
    return Capture(process, output, serial=serial, remote_path=remote)


def convert_to_wav(capture: Path, wav_path: Path) -> None:
    run(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            str(capture),
            "-map",
            "0:a:0",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "pcm_s16le",
            str(wav_path),
        ]
    )
    if not wav_path.is_file() or wav_path.stat().st_size <= 44:
        raise HarnessError("captured media contains no decodable audio", EXIT_CAPTURE)


def dbfs(value: float) -> float:
    return -120.0 if value <= 0 else 20.0 * math.log10(value / 32768.0)


def measure_signal(path: Path) -> SignalMetrics:
    try:
        with wave.open(str(path), "rb") as source:
            channels = source.getnchannels()
            width = source.getsampwidth()
            rate = source.getframerate()
            frames = source.getnframes()
            raw = source.readframes(frames)
    except (wave.Error, OSError) as error:
        raise HarnessError(f"could not read captured PCM: {error}", EXIT_CAPTURE) from error
    if channels != 1 or width != 2 or rate != 16000:
        raise HarnessError(
            f"expected mono 16-bit 16000 Hz PCM, got {channels} channel(s), {width * 8}-bit, {rate} Hz",
            EXIT_CAPTURE,
        )
    samples = array.array("h")
    samples.frombytes(raw)
    if sys.byteorder != "little":
        samples.byteswap()
    if not samples:
        return SignalMetrics(0.0, 0.0, -120.0, -120.0, 0.0)
    squares = sum(sample * sample for sample in samples)
    rms = math.sqrt(squares / len(samples))
    peak = max(abs(sample) for sample in samples)
    clipped = sum(1 for sample in samples if abs(sample) >= 32700) / len(samples)
    window = max(1, rate // 50)
    active = 0
    for start in range(0, len(samples), window):
        part = samples[start : start + window]
        if not part:
            continue
        part_rms = math.sqrt(sum(sample * sample for sample in part) / len(part))
        if dbfs(part_rms) >= -42.0:
            active += len(part)
    return SignalMetrics(
        duration_seconds=len(samples) / rate,
        active_seconds=active / rate,
        rms_dbfs=dbfs(rms),
        peak_dbfs=dbfs(float(peak)),
        clipped_fraction=clipped,
    )


def signal_problems(metrics: SignalMetrics) -> list[str]:
    problems: list[str] = []
    if metrics.active_seconds < MIN_ACTIVE_SECONDS:
        problems.append(
            f"only {metrics.active_seconds:.2f}s was above the speech floor"
        )
    if metrics.rms_dbfs < MIN_RMS_DBFS:
        problems.append(f"RMS level {metrics.rms_dbfs:.1f} dBFS is effectively silent")
    if metrics.peak_dbfs < MIN_PEAK_DBFS:
        problems.append(f"peak level {metrics.peak_dbfs:.1f} dBFS is too quiet")
    if metrics.clipped_fraction > MAX_CLIPPED_FRACTION:
        problems.append(
            f"{metrics.clipped_fraction:.1%} of samples are clipped, consistent with corrupt audio"
        )
    return problems


def parse_transcript(raw: str) -> Transcript:
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as error:
        raise HarnessError("transcriber stdout is not one JSON object", EXIT_TRANSCRIPT) from error
    if not isinstance(value, dict):
        raise HarnessError("transcriber stdout must be a JSON object", EXIT_TRANSCRIPT)
    text = value.get("text")
    language = value.get("language")
    confidence = value.get("confidence")
    if not isinstance(text, str) or not text.strip():
        raise HarnessError("transcriber returned no recognised text", EXIT_TRANSCRIPT)
    if not isinstance(language, str) or not language.strip():
        raise HarnessError("transcriber returned no detected language", EXIT_TRANSCRIPT)
    if confidence is not None and not isinstance(confidence, (int, float)):
        raise HarnessError("transcriber confidence must be numeric when present", EXIT_TRANSCRIPT)
    return Transcript(text.strip(), language.strip(), float(confidence) if confidence is not None else None)


def primary_language(tag: str) -> str:
    return re.split(r"[-_]", tag.strip().lower(), maxsplit=1)[0]


def words(text: str) -> list[str]:
    return re.findall(r"[^\W_]+", text.casefold(), flags=re.UNICODE)


def word_precision(expected: str, heard: str) -> tuple[float, int]:
    expected_counts = Counter(words(expected))
    heard_words = words(heard)
    if not heard_words:
        return 0.0, 0
    overlap = sum((Counter(heard_words) & expected_counts).values())
    return overlap / len(heard_words), overlap


def run_transcriber(executable: str, wav_path: Path) -> Transcript:
    result = run([executable, str(wav_path)], timeout=180, failure_code=EXIT_TRANSCRIPT)
    return parse_transcript(result.stdout)


def transcript_problems(
    transcript: Transcript,
    expected_text: str,
    expected_language: str,
    minimum_precision: float,
) -> tuple[list[str], float, int]:
    problems: list[str] = []
    actual_language = primary_language(transcript.language)
    wanted_language = primary_language(expected_language)
    if actual_language != wanted_language:
        problems.append(
            f"detected language {actual_language!r}, expected {wanted_language!r}"
        )
    precision, overlap = word_precision(expected_text, transcript.text)
    if overlap < 3:
        problems.append(f"only {overlap} recognised words occur in the selected message")
    if precision < minimum_precision:
        problems.append(
            f"recognised-word precision {precision:.1%} is below {minimum_precision:.1%}"
        )
    return problems, precision, overlap


def ensure_playwright() -> None:
    try:
        from playwright.sync_api import sync_playwright  # noqa: F401
    except ImportError as error:
        raise HarnessError(
            "Python Playwright is missing; install it with: python3 -m pip install --user playwright",
            EXIT_BROWSER,
        ) from error


def connect_android_chrome(playwright: Playwright, serial: str) -> tuple[Browser, int]:
    forwarded = run(
        [*adb_prefix(serial), "forward", "tcp:0", "localabstract:chrome_devtools_remote"],
        failure_code=EXIT_BROWSER,
    ).stdout.strip()
    if not forwarded.isdigit():
        raise HarnessError("adb did not allocate a Chrome DevTools forwarding port", EXIT_BROWSER)
    port = int(forwarded)
    try:
        browser = playwright.chromium.connect_over_cdp(f"http://127.0.0.1:{port}")
    except Exception as error:
        run([*adb_prefix(serial), "forward", "--remove", f"tcp:{port}"], check=False)
        raise HarnessError(
            "Android Chrome did not expose its DevTools socket; open Chrome and enable USB debugging",
            EXIT_BROWSER,
        ) from error
    return browser, port


def find_page(browser: Browser, wanted_url: str, timeout_seconds: float = 20.0) -> Page:
    wanted = urlparse(wanted_url)
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        for context in browser.contexts:
            for page in context.pages:
                current = urlparse(page.url)
                if (
                    current.scheme == wanted.scheme
                    and current.netloc == wanted.netloc
                    and current.path.rstrip("/") == wanted.path.rstrip("/")
                ):
                    return page
        time.sleep(0.2)
    raise HarnessError("the requested vibe-talk page did not open in Android Chrome", EXIT_BROWSER)


def open_target_page(
    page: Page,
    token: str,
    channel: str | None,
    message_id: str | None,
    message_index: int,
) -> tuple[Locator, SpeechTarget]:
    page.evaluate("token => localStorage.setItem('vibe-talk.token', token)", token)
    page.reload(wait_until="domcontentloaded")
    try:
        page.locator("#screen-main").wait_for(state="visible", timeout=20_000)
    except Exception as error:
        raise HarnessError(
            "the token did not reach the main screen; verify the deployment and write token",
            EXIT_BROWSER,
        ) from error
    if not page.locator("#pane-discord").is_visible():
        page.locator("#view-switch").click()
    if channel:
        page.locator("#discord-channel").select_option(channel)
    rows = page.locator("#discord-log .discord-message")
    try:
        rows.first.wait_for(state="visible", timeout=20_000)
    except Exception as error:
        raise HarnessError("the selected channel exposed no readable message row", EXIT_BROWSER) from error
    if message_id:
        matching = page.locator(
            f'#discord-log .discord-message[data-ids~="{message_id}"]'
        )
        if matching.count() != 1:
            raise HarnessError("--message-id did not select exactly one loaded row", EXIT_BROWSER)
        row = matching.first
    else:
        if message_index < 0 or message_index >= rows.count():
            raise HarnessError("--message-index is outside the loaded rows", EXIT_BROWSER)
        row = rows.nth(message_index)

    source = page.locator("#audio-source")
    if source.is_visible() and source.get_attribute("aria-checked") == "true":
        if not page.locator("#audio-agent-icon").is_visible():
            raise HarnessError("agent audio is selected but its cloud icon is not visible", EXIT_BROWSER)
        if page.locator("#audio-device-icon").is_visible():
            raise HarnessError("the device icon is visible while agent audio is selected", EXIT_BROWSER)
        source.click()
    if source.is_visible():
        if not page.locator("#audio-device-icon").is_visible():
            raise HarnessError("device speech is selected but its device icon is not visible", EXIT_BROWSER)
        if page.locator("#audio-agent-icon").is_visible():
            raise HarnessError("the cloud agent icon is visible while device speech is selected", EXIT_BROWSER)

    details = cast(
        dict[str, str | None],
        page.evaluate(
            """() => {
              const voice = typeof browserSpeechVoice === 'function' ? browserSpeechVoice() : null;
              return {
                browserLanguage: String(navigator.language || ''),
                voiceLanguage: voice ? String(voice.lang || '') : null,
                voiceName: voice ? String(voice.name || '') : null,
              };
            }"""
        ),
    )
    if not details.get("voiceLanguage"):
        raise HarnessError(
            "vibe-talk found no installed local voice compatible with the browser language",
            EXIT_BROWSER,
        )
    text = cast(
        str,
        row.evaluate(ROW_TEXT_SCRIPT),
    ).strip()
    if len(words(text)) < 3:
        raise HarnessError("the selected row is too short for a language regression", EXIT_BROWSER)
    page.locator("#read-aloud").click()
    return row, SpeechTarget(
        text=text,
        browser_language=str(details.get("browserLanguage") or ""),
        voice_language=str(details.get("voiceLanguage") or ""),
        voice_name=str(details.get("voiceName") or ""),
    )


# Raw: a cooked "\n" would put a line break inside the JavaScript string literal, which Chrome rejects.
ROW_TEXT_SCRIPT = r"""node => (node.messages || []).map(message => String(message.content || '')).join('\n\n')"""


def artifact_paths(out: Path, capture_suffix: str) -> tuple[Path, Path, Path]:
    # Distinct stems: with --capture-suffix .wav a shared stem would make ffmpeg overwrite its input.
    return out / f"recording{capture_suffix}", out / "capture.wav", out / "report.json"


def self_test() -> int:
    failures: list[str] = []

    def check(condition: bool, name: str) -> None:
        if not condition:
            failures.append(name)

    with tempfile.TemporaryDirectory(prefix="vibe-talk-speech-self-test-") as directory:
        root = Path(directory)
        silence = root / "silence.wav"
        tone = root / "tone.wav"
        clipped = root / "clipped.wav"
        for path, amplitude in ((silence, 0), (tone, 6000)):
            samples = array.array(
                "h",
                (
                    int(amplitude * math.sin(2 * math.pi * 220 * index / 16000))
                    for index in range(16000)
                ),
            )
            with wave.open(str(path), "wb") as sink:
                sink.setnchannels(1)
                sink.setsampwidth(2)
                sink.setframerate(16000)
                sink.writeframes(samples.tobytes())
        check(bool(signal_problems(measure_signal(silence))), "silence was accepted")
        check(not signal_problems(measure_signal(tone)), "an audible clean signal was rejected")
        with wave.open(str(clipped), "wb") as sink:
            sink.setnchannels(1)
            sink.setsampwidth(2)
            sink.setframerate(16000)
            sink.writeframes(array.array("h", [32767, -32768] * 8000).tobytes())
        check(bool(signal_problems(measure_signal(clipped))), "heavily clipped audio was accepted")

    transcript = parse_transcript(
        '{"text":"the blue bicycle waits beside the library","language":"en-US","confidence":0.9}'
    )
    problems, precision, overlap = transcript_problems(
        transcript,
        "The blue bicycle waits beside the library every morning.",
        "en-GB",
        DEFAULT_WORD_PRECISION,
    )
    check(not problems and precision > 0.8 and overlap >= 3, "matching English speech was rejected")
    wrong_language = Transcript("the blue bicycle waits beside the library", "fr-FR", 0.8)
    wrong_language_problems, _, _ = transcript_problems(
        wrong_language,
        "The blue bicycle waits beside the library.",
        "en-US",
        DEFAULT_WORD_PRECISION,
    )
    check(len(wrong_language_problems) == 1, "wrong-language speech was accepted")
    unrelated = Transcript("garbled unrelated syllables with no matching content", "en-US", 0.2)
    unrelated_problems, _, _ = transcript_problems(
        unrelated,
        "The blue bicycle waits beside the library.",
        "en-US",
        DEFAULT_WORD_PRECISION,
    )
    check(len(unrelated_problems) >= 2, "unrelated recognised words were accepted")
    check(playback_source("--audio-source playback") == "playback", "playback source not found")
    check(playback_source("--audio records microphone") is None, "microphone capture was accepted")
    try:
        parse_transcript("not json")
        failures.append("invalid transcriber output was accepted")
    except HarnessError:
        pass
    for suffix in (".mp4", ".wav"):
        capture, wav_path, _ = artifact_paths(Path("out"), suffix)
        check(capture != wav_path, f"a {suffix} capture shares its path with the converted WAV")
    try:
        joined = subprocess.run(
            [
                "node",
                "-e",
                "process.stdout.write(JSON.stringify(("
                + ROW_TEXT_SCRIPT
                + ")({messages: [{content: 'first'}, {content: 'second'}]})))",
            ],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        check(
            joined.returncode == 0 and json.loads(joined.stdout or "null") == "first\n\nsecond",
            "the row-text script does not evaluate to the joined message text",
        )
    except FileNotFoundError:
        failures.append("node is required to check the row-text script")

    if failures:
        print("android device-speech self-test FAILED:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return EXIT_SELF_TEST
    print("android device-speech self-test: 12 controls passed")
    return EXIT_OK


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description=(
            "Drive vibe-talk in Android Chrome, capture rendered device speech, and reject "
            "silence, corrupt levels, the wrong language, or unrelated recognised words."
        )
    )
    result.add_argument("--url", help="served /voice URL; required except with --self-test")
    result.add_argument("--serial", help="adb serial; required only when more than one device is connected")
    result.add_argument("--channel", help="channel id to select; defaults to the page's current channel")
    result.add_argument("--message-id", help="loaded message id to read; mutually exclusive with --message-index")
    result.add_argument("--message-index", type=int, default=0, help="zero-based loaded row to read (default: 0)")
    result.add_argument("--chrome-package", default="com.android.chrome", help="Android Chrome package (default: com.android.chrome)")
    result.add_argument("--duration", type=int, default=15, help="capture duration in seconds, 5..60 (default: 15)")
    result.add_argument("--out", type=Path, help="artifact directory (default: debug/android-device-speech/<UTC timestamp>)")
    result.add_argument("--transcriber", help="STT adapter executable; required unless --signal-only")
    result.add_argument("--capture-adapter", help="capture executable for devices without screenrecord playback capture")
    result.add_argument("--capture-suffix", default=".mp4", help="capture filename suffix for ffmpeg detection (default: .mp4)")
    result.add_argument("--expect-language", help="expected language tag (default: Android Chrome's navigator.language)")
    result.add_argument("--minimum-word-precision", type=float, default=DEFAULT_WORD_PRECISION, help="minimum fraction of recognised words found in the selected message (default: 0.55)")
    result.add_argument("--signal-only", action="store_true", help="check captured signal but deliberately skip STT/language; not a full regression pass")
    result.add_argument("--self-test", action="store_true", help="test the evaluator offline and exit")
    return result


def main() -> int:
    args = parser().parse_args()
    if args.self_test:
        return self_test()
    if not args.url:
        raise HarnessError("--url is required", EXIT_USAGE)
    if not args.signal_only and not args.transcriber:
        raise HarnessError("--transcriber is required for a closed-loop language check", EXIT_USAGE)
    if args.message_id and args.message_index != 0:
        raise HarnessError("--message-id and a nonzero --message-index are mutually exclusive", EXIT_USAGE)
    if not 5 <= args.duration <= 60:
        raise HarnessError("--duration must be between 5 and 60 seconds", EXIT_USAGE)
    if not 0.0 < args.minimum_word_precision <= 1.0:
        raise HarnessError("--minimum-word-precision must be above 0 and at most 1", EXIT_USAGE)
    if not args.capture_suffix.startswith(".") or "/" in args.capture_suffix:
        raise HarnessError("--capture-suffix must be a simple extension such as .mp4 or .wav", EXIT_USAGE)
    if args.message_id and not args.message_id.isdigit():
        raise HarnessError("--message-id must contain decimal digits only", EXIT_USAGE)
    parsed_url = urlparse(args.url)
    if parsed_url.scheme not in {"http", "https"} or not parsed_url.netloc:
        raise HarnessError("--url must be an absolute http or https URL", EXIT_USAGE)
    token = os.environ.get("VIBE_TALK_WRITE_TOKEN", "")
    if not token:
        raise HarnessError("VIBE_TALK_WRITE_TOKEN is not set", EXIT_USAGE)

    serial = select_device(args.serial)
    ensure_playwright()
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out = args.out or Path(__file__).resolve().parent.parent / "debug" / "android-device-speech" / stamp
    out.mkdir(parents=True, exist_ok=False)
    capture_path, wav_path, report_path = artifact_paths(out, args.capture_suffix)

    run(
        [
            *adb_prefix(serial),
            "shell",
            "am",
            "start",
            "-W",
            "-a",
            "android.intent.action.VIEW",
            "-d",
            args.url,
            args.chrome_package,
        ],
        failure_code=EXIT_BROWSER,
    )

    from playwright.sync_api import sync_playwright

    target: SpeechTarget
    forwarded_port = 0
    with sync_playwright() as playwright:
        browser, forwarded_port = connect_android_chrome(playwright, serial)
        try:
            page = find_page(browser, args.url)
            row, target = open_target_page(
                page,
                token,
                args.channel,
                args.message_id,
                args.message_index,
            )
            capture = start_capture(serial, args.duration, capture_path, args.capture_adapter)
            try:
                row.click()
                time.sleep(0.25)
                if page.locator("#read-aloud").get_attribute("data-read-state") == "failed":
                    raise HarnessError("vibe-talk reported that device speech failed to start", EXIT_BROWSER)
                capture.finish(args.duration + 20)
            except BaseException:
                capture.abort()
                raise
        finally:
            if forwarded_port:
                run(
                    [*adb_prefix(serial), "forward", "--remove", f"tcp:{forwarded_port}"],
                    check=False,
                )

    convert_to_wav(capture_path, wav_path)
    metrics = measure_signal(wav_path)
    problems = signal_problems(metrics)
    transcript: Transcript | None = None
    precision: float | None = None
    overlap: int | None = None
    expected_language = args.expect_language or target.browser_language
    if not primary_language(expected_language):
        problems.append("Android Chrome exposed no language and --expect-language was not supplied")
    if not args.signal_only and not problems:
        transcript = run_transcriber(args.transcriber, wav_path)
        transcript_issues, precision, overlap = transcript_problems(
            transcript,
            target.text,
            expected_language,
            args.minimum_word_precision,
        )
        problems.extend(transcript_issues)

    report = {
        "result": "fail" if problems else "signal-only" if args.signal_only else "pass",
        "signal": asdict(metrics),
        "browser_language": target.browser_language,
        "selected_voice_language": target.voice_language,
        "selected_voice_name": target.voice_name,
        "expected_language": expected_language,
        "expected_text": target.text,
        "transcript": asdict(transcript) if transcript else None,
        "recognised_word_precision": precision,
        "recognised_word_overlap": overlap,
        "problems": problems,
    }
    report_path.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(
        f"captured {metrics.duration_seconds:.1f}s; active {metrics.active_seconds:.1f}s; "
        f"RMS {metrics.rms_dbfs:.1f} dBFS; peak {metrics.peak_dbfs:.1f} dBFS"
    )
    if transcript is not None:
        print(
            f"STT language {primary_language(transcript.language)}; "
            f"recognised-word precision {precision:.1%}; overlap {overlap} words"
        )
    print(f"artifacts: {out.resolve()}")
    if problems:
        raise HarnessError("; ".join(problems), EXIT_SIGNAL if signal_problems(metrics) else EXIT_TRANSCRIPT)
    if args.signal_only:
        print("SIGNAL ONLY: audible audio passed, but language and words were not checked")
    else:
        print("PASS: Android emitted audible speech in the expected language with matching words")
    return EXIT_OK


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except HarnessError as error:
        print(f"android-device-speech: {error}", file=sys.stderr)
        raise SystemExit(error.exit_code)
