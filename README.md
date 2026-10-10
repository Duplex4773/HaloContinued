# Halo Continued

**Battery monitoring and device controls, without a heavyweight background suite.**

Halo Continued is an independent Rust/Win32 application for Windows 11 x64,
inspired by [HaloBattery](https://github.com/HeyOkay/HaloBattery). It began as a
Rust port and carries forward upstream device protocols, provider behaviour,
tests and community hardware reports, while adding practical device controls
and richer battery insights. It is maintained by
[Duplex4773](https://github.com/Duplex4773), independently of HaloBattery.

Native Windows controls keep it small and responsive. There is no Python, .NET,
browser or webview runtime to install, and no vendor suite is required for the
supported controls.

## What you can do

| Feature | What it offers |
| --- | --- |
| Battery monitoring | A tray icon for each battery-powered device, charging animation, warning colours, low-battery and full-charge notifications, and optional alert sounds. |
| Polling rates | Read and change supported mouse and keyboard rates. Supported mice show the last confirmed rate on hover and offer rate selection in the tray menu. |
| Automatic fullscreen boost | Opt in to a higher mouse polling rate during Windows-reported fullscreen gaming, then restore the previous rate on the desktop. |
| Mouse sensitivity (DPI) | Read, change and restore sensitivity on selected Razer and Logitech mice. The app shows supported ranges and saves original settings before changes. |
| Hardware sleep timers | Set how long a supported mouse waits before sleeping; the device handles the timer itself. |
| Low-battery mode | Optionally reduce polling rate and shorten sleep time below your chosen threshold. Fullscreen boost takes priority for competitive play. |
| History and Insights | Thirty days of history, estimated battery life, battery sessions and estimates that follow your mix of normal, boosted and low-battery polling rates. |
| Personalisation | Rename devices, hide icons, choose icon styles and colours, set per-device alert thresholds and choose which providers to enable. |
| Native dashboard | Devices, History, Insights and Settings with light/dark appearance following Windows, plus matching tray menus. |
| Portable use | A self-contained executable, optional local settings and optional startup with Windows. Closing the dashboard keeps monitoring active. |

Batteryless keyboards appear on Devices without battery tray icons, alerts,
history or battery-life estimates.

## Get started

1. Extract a portable package, or build from source below.
2. Run **HaloContinued.exe**. Monitoring starts and the dashboard opens.
3. Open **Settings** to choose appearance, notifications and startup preferences.
4. To configure hardware, enable **Allow device settings changes**, then select
   your device on **Devices**. Only supported controls are offered.

Closing the dashboard leaves monitoring active. Open it again from a tray icon
or by launching the executable. Choose **Exit Halo Continued** from the tray menu
to quit. `--background` starts without opening the dashboard.

Settings and history normally live in `%APPDATA%\HaloBatteryNext`. For portable
settings, create `portable.txt` beside the executable; data goes in the adjacent
`HaloBatteryNext-data` folder. An unwritable portable folder falls back to the
normal location. These folder names preserve compatibility with earlier Rust
builds; upstream HaloBattery settings are separate and are not imported.

Exit an older build before upgrading. Keep portable data beside the replacement
executable. If its location changes, save **Start with Windows** again to update
the startup path. Update checking remains disabled unless a release repository
is configured.

## Device support and limitations

Battery monitoring covers **28 provider families** across supported mice,
keyboards, headsets and controllers. Configuration support is narrower than
battery support, and depends on the exact model, firmware and connection.

| Control | Enabled support |
| --- | --- |
| Mouse polling | Selected Razer, Logitech and MCHOSE models; available rates depend on the device and connection. |
| Keyboard polling | Razer Huntsman V2 TKL, Huntsman V2, BlackWidow V4, V4 Pro and V4 75%. Recognized Corsair models are shown, but changes requiring an ongoing software session are unavailable. |
| DPI | Selected Razer and Logitech models, with original-setting backups and readback checks. Protocol tests do not establish physical DPI verification. |
| Manual sleep timers | Selected Razer, SteelSeries and ASUS models. SteelSeries reports command delivery without claiming confirmed readback. |
| Automatic low-battery sleep | Supported Razer and ASUS timers whose previous values can be read and verified. |

The wireless **Razer DeathAdder V4 Pro** has user-verified polling changes,
fullscreen boost/restoration and hardware sleep at the tested timeout. This does
not verify every connection, sleep value or power-cycle behaviour. Battery
reports inherited from HaloBattery retain their **User-verified in parent app**
distinction. Simulated tests do not establish hardware verification.

Fullscreen boost is off by default and enabled per mouse. Windows is checked
every five seconds when needed; boosting requires ten seconds of stable
fullscreen gaming, and restoration waits fifteen seconds on the desktop. Some
borderless games are missed. Keep the app running for restoration.

Low-battery mode is also opt-in. It only lowers a faster polling rate or shortens
a longer sleep timer. Boost takes priority over its polling rate. Prior settings
return when charging or when battery recovers five percentage points above the
threshold. The mode stays saved but inactive on a recognized wired connection.
Manual changes take priority; uncertain writes are not automatically retried.

DPI changes first save recovery data under `dpi-backups` in the app's data folder.
Restore previous requires the same verified connection/profile during the current
dashboard session. Recovery files remain after closing it, but are not applied
automatically. Reconnecting, suspending or quitting can leave hardware at its
current setting; check it again before making further changes.

Configuration uses ordinary user-mode HID access, without additional drivers,
elevation, game-process access or input interception. This is not a guarantee of
approval by every anti-cheat service.

Insights learns from reliable battery drops and the time spent at confirmed
polling rates. It excludes sleeping, charging, missing readings and uncertain
transitions where appropriate. “Time used” estimates device availability; it
does not monitor input or measure gaming time. Estimates remain in learning
until sufficient evidence is available.

## Performance: Rust versus upstream

A native executable avoids the bundled Python runtime. Bounded workers, cached
HID discovery and tray graphics, event-driven waits and batched SQLite writes
limit background work. Charging frames are pre-rendered; the animation timer
stops when no icons need it. Dashboard graphics are released when it closes,
and hovering an icon does not query hardware. The standard executable is about
**3.22 MiB**, including HIDAPI, SQLite and the MSVC runtime.

**Controlled comparison, 10 October 2026:** one simulated mouse, three-minute
samples after warm-up, fresh data and dashboard never opened. CPU percentages
refer to **one logical core**; RAM means **private committed memory**.

| Build | CPU: animation off | CPU: animation on | Mean RAM: animation off | Mean RAM: animation on |
| --- | ---: | ---: | ---: | ---: |
| HaloBattery 1.14.0 (Python harness) | 0.069% | 0.130% | 32.56 MiB | 33.78 MiB |
| Halo Continued, standard Rust | 0.017% | 0.035% | 4.75 MiB | 4.55 MiB |
| Halo Continued, AVX512 Rust | Below counter resolution | 0.043% | 4.81 MiB | 4.71 MiB |

Rust used about **85–87% less private memory** in these samples and recorded less
CPU time. All versions had very small absolute CPU costs. AVX512 showed **no
consistent background-monitoring advantage**; the standard build is the default.
Below counter resolution means Windows recorded no CPU-time increment during
that sample, not that the application performed no work.

These are **simulated application-code comparisons, not published-binary or
live-hardware benchmarks**. Upstream revision `0e383bb` ran under Python 3.13.15
rather than its distributed Python 3.12 runtime; Rust used release-optimised
native test harnesses. The host was Windows 11 on a Ryzen 9 9950X3D, with each
measured process restricted to one logical core. A normal app instance remained
running outside the measurements. Each case has one sample; very low CPU readings
are sensitive to counter resolution and background activity.

A separate **11 October audit** of the newer code, after **40 dashboard open/close
cycles**, averaged **0.052% of one logical core** and **6.22 MiB private memory**
(6.38 MiB peak) over three minutes with a simulated mouse and animation off.
Native resource counts stayed stable. This is a different workload, not a direct
speedup comparison with the table. The audit also removed work on unchanged empty
provider results, reduced temporary allocations when loading saved estimates,
and eliminated repeated string allocations in Insights queries.

## Build from source

Requirements: Windows, Rust's MSVC toolchain (Rust 1.88 or newer), Visual Studio
C++ Build Tools with the Windows SDK, and PowerShell 7.

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
.\tools\build-rust.ps1
.\tools\package-rust.ps1 -Version 0.1.0
```

The executable is `target/x86_64-pc-windows-msvc/release/HaloContinued.exe`.
The portable ZIP is `dist/HaloContinued-0.1.0-windows-x64.zip`. Release builds use
size optimisation, link-time optimisation, static runtime linking and stripped
symbols; local compiler paths are removed. Packages include dependency notices
and licences. Optional API/import validation uses `tools/validate-configuration.ps1`
with ripgrep and Visual Studio's dumpbin installed.

The five workspace crates separate device models, protocols, Windows integration,
storage and the application. Latest local validation: **677 passing tests**, with
three opt-in benchmark tests skipped, plus formatting and strict Clippy. Hardware
changes need a reviewed implementation and tests; recompiling alone does not add
support. No GitHub Actions workflows are included.

## Credits and licence

**Thank you to [HeyOkay](https://github.com/HeyOkay) and every
[HaloBattery contributor](https://github.com/HeyOkay/HaloBattery/graphs/contributors).**
Their application, device research, protocol implementations, tests and community
hardware reports are the foundation of this project. “Inspired by” describes the
independent direction; it does not remove the port's upstream origins or credits.

The port started from HaloBattery **1.13.0** (`a566a046`), with reviewed changes
through **1.14.0** (`0e383bb`). Halo Continued is not an official HaloBattery release.
The upstream copyright notice is preserved alongside the continuation's notice
in the [MIT licence](LICENSE).

Thanks also to the protocol researchers and contributors behind
[HeadsetControl](https://github.com/Sapd/HeadsetControl),
[Solaar](https://github.com/pwr-Solaar/Solaar),
[libratbag](https://github.com/libratbag/libratbag),
[logi-cli](https://github.com/meech-io/logi-cli),
[the Razer Windows protocol reference](https://github.com/Ar4ikov/openrazer-win/tree/6a626b2d11069ec8be6a7eda6e0dd8ee3ece0998),
[Rivalcfg](https://github.com/flozz/rivalcfg),
[G-Helper](https://github.com/seerge/g-helper),
[OpenLinkHub](https://github.com/jurkovic-nikola/OpenLinkHub),
[ckb-next](https://github.com/ckb-next/ckb-next),
[HyperHeadset](https://github.com/LennardKittner/HyperHeadset),
[Linux-Arctis-Manager](https://github.com/elegos/Linux-Arctis-Manager),
[Arctis-Sound-Manager](https://github.com/loteran/Arctis-Sound-Manager) and
[hitscan-battery](https://github.com/sopparus/hitscan-battery), along with the
[original protocol contributors](https://github.com/HeyOkay/HaloBattery/tree/0e383bb560c04f9d9ac4b163b04941c6ade4cb85).
External protocol references are not runtime dependencies. Portable packages
retain notices for the libraries they distribute.
