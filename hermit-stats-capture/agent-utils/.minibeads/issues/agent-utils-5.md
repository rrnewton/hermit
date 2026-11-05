---
title: 'device-speech-e2e: close the Android speech-output regression loop'
status: closed
priority: 0
issue_type: task
labels:
- vibe-talk
- audio
- android
- testing
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T01:32:02.484760695+00:00
updated_at: 2026-09-25T12:04:42.512388607+00:00
---

# Description

[gpt-5.6-sol] Reproduce the reported on-device speech noise or wrong-language output, prevent selection of an incompatible local voice, and provide an opt-in device harness that captures audio and evaluates audibility plus language or transcript correctness without storing private message text.

# Acceptance Criteria

Offline negative controls pass; an attached Android run records actual emitted speech and rejects silence, corrupt audio, wrong language, or poor transcript overlap; normal validation remains hermetic.

# Notes

A work-in-progress patch now rejects local voices whose language does not match navigator.languages, adds browser regression tests, and adds an opt-in ADB capture plus external-STT harness with offline negative controls. This host has ADB and ffmpeg but no attached device, emulator SDK, or STT adapter, so the physical closed loop remains unverified.

[gpt-5.6-sol] 2026-09-25 external-device blocker: `adb devices -l` and `adb mdns services` both found zero Android targets. The host has `/dev/kvm` and bare `qemu-system-x86_64`, but no `emulator`, `avdmanager`, or `sdkmanager` executable, no `~/.android/avd`, no Android SDK in the standard local or system locations, and no cached Android system image in the searched local installation/cache paths. It also exposes no `/dev/snd`, installed audio capture command, STT executable, or cached standalone STT model. A direct harness attempt therefore stopped before browser startup with exit 30: `expected exactly one authorized Android device, found 0`. The hermetic evaluator still passes all nine controls. Completion requires an authorized Android Chrome target, working playback capture (or a capture adapter), and an STT adapter; no actual speech was synthesized or claimed as verified here.

[opus 5.5] 2026-09-25 closed on the emulator closed loop. No physical device was attached. The run used a headless API 34 Google Play x86_64 emulator with Android Chrome. The page chose a local en_US voice and synthesized the selected row. Emulator gRPC audio capture fed a local STT adapter. The harness passed: language en, a 23-word overlap, 0.8846 word precision, and no signal or transcript problems.

The same harness flagged the audio-source icons. Against the old page it exited 31 because the phone icon stayed visible in agent mode. After the fix it passed on both states: cloud icon in agent mode, phone icon in device mode. The agent-mode label and title come from the backend provider profile through client-config `read_aloud.label`, and the page carries no vendor string.

The run also fixed two harness bugs, each now covered by a hermetic self-test control:
- a cooked `'\n\n'` in the row-text script caused a JavaScript SyntaxError;
- a `.wav` capture shared its stem with the converted WAV, so ffmpeg overwrote its own input.

Of the live negative controls, wrong language was rejected with exit 34. The emulator tap records guest output before Android stream volume, so a muted-volume control cannot fail there. That check, along with speaker routing and OEM TTS engines, needs a physical phone with a microphone capture adapter. Audio, transcripts and screenshots stayed outside the repository and were deleted.
