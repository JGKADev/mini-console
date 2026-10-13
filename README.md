# Mini Console

An unofficial Windows control app for the **TC-Helicon GoXLR Mini**. It does what the official GoXLR
App does day to day (mixer, routing, mic, lighting, profiles) in a single small program, and adds a
guided **mic auto-tune**.

> Mini Console is an independent project. It is not made, endorsed or supported by TC-Helicon or
> Music Tribe. "GoXLR" and "TC-Helicon" are trademarks of their owners and are used here only to say
> which device this app works with.

## Features
- **Mixer:** the four faders and their mute buttons, the channels without a fader, live meters for
  the PC channels. Moves on the Mini and in the window stay in sync.
- **Routing:** the full input → output matrix.
- **Mic:** mic type, gain, 6-band EQ, noise gate, compressor, de-esser, bleep level.
- **Mic auto-tune:** a guided test of about a minute. It records your voice through the GoXLR's ASIO
  driver (raw and processed mic at once), measures level, room noise, tone, sibilance and pops,
  proposes settings with a reason for each, lets you try them, and plays before/after (also
  volume-matched). **Keep** saves a new mic profile; your existing one is never overwritten.
- **Lighting:** colours, fader display styles, animation.
- **Profiles:** uses the same `.goxlr` and `.goxlrMicProfile` files as the official app
  (Documents\GoXLR). Changes are written back into the original file, only the values you changed,
  with a backup first.
- **Start with Windows** (optional), and a live "wire" view of every packet sent to the device.

## Safety: this app never flashes the device
Firmware updates are not a feature, by design. `dcp::ALLOWED_CATEGORIES` in `src/dcp.rs` is the only
path to the device. It contains only the GoXLR control commands; firmware update and every
erase/program command are absent, and a unit test fails if one is ever added.

## Requirements
- Windows 10 or 11, 64-bit.
- The **TC-Helicon GoXLR driver** installed (it comes with the official GoXLR App). Mini Console
  talks to the Mini through it. The mic auto-tune also uses the driver's ASIO interface.
- WebView2 runtime (part of Windows 11; on Windows 10 it comes with Microsoft Edge).
- Only one program can control the Mini at a time. **Close the official GoXLR App** (and stop its
  autostart) before using Mini Console; the app waits while the official one is running.

## Install
Install Rust from [rustup.rs](https://rustup.rs) (MSVC toolchain), then in PowerShell:

```powershell
.\install.ps1
```

This builds the app and installs it to `%LOCALAPPDATA%\Programs\Mini Console` with a Start menu
shortcut. No admin rights needed. Settings and backups live in `%LOCALAPPDATA%\mini-console`.

## Build and test
```powershell
cargo build --release
cargo test
```

The exe also has a few command-line tools: `mini-console info` (read-only device info),
`plan` (prints the startup state built from a profile, offline), `snapshot` (the UI state as JSON),
`record` and `autotune-wav` (record and analyse a mic take), and `golden` (prints everything the
app derives from a profile and what a fixed script of controls sends, for checking that a change
keeps behaviour identical). Run it with an unknown command to list them.

## Status and limits
- **GoXLR Mini only.** The full-size GoXLR is not supported.
- Tested on one GoXLR Mini (firmware 1.2.1.49). Other firmware versions should work but are untested.
- The mic auto-tune's EQ suggestions use a model of the Mini's EQ bands; the check step measures the
  real result and can refine from there.

## Credits
- Fonts: Michroma, Hanken Grotesk and JetBrains Mono, under the SIL Open Font License (`ui/fonts`).
- Mic tone target: the long-term average speech spectrum of Byrne et al. (1994).

## Licence
MIT, see `LICENSE`.
