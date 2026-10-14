# Bufferless in C

A one-to-one C port of the Rust app in `../src`: same capture pipeline, same
settings window, tray menu and notifications, same `config.toml`, same log
lines. It's linked without the C runtime (`src/crt.c` and `src/rt.c` provide
the little that's needed).

| File | Size |
| --- | --- |
| `dist\bufferless-c.exe` | ~67 KB |
| `dist\bufferless-c-tiny.exe` (UPX) | ~39 KB |
| `dist\bufferless-c-x86.exe` (32-bit) | ~54 KB |
| `dist\bufferless-c-x86-tiny.exe` (32-bit, UPX) | ~34 KB |

| `dist\bufferless-c-x86-crinkler.exe` (32-bit, Crinkler) | ~24 KB |

The 32-bit build is optional (`-Arch x86`). It's smaller because x86 code is
denser and has no unwind tables, but a 32-bit process can only use about 3.5 GB,
so very large replay buffers (say 20 minutes at 25+ Mbps) won't fit.

## Building

Needs Visual Studio with the C++ tools, Python (for the icon) and, for the
packed build, [UPX](https://upx.github.io/).

```
powershell -File c\build.ps1
```

Pass `-NoUpx` to skip the packed build, or `-Arch x86` for the 32-bit one.

### The Crinkler build

For fun: `-Arch x86 -Crinkler -CrinklerPath path\to\Crinkler.exe` links with
[Crinkler](https://github.com/runestubbe/Crinkler), the demoscene's
compressing linker for 4k intros, instead of `link.exe`. Same app, ~24 KB.
Things to know:

- Crinkler can't embed resources, so the exe has no icon in Explorer, and the
  modern control style is switched on at runtime (borrowing the manifest inside
  Windows' `shell32.dll`). The settings window's title bar is a few pixels
  shorter, since Windows 10 compatibility can only be declared in a manifest.
- Its packed header leaves large default thread stacks, so the process commits
  about 0.5 GB it never touches. Actual RAM use is normal, but it uses up 32-bit
  address space, leaving less room for the replay buffer.
- Startup takes about half a second while it decompresses.
- Every antivirus flags Crinkler output. Keep it to yourself. The cursor shaders are shared with the
Rust build (`..\shaders`), and the icon is drawn by `tools\make_icon.py` with the
same geometry as `..\src\icon.rs`.

| File | What's in it |
| --- | --- |
| `src/capture.c` | Screen capture, cursor overlay, GPU colour conversion, frame timing |
| `src/encoder.c` | Hardware H.264 encoder |
| `src/audio.c` | System audio and mic capture, mixing, AAC encoding |
| `src/ring.c` | The replay buffer |
| `src/mux.c` | MP4 writer |
| `src/app.c` | Tray icon, hotkey, saving clips |
| `src/settings.c` | Settings window |
| `src/config.c` | `config.toml` reading and writing |
| `src/util.c` | Icons, sounds, clip names, autostart |
| `src/rt.c`, `src/crt.c` | The bits of a C runtime the app needs |
| `src/crt32.c` | 64-bit math helpers that 32-bit code needs |
