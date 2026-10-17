# Halo Continued

**Battery monitoring, device controls and battery-life insights for Windows.**

**Performance goal:** keep CPU and RAM use as low as practical while adding
device controls, history and battery insights. The aim is no noticeable
performance compromise during work or gaming. The app still consumes resources;
usage varies with connected devices, settings and dashboard activity.

Current release: **1.0.4** for Windows 11 x64, available as an unsigned per-user
installer and a portable ZIP.

| Download | Use it for |
| --- | --- |
| [**V1.0.4 installer**](https://github.com/Duplex4773/HaloContinued/releases/download/v1.0.4/HaloContinued-1.0.4-windows-x64-setup.exe) | Install for your Windows account, with a Start menu shortcut and optional startup at sign-in. No administrator privileges required. |
| [**V1.0.4 portable ZIP**](https://github.com/Duplex4773/HaloContinued/releases/download/v1.0.4/HaloContinued-1.0.4-windows-x64.zip) | Extract the ZIP and run `HaloContinued.exe`. |

[SHA-256 checksums](https://github.com/Duplex4773/HaloContinued/releases/download/v1.0.4/HaloContinued-1.0.4-SHA256SUMS.txt)
· [Release notes](https://github.com/Duplex4773/HaloContinued/releases/tag/v1.0.4)

Both downloads are unsigned. Windows may show an unrecognised-app warning.

**New in 1.0.4:** Battery history, estimates and alerts no longer carry over to
different hardware that reuses a device identifier. Connections limited to
1000 Hz no longer show an empty automatic-boost selector. Preference linking
and sleep-setting checks also avoid unnecessary temporary allocations.
[Read the changelog](https://github.com/Duplex4773/HaloContinued/releases/tag/v1.0.4).

**Personal project:** I build Halo Continued for my own use. It is shared
publicly, but whether others use it is not a priority.

Halo Continued is an independent Rust/Win32 application for Windows 11 x64,
inspired by [HaloBattery](https://github.com/HeyOkay/HaloBattery). It began as a
Rust port and carries forward upstream device protocols, provider behaviour,
tests and community hardware reports, while adding practical device controls
and richer battery insights. It is maintained by
[Duplex4773](https://github.com/Duplex4773), independently of HaloBattery.

The dashboard uses native Windows controls. The portable executable includes its
runtime dependencies, and supported device controls work without installing a
vendor suite. Hardware changes are optional and disabled by default.

## What you can do

| Feature | What it offers |
| --- | --- |
| Battery monitoring | A tray icon for each battery-powered device, charging animation, warning colours, low-battery and full-charge notifications, and optional alert sounds. |
| Disconnected devices | Optionally keep disconnected device icons greyed with their last battery level until the app exits. |
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

## Screenshots

The actual V1.0.0 interface, captured with simulated devices and example history.
Images follow your preferred light or dark theme; no personal device data is shown.

<table>
  <tr><th>Devices and polling controls</th><th>Battery history</th></tr>
  <tr>
    <td><picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Duplex4773/HaloContinued/main/assets/screenshots/devices-dark.png">
      <img src="https://raw.githubusercontent.com/Duplex4773/HaloContinued/main/assets/screenshots/devices-light.png" alt="Devices page showing a simulated mouse, battery level, alert preferences and polling-rate controls" width="480">
    </picture></td>
    <td><picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Duplex4773/HaloContinued/main/assets/screenshots/history-dark.png">
      <img src="https://raw.githubusercontent.com/Duplex4773/HaloContinued/main/assets/screenshots/history-light.png" alt="Battery history page showing an example battery-level graph against estimated time used" width="480">
    </picture></td>
  </tr>
  <tr><th>Battery-life insights</th><th>Settings</th></tr>
  <tr>
    <td><picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Duplex4773/HaloContinued/main/assets/screenshots/insights-dark.png">
      <img src="https://raw.githubusercontent.com/Duplex4773/HaloContinued/main/assets/screenshots/insights-light.png" alt="Insights page showing example remaining battery life, polling-rate mix and automatic boost history" width="480">
    </picture></td>
    <td><picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Duplex4773/HaloContinued/main/assets/screenshots/settings-dark.png">
      <img src="https://raw.githubusercontent.com/Duplex4773/HaloContinued/main/assets/screenshots/settings-light.png" alt="Settings page with battery alerts, appearance, battery checks and optional device controls" width="480">
    </picture></td>
  </tr>
</table>

## Get started

1. Run the installer, extract a portable package, or build from source below.
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

The optional installer installs for your account in
`%LOCALAPPDATA%\Programs\HaloContinued`, without administrator privileges. It
adds a Start menu shortcut, with optional desktop shortcut and startup at sign-in.
Run a newer installer to upgrade; settings and history are preserved. Uninstall
from Windows **Settings > Apps > Installed apps**. Uninstalling keeps your data
and removes startup registration only if it still points to that installation.
Download releases from this project's GitHub repository.

Exit an older build before upgrading. Keep portable data beside the replacement
executable. If its location changes, save **Start with Windows** again to update
the startup path. Update checking remains disabled unless a release repository
is configured.

## Relationship to HaloBattery

HaloBattery provides the battery-monitoring foundation, including device
protocols and community hardware reports. Halo Continued builds on that work
with optional hardware settings and a dashboard for history and battery analysis.

This comparison refers to the reviewed [HaloBattery 1.15.0 revision](https://github.com/HeyOkay/HaloBattery/tree/2251f0534a8e886c9692ead42056059b4f69d8c0)
and Halo Continued 1.0.0:

| Area | HaloBattery | Halo Continued |
| --- | --- | --- |
| Everyday interface | Per-device tray icons and menus. | Per-device tray icons and menus, plus Devices, History, Insights and Settings pages. |
| Hardware settings | Focuses on battery monitoring and its preferences. | Adds polling-rate, sensitivity and hardware sleep controls for specifically supported devices. |
| Automatic device settings | Fullscreen detection adjusts battery checks and notifications. | Also offers opt-in fullscreen polling boosts and low-battery polling/sleep changes. |
| Battery analysis | Saves readings to estimate remaining use. | Adds 30-day charts, battery sessions and estimates that account for a mix of polling rates. |
| Packaging | Python application distributed with a bundled runtime folder. | Self-contained Rust executable, portable ZIP and optional per-user installer. |
| Windows support | Windows 10 and 11. | Windows 11 x64. |
| Release updates | In-app automatic updates. | Manual downloads and installer upgrades. |

The additional controls are useful when your hardware supports them; the
dashboard brings their settings and battery records together. Hardware coverage,
Windows version and your preferred workflow are practical reasons to choose
between the projects. Shared features such as notifications, portable settings
and working without vendor suites originate in HaloBattery too.

## Device support and limitations

Battery monitoring covers **30 provider families and 326 hardware IDs** across supported mice,
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

Battery support reviewed against HaloBattery **1.15.0 and subsequent fixes**
through [`2251f05`](https://github.com/HeyOkay/HaloBattery/commit/2251f05):

| Device or family | Added or corrected support | Hardware evidence |
| --- | --- | --- |
| Razer BlackShark V2 Pro (2020) | Receiver `1532:0528`; coarse readings, unknown power states and charging with unreliable percentages hidden. Its charging cable is not probed. | User-verified in parent app |
| Steam Controller (2025) | Pucks `28DE:1304` and `1305`, independent controller slots; passive battery reads, charging and sleep retention. | User-verified in parent app |
| NZXT Lift Elite Wireless | `1E71:2101` receiver and `2129` cable; battery and charging. | User-verified in parent app |
| MCHOSE A5 Pro Max | `2023:F013` 1K receiver, `F015` 4K receiver and `F019` cable; battery and charging on the model's vendor collection. | 1K receiver user-verified in parent app; other connections retain protocol-based support |
| Logitech HID++ 1.0 | Older devices read through battery registers, with coarse readings labelled where appropriate. | Register support user-verified in parent app |
| HyperX Cloud II Core Wireless | `03F0:0995` and `0795`; battery and charging. | User-verified in parent app |
| Pulsar X2 CrazyLight Mini | `3710:5406` receiver and `3414` cable. Pulsar/ATK/VXE sleeping readings remain while their receiver is present and refresh after activity. | Both connections user-verified in parent app |
| SteelSeries Arctis GameBuds X | `1038:2317`; existing GameBuds battery exchange. | User-verified in parent app |
| Corsair Dark Core RGB Pro SE | `1B1C:1B7F`; corrected routed battery reads with confirmation and bounded waits. | User-verified in parent app |
| Corsair Virtuoso SE / XT | SE `1B1C:0A40`, `0A3E`, `0A3D`; XT `0A64`, `0A62`; model-specific framing, battery and charging. | `0A40` user-verified in parent app; remaining IDs unverified in parent app |
| 8BitDo Pro 3 | Correct battery report over Bluetooth without changing controller mode. | Bluetooth user-verified in parent app |

Receiver/cable readings share an icon when trusted hardware identity proves they
belong together. Devices with ambiguous identities remain separate. These are
battery-support additions; they do not expand polling, DPI or sleep write allowlists.

After a slow Windows boot, an empty inventory is retried every ten seconds for
up to ninety seconds. Sleeping Pulsar-family mice use Windows' last-input timestamp
to request a targeted battery refresh, at most once every five seconds and outside
quiet gaming. Input contents are never read or saved. Silent WLmouse receivers
back off to every fifth scheduled poll after their initial attempts, with immediate
recovery when their collection inventory changes.

Fullscreen boost is off by default and enabled per mouse. Windows is checked
every five seconds when needed; boosting requires ten seconds of stable
fullscreen gaming, and restoration waits fifteen seconds on the desktop. Some
borderless games are missed. Keep the app running for restoration.

Refresh and changes to other devices preserve verified original polling rates
and sleep timers. Before restoring, the app checks the target's Windows connection
stamp, exact identity and current setting. A reconnect, missing connection proof
after a refresh, or an uncertain write cancels that setting's automatic recovery.
These checks run with configuration transactions; they add no background scan or timer.

With device settings enabled, launch also checks supported mice's current polling
rates in the background. Receivers have up to five minutes to become available;
failed reads get up to three attempts, at least fifteen seconds apart. A confirmed
read stops the checks. Saved-rate restoration remains one-shot: if it fails, the
app can read the current rate without repeating the change. No continuous polling
rate checks or additional background timer are used.

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
does not record keystrokes, mouse movements or gaming time. Estimates remain in learning
until sufficient evidence is available.

## Resource use

A native executable avoids the bundled Python runtime. Bounded workers, cached
HID discovery and tray graphics, event-driven waits and batched SQLite writes
limit background work. Charging frames are pre-rendered; the animation timer
stops when no icons need it. Dashboard graphics are released when it closes,
and hovering an icon does not query hardware. The standard executable is about
**3.32 MiB**, including HIDAPI, SQLite and the MSVC runtime.

**Controlled comparison, 10 October 2026:** one simulated mouse, three-minute
samples after warm-up, fresh data and dashboard never opened. CPU percentages
refer to **one logical core**; RAM means **private committed memory**.

| Build | CPU: animation off | CPU: animation on | Mean RAM: animation off | Mean RAM: animation on |
| --- | ---: | ---: | ---: | ---: |
| HaloBattery 1.14.0 (Python harness) | 0.069% | 0.130% | 32.56 MiB | 33.78 MiB |
| Halo Continued, standard Rust | 0.017% | 0.035% | 4.75 MiB | 4.55 MiB |
| Halo Continued, AVX512 Rust | Below counter resolution | 0.043% | 4.81 MiB | 4.71 MiB |

The Halo Continued harness used about **85–87% less private memory** in these
samples and recorded less CPU time. These results apply to the implementations
and test conditions described here. All versions had very small absolute CPU costs. AVX512 showed **no
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

The **12 October performance audit** removed two-second Bluetooth checks when
there are no links to monitor, and pauses configuration-device discovery while
the dashboard is minimized or hidden. Repeated window visibility messages no
longer enqueue duplicate work. Battery monitoring and automatic boost continue.
History sampling now reuses bounded buffers and decodes full readings only for
the graph points it keeps. A synthetic 43,200-row query was **21% faster**
(78.76 to 62.38 ms, median of nine interleaved runs on the same warmed SQLite
connection). A separate allocation fixture reduced temporary Rust allocation traffic
from **4.63 MiB to 0.62 MiB**, with identical output. Those allocation totals are
not resident RAM measurements; history processing runs only when requested.

Background regression checks on 12 October used the same simulated workload:
one three-minute run without animation and two three-minute runs with animation
per build. The updated build recorded **0.026% CPU without animation** and
**0.035–0.061% with animation**, versus 0.035% and 0.035–0.078% before. Mean private
memory stayed around **5 MiB** (4.92–4.99 MiB after; 4.74–4.87 MiB before). These
small CPU readings vary by a few Windows accounting ticks; the samples show no
clear CPU regression, not a proven sustained reduction in background CPU or RAM.

After the 1.15 support additions, a **14 October three-minute regression sample**
with one simulated charging mouse and the dashboard closed recorded **0.035% of
one logical core** and **4.59 MiB mean private memory** (4.71 MiB peak). It used the
same release test harness and warm-up method. This checks the simulated background
workload; newly supported hardware has not been resource-benchmarked locally.

The **17 October 1.0.2 audit** also skips unnecessary fullscreen checks and device
inventory copies when low-battery changes cannot run. A matched two-minute
simulated background sample, with one animated charging device and the dashboard
closed, recorded 0.078% CPU before and 0.052% after, with mean private memory of
4.71 and 4.69 MiB respectively (peaks 4.80 and 4.96 MiB). These measurements used
the patched code before its version update. The small differences may be Windows
accounting or run-to-run variation; they do not establish a sustained speedup.
No new background timers, workers or caches were added.

The follow-up **1.0.3 audit** removed repeated estimator identity allocations and
duplicate selected-device copies. A session-specific database index reduced the
Insights lookup from 400,402 to 7,954 SQLite instruction steps in a synthetic
fixture with 256 readings and 256 unrelated polling events. The existing 512 KiB
SQLite page-cache budget stays fixed; the index adds disk storage, not a new RAM
cache or periodic task.

Matched two-minute simulated background samples of published 1.0.2 source and
the patched source used for 1.0.3, before its version update, used one animated
charging device and the dashboard closed:

| Simulated build | CPU, one logical core | Mean private RAM | Peak private RAM |
| --- | ---: | ---: | ---: |
| Published 1.0.2 source | 0.078% | 4.88 MiB | 4.95 MiB |
| Patched source used for 1.0.3 | 0.052% | 4.76 MiB | 4.82 MiB |

These are single regression samples, not a new comparison with HaloBattery or
live-hardware benchmarks. Windows accounting resolution and run-to-run variation
can explain small differences; they do not establish a sustained speedup.

The **1.0.4 audit** removed temporary descriptor copies during preference linking
and temporary strings and lists during sleep-setting checks. History identity
checks borrow stored fields and keep only the preceding identity per query;
no background timers, workers or persistent caches were added.

Matched two-minute simulated samples, with one animated charging mouse and the
dashboard closed, measured the published 1.0.3 source against the patched source
used for 1.0.4, before its version update:

| Simulated build | CPU, one logical core | Mean private RAM | Peak private RAM |
| --- | ---: | ---: | ---: |
| Published 1.0.3 source | 0.078% | 4.87 MiB | 4.90 MiB |
| Patched source used for 1.0.4 | 0.039% | 4.81 MiB | 4.90 MiB |

These single samples show no observed resource regression. Small differences
can reflect Windows accounting resolution or run-to-run variation; they do not
establish a sustained reduction in CPU or RAM use or predict live-hardware usage.

## Build from source

Requirements: Windows, Rust's MSVC toolchain (Rust 1.88 or newer), Visual Studio
C++ Build Tools with the Windows SDK, and PowerShell 7.

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
.\tools\build-rust.ps1
.\tools\package-rust.ps1 -Version 1.0.4
```

The executable is `target/x86_64-pc-windows-msvc/release/HaloContinued.exe`.
The portable ZIP is `dist/HaloContinued-1.0.4-windows-x64.zip`. Release builds use
size optimisation, link-time optimisation, static runtime linking and stripped
symbols; local compiler paths are removed. Packages include dependency notices
and licences. Optional API/import validation uses `tools/validate-configuration.ps1`
with ripgrep and Visual Studio's dumpbin installed.

To also build the unsigned installer, install [Inno Setup](https://jrsoftware.org/isinfo.php)
6.3 or newer, then run:

```powershell
.\tools\package-installer.ps1 -Version 1.0.4
# For a portable compiler installation, add -Iscc 'path\to\ISCC.exe'.
```

This produces the portable ZIP, `dist/HaloContinued-1.0.4-windows-x64-setup.exe`
and `dist/HaloContinued-1.0.4-SHA256SUMS.txt`. The installer uses the same app and
dependency notices as the ZIP; it adds no service, updater or background process.
Exit Halo Continued from its tray menu before installing or uninstalling.
Installer regression checks can be run with
`.\tools\test-installer.ps1 -Iscc 'path\to\ISCC.exe'`. They use isolated test
identities, leave the running app alone and never launch the packaged app.
Add `-PreviousPackageZip 'dist/HaloContinued-0.1.0-windows-x64.zip'` to test an
upgrade from that version, including executable replacement, the Installed apps
version and preservation of settings and history.

The five workspace crates separate device models, protocols, Windows integration,
storage and the application. Latest local validation: **749 passing tests**, with
four opt-in benchmark tests skipped, plus formatting and strict Clippy. Hardware
changes need a reviewed implementation and tests; recompiling alone does not add
support. No GitHub Actions workflows are included.

## Credits and licence

**Thank you to [HeyOkay](https://github.com/HeyOkay) and every
[HaloBattery contributor](https://github.com/HeyOkay/HaloBattery/graphs/contributors).**
Their application, device research, protocol implementations, tests and community
hardware reports are the foundation of this project. “Inspired by” describes the
independent direction; it does not remove the port's upstream origins or credits.

The port started from HaloBattery **1.13.0** (`a566a046`), with reviewed changes
through **1.15.0 and subsequent fixes** (`2251f05`). Halo Continued is not an official HaloBattery release.
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
[SDL controller researchers](https://github.com/libsdl-org/SDL),
[sc2-research](https://github.com/CouchTurtle/sc2-research),
[steambattery](https://github.com/eva-val/steambattery) and the
[original protocol contributors](https://github.com/HeyOkay/HaloBattery/tree/2251f05).
External protocol references are not runtime dependencies. Portable packages
retain notices for the libraries they distribute.
