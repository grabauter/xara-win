# Changelog

## Unreleased

windows support

- xara and xara-monitor build and run on Windows (x86_64 MSVC)
- WASAPI audio backend (`--audio wasapi`); devices are Windows device names
- PTT on Windows: RTS/DTR on COM ports and rigctld (CM108 hidraw stays Linux-only)
- `--list-audio` on both binaries prints the device names for the chosen backend
- Ctrl+C and console close stop xara cleanly and release PTT, like SIGINT on Linux

## 0.1.1

recording feature and bugfixes

- `--record` flag captures rx, tx, and PTT in FLAC
- bugfix: long squelch tails no longer trigger false receive
- bugfix: phantom frames no longer send inappropriate compression

## 0.1.0

first release

- modem is fully compatible with VARA HF 4.9.0 and VARA FM 4.4.0
- web UI for configuration and management
- all the usual TCP ports (works with your favorite software)
- pipewire, pulse, and alsa support
- `xara-monitor`: passive VARA HF monitor (with web UI)
