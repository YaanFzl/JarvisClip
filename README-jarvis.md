# Jarvis Clip

A voice-controlled replay buffer for Windows. It always keeps the last 30 seconds
of the screen and the system audio. Say "jarvis clip that" to save them as an MP4.

This app is based on [veyzyn/bufferless](https://github.com/veyzyn/bufferless)
(MIT, commit 03cf322). Capture and encoding run on the GPU (Media Foundation).
This fork adds:

- **Voice commands** with [Vosk](https://alphacephei.com/vosk/) (offline, Apache-2.0).
  Vosk listens only for a fixed list of phrases, so it rarely triggers on game
  sound or talking. Code: `src/voice.rs`.
- **Replay buffer on disk** (default). Encoded video goes to temp files in
  `buffer\`. One file is written for every 5 seconds. Each file deletes itself
  when its footage ages out, when the app exits, or when the app crashes. Only a
  small index stays in RAM. You can switch back to RAM in the UI. Code:
  `src/ring.rs`.
- **Control window**: a dark UI with the buffer status, a voice feed, your clips
  with previews, and settings. The app serves it on `127.0.0.1` and shows it in a
  Chrome, Edge or Brave `--app` window, so you do not see tabs or an address bar.
  The browser runs only while the window is open. Code: `src/ui.rs`,
  `src/ui.html`.
- **Portable.** Everything is in the exe's folder: `config.toml`,
  `jarvis-clip.log`, `clips\`, `buffer\`, `vosk\`, and `ui-profile\` (the
  window's browser profile). Nothing is written to AppData or to C:.

## Folder layout (`dist\`)

```
jarvis-clip.exe
vosk\libvosk.dll, libstdc++-6.dll, libgcc_s_seh-1.dll, libwinpthread-1.dll   (vosk-win64-0.3.45)
vosk\model\                                                                  (vosk-model-small-en-us-0.15)
```

If you delete `vosk\`, voice is turned off. Everything else still works.

## Use

1. Run `jarvis-clip.exe`. A tray icon appears and the buffer starts.
2. Double-click the tray icon to open the control window.
3. Say one of these commands:

| Say | Effect |
| --- | --- |
| "jarvis clip that" / "jarvis clip it" / "jarvis clip" | Saves the last 30 s to `clips\` |
| "jarvis record" / "jarvis start recording" | Starts a full recording |
| "jarvis stop" / "jarvis stop recording" | Stops the recording and saves it |

The hotkeys also work: Alt+F10 saves a clip, and Ctrl+Alt+F9 starts or stops a
recording. The microphone is the Windows default recording device.

## Settings

All settings are in the control window and are saved as soon as you change them.
To change the hotkeys, the audio devices or the per-app tracks, go to
**General → Advanced**. This opens the classic settings window.

`voice_min_confidence` is the strictness setting. If game sound triggers it,
raise the value. The log records every phrase it hears with its confidence.

## Resource use (measured on the i5-10300H laptop, 1080p60, 20 Mbps)

| | Working set | Private (commit) |
| --- | --- | --- |
| Voice off, buffer on disk | ~160 MB | ~185 MB |
| Voice off, buffer in RAM | ~240 MB | ~335 MB |
| Voice on (Vosk), buffer on disk | ~280 MB | ~560 MB |

On disk, the buffer is about 2.5 MB/s of writes, or roughly 100 MB of temp files
at any moment.

## Build

You need the Rust GNU toolchain and these tools from MSYS2 binutils: `as`,
`dlltool` and `windres`. They are extracted to `D:\ML\.tmp\binutils\mingw64\bin`.

```bash
bash build.sh        # output: dist/jarvis-clip.exe; cargo caches in D:\ML\.cargo, temp in D:\ML\.tmp
```

The `nostd` (tiny) build from upstream is not supported, because the fork uses
`std` for the disk buffer, the UI server and voice.

## Test voice without the mic

`JARVIS_VOICE_WAV=<16 kHz mono 16-bit .wav>` makes the app feed that file to
the recognizer in real time instead of the microphone.
