# xara on Windows

xara runs natively on Windows (x86_64, Windows 8 or newer). The modem, the
web UI, the TCP ports (8300/8301 cmd+data, 8100 KISS), recording, and
xara-monitor all work exactly as on Linux. This file explains what is
different and why.

## the short version

one codebase, one command: `cargo build --release` works on both platforms.
the audio backend, the PTT wiring, the signal handling and the config file
location pick the right implementation for the OS you build on; features
name intent, the platform decides what is real.

## audio

Linux uses PipeWire/Pulse or ALSA. Windows uses WASAPI through the cpal
crate, with no libraries to install.

the rules:

 - `--capture` and `--playback` take the Windows device *display name*,
   as shown in Windows Sound settings (Win+R, `mmsys.cpl`). matching is
   the whole name, case-insensitive, so quote it:

         xara.exe --call DL9ABC --hf 2300 ^
                  --capture "Eingang (USB Audio CODEC)" ^
                  --playback "Lautsprecher (USB Audio CODEC)"

 - `xara --list-audio` (or `xara-monitor --list-audio`) prints the exact
   names and exits; that is the fastest way to find them.
 - `--radio` must name the playback device explicitly; "default" is
   refused, same idea as the Linux null-sink guard. without `--radio`
   xara plays wherever you point it.
 - a stereo device is fine: xara downmixes capture to mono and duplicates
   its mono output to both channels. the sample rate must include 48 kHz
   (essentially every Windows audio device does).
 - `--audio pulse|alsa|wasapi` still parses everywhere: `--audio pulse`
   on Windows just tells you it is not available in this build.

internally, WASAPI delivers audio through callbacks on its own real-time
thread. xara bridges those callbacks to the same blocking read/write
contract the ALSA and Pulse backends use, through two small lock-free
rings, so the modem loop did not change at all.

## PTT

 - `--ptt vox`, `--ptt rts:DEV` and `--ptt dtr:DEV` work; on Windows DEV
   is a COM port: `--ptt rts:COM3`.
 - `--ptt rigctld[:HOST:PORT]` works identically (it is plain TCP).
 - `--ptt cm108:...` is **not supported**: it depends on Linux hidraw.
   use a COM-port interface, rigctld, or vox instead.
 - PTT lead/tail/max/watchdog timing and the "release on stall/panic"
   safety logic are the same code as on Linux.

## stopping xara

there are no unix signals in the usual sense. Ctrl+C, Ctrl+Break and
closing the console window all do what SIGINT does on Linux: xara stops
the audio loop, releases PTT, and exits 130. a second Ctrl+C while it is
still shutting down releases the PTT after the same two-second grace and
force-exits, just like the SIGTERM-twice path on Linux.

## config and state files

 - settings file: `%APPDATA%/xara/xara.conf` instead of
   `~/.config/xara/xara.conf`. same `key = value` format; see
   `dist/xara.conf.example`.
 - `--state-dir` works the same. the systemd `STATE_DIRECTORY` variable
   obviously does not exist here.
 - the POSIX permission checks (private state dir, 0600 UI token file)
   have no Windows equivalent; isolation comes from your user account's
   ACLs. the token is still generated, validated and reused the same way.
 - when xara restarts itself (tune in the UI, or `--record` parts),
   Windows has no `exec()`: the new process is spawned and the old one
   exits normally.

## running as a service

there is no systemd. run xara in a console, or start it at logon with
Task Scheduler. the `dist/systemd/` units are Linux-only and not shipped
in the Windows zip.

## building

Windows (a Visual Studio or Build Tools install, or just the default
rust toolchain on a Windows machine):

    cargo build --release

Cross-building the gnu flavor from Linux (produces working .exe files;
the msvc flavor is what CI builds):

    rustup target add x86_64-pc-windows-gnu
    cargo build --release --target x86_64-pc-windows-gnu -p xara

Release packaging: GitHub Actions builds `xara-<ver>-x86_64-pc-windows-msvc.zip`
from `scripts/release_bins_win.ps1` alongside the Linux tarballs, with
the same SHA256SUMS and provenance attestation. The zip contains
`xara.exe`, `xara-monitor.exe`, LICENSE and README; it needs no runtime
DLLs beyond stock Windows. The release (and CI) build adds
`--features getrandom`: the web UI token is then drawn from the `rand`
crate, because Windows has no `/dev/urandom` for the default code path.

## what changed in the code (for the curious)

 - audio backends are gated by feature *and* platform: `alsa`/`pulse`
   compile on unix only, `wasapi` on windows only; the shared
   `Audio` trait (48 kHz i16 `read`/`write`, plus the 0.1.2 rate-matcher
   `queued`/`backlog`) is the only interface the modem sees.
 - the TCP host ports use the `socket2` crate instead of raw libc
   socket calls, so `SO_REUSEADDR`, the one-client pause/resume and the
   listener shutdown work on both platforms.
 - PTT serial ports use `EscapeCommFunction`/`GetCommModemStatus` on
   Windows and the tty ioctls on Linux, behind the same `Ptt` trait.
 - process-wide signal trapping is `sigaction`+self-pipe on unix and
   `SetConsoleCtrlHandler` on Windows.
 - file-permission bits (`0o600`, `0o700`) are unix-only details, cfg'd
   out on Windows; atomic tmp+rename writes and the symlink-trap test
   work everywhere (hard links stand in on Windows).
 - the DSP itself needed nothing: the turbo decoder's SSE2 path is part
   of the Windows x86_64 baseline.

## status

verified automatically: the whole Linux test suite, a Windows CI job
(fmt/clippy/tests on windows-latest), and the cross-built .exe files run
end-to-end under wine (device listing, WASAPI endpoint handling, stdin
monitor decoding). still worth a hands-on pass on real hardware: a live
sound-card loopback QSO, a COM-port PTT, and Ctrl+C behavior in a console
window.
