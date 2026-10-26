# Halo Continued

**Battery monitoring, device controls and battery-life insights for Windows.**

**Performance goal:** keep CPU and RAM use as low as practical while adding
device controls, history and battery insights. The aim is no noticeable
performance compromise during work or gaming. The app still consumes resources;
usage varies with connected devices, settings and dashboard activity.

**Added controls and insights, with low measured background resource use:**

- **Extra device controls:** change polling rates, mouse sensitivity (DPI) and
  hardware sleep timers on supported devices. Opt into fullscreen polling boosts
  or low-battery power saving, and explore 30-day history and battery-life
  estimates that account for changing polling rates in the native dashboard.
- **Substantially lower measured RAM:** version 1.0.4 used **4.58–4.60 MiB mean
  private memory**, compared with **33.99–34.31 MiB** for upstream HaloBattery
  in the matched tests—approximately **86–87% less**.
- **Very low measured CPU overhead:** version 1.0.4 recorded **0.009% of one
  logical core with animation off** and **0.069% with charging animation on**.
  Upstream recorded **0.122% and 0.148%**, respectively.

**Test scope:** these are single three-minute **simulated source-harness samples**
with one charging mouse, fresh data and the dashboard closed. Optional hardware
controls were disabled, so the figures do not measure the cost of actively using
those controls. They are not distributed-executable or live-hardware benchmarks,
and do not measure game frame rates or input latency. Small CPU differences can
reflect counter resolution or run-to-run variation; actual usage varies.
[See the full comparison, settings and limitations](#matched-version-comparison-18-october-2026).

Local build: **1.1.0**, with a native device overview, device cards, focused
control tabs and a refreshed tray menu. The redesign adds no background timers
or device queries. Device info reads firmware only when opened or refreshed.
This build also preserves rapid history transitions, combines repeated graph
requests, avoids connection checks for Bluetooth devices with no battery data,
and records battery-saver changes separately in Insights. Short Time used graphs
reuse a bounded timing index, released when History closes. Existing history is
upgraded automatically without changing its recorded timestamps. This build has
not been published yet. Rapid readings remain ordered on the graph, same-second
rate changes retain their battery session, and selected device cards respect
Windows high-contrast colours.

Latest published release: **1.0.10** for Windows 11 x64, available as an unsigned per-user
installer and a portable ZIP.

| Download | Use it for |
| --- | --- |
| [**V1.0.10 installer**](https://github.com/Duplex4773/HaloContinued/releases/download/v1.0.10/HaloContinued-1.0.10-windows-x64-setup.exe) | Install for your Windows account, with a Start menu shortcut and optional startup at sign-in. No administrator privileges required. |
| [**V1.0.10 portable ZIP**](https://github.com/Duplex4773/HaloContinued/releases/download/v1.0.10/HaloContinued-1.0.10-windows-x64.zip) | Extract the ZIP and run `HaloContinued.exe`. Includes licences and notices. |

[SHA-256 checksums](https://github.com/Duplex4773/HaloContinued/releases/download/v1.0.10/HaloContinued-1.0.10-SHA256SUMS.txt)
· [Release notes](https://github.com/Duplex4773/HaloContinued/releases/tag/v1.0.10)

Both downloads are unsigned. Windows may show an unrecognised-app warning.

**New in 1.0.10:** Waiting battery alerts avoid repeated memory allocations while
a device is unavailable, and battery-life learning avoids creating empty records
from unusable first readings. Tray menus now display device names containing `&`
correctly, including renamed devices. These changes add no background workers,
timers or battery checks.
[Full changelog](https://github.com/Duplex4773/HaloContinued/releases/tag/v1.0.10).

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
| Device information | View the connection, supported controls and verification status. Firmware is read on demand for reviewed modern Razer mice, with no background firmware polling. |
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
and Halo Continued 1.0.4:

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

Battery monitoring covers **41 provider families and 368 hardware IDs** across supported mice,
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

Additional battery support from **hardware-confirmed parent pull requests**, reviewed
25 October 2026. The reports confirm the connections listed here; the Rust implementations
are tested with simulated transports, not newly certified on all of this hardware.
The exact source revisions are retained in the [reviewed device catalog](crates/providers/reviewed-devices.json).

| Device | Enabled connections and limits | Parent reports |
| --- | --- | --- |
| NVIDIA SHIELD Controller (2017) | `0955:7214`, USB and Bluetooth; board identity joins proven transport aliases and prefers USB. Unknown charging stays unknown; alerts wait for confirmation. | [#250](https://github.com/HeyOkay/HaloBattery/pull/250) |
| Razer Pro Type Ultra; Barracuda Pro receiver revision | `1532:027B` keyboard receiver, with corrected transaction fallback and sleep recovery; `1532:0583` headset receiver uses its PA battery exchange. | [#203](https://github.com/HeyOkay/HaloBattery/pull/203), [#315](https://github.com/HeyOkay/HaloBattery/pull/315) |
| Redragon M991 / K1NG Max; Scyrox mice | Redragon `3554:F5D5` receiver / `F55E` cable; Scyrox `3554:F5F7` receiver / `F5F6` cable. Shared receiver names avoid claiming a specific unidentified mouse. | [#200](https://github.com/HeyOkay/HaloBattery/pull/200), [#182](https://github.com/HeyOkay/HaloBattery/pull/182) |
| Hitscan Hyperlight; ATK A9 Ultimate | Hitscan 8K receiver `3770:0300`; A9 receiver `373B:11D9`. The untested A9 cable is excluded. | [#158](https://github.com/HeyOkay/HaloBattery/pull/158), [#305](https://github.com/HeyOkay/HaloBattery/pull/305) |
| G-Wolves shared 8K receiver | Existing ID `33E4:3854` gains the confirmed Compx path when its 17-byte output collection is present and no legacy 65-byte feature collection is available. Existing feature-report units retain their protocol. | [#317](https://github.com/HeyOkay/HaloBattery/pull/317) |
| Pulsar X2 V3 Mini | `3710:5403` 8K receiver, using its separate Sonix feature report. The unverified cable ID is excluded; charging is unknown. | [#213](https://github.com/HeyOkay/HaloBattery/pull/213) |
| Finalmouse UltralightX | Receiver `361D:0100`; battery and charging. Voltage-based levels are labelled approximate and may rise while charging, matching the manufacturer's curve. | [#284](https://github.com/HeyOkay/HaloBattery/pull/284) |
| ATK X1 Ultimate Nearlink; V75X | X1 `373B:108C` receiver / `108B` cable; V75X `320F:5088` receiver requires the exact product name because the ID is shared. | [#279](https://github.com/HeyOkay/HaloBattery/pull/279), [#308](https://github.com/HeyOkay/HaloBattery/pull/308) |
| Inphic In9 Pro; Attack Shark X11 / R1 / X3 Max | Receivers `1D57:FA65` / `FA60`; passive reads send no commands. R1 levels are coarse, with no charging field. | [#181](https://github.com/HeyOkay/HaloBattery/pull/181) |
| Rapoo VT7 Gen-2; V700DIY-98 | Mouse `24AE:1413` / `4613`; keyboard `24AE:1924` / `4824`. Passive battery and charging reports. | [#215](https://github.com/HeyOkay/HaloBattery/pull/215) |
| MCHOSE V9 Pro; V9 Turbo+ | Pro `291D:385D`; Turbo+ receiver `3837:6008` / headset `600A`. Turbo+ charging is unavailable, so no charging state is invented. | [#248](https://github.com/HeyOkay/HaloBattery/pull/248), [#215](https://github.com/HeyOkay/HaloBattery/pull/215) |
| Lofree Flow Lite84; AULA V9 Max+ / F87 Pro V2C | Flow receiver `05AC:024F` requires its vendor collection; ordinary Apple keyboards sharing the ID are untouched. AULA mouse `320F:22A0` / `22B7`; keyboard receiver `0C45:FEFC`. No battery support is claimed for the keyboard's cable. | [#169](https://github.com/HeyOkay/HaloBattery/pull/169), [#327](https://github.com/HeyOkay/HaloBattery/pull/327) |
| ASUS TX Gaming Mouse Mini Miku; Strix Go 2.4 | Mouse `0B05:1C5A`; headset receiver `0B05:18D6`. The headset reports voltage-derived bands, not an exact percentage. Its untested cable is excluded. | [#205](https://github.com/HeyOkay/HaloBattery/pull/205), [#296](https://github.com/HeyOkay/HaloBattery/pull/296) |
| ASUS ROG OMNI | `0B05:1ACE`; reads the pair list before querying the confirmed Harpe Ace Mini and Falchion RX Low Profile. Other paired models are not queried. | [#185](https://github.com/HeyOkay/HaloBattery/pull/185) |
| HyperX Cloud II Wireless, Kingston revision | `0951:1718`, its own 62-byte battery and charging exchange. | [#198](https://github.com/HeyOkay/HaloBattery/pull/198) |
| Corsair K65 PLUS Wireless; Void v2 family | K65 receiver `1B1C:2B07`, battery only. Existing Void v2 / Virtuoso Max / HS80 Max support gains the Windows 64-byte framing fix with a refused-write fallback. | [#293](https://github.com/HeyOkay/HaloBattery/pull/293), [#176](https://github.com/HeyOkay/HaloBattery/pull/176) |
| Turtle Beach Stealth Pro II; Stealth 700 Gen 3 | Pro II `10F5:229B` transmitter / `229E` cable; 700X transmitter `2251` / headset cable `2253`. The 700's charging status is unknown; unconfirmed transmitter `2250` is excluded. | [#194](https://github.com/HeyOkay/HaloBattery/pull/194), [#211](https://github.com/HeyOkay/HaloBattery/pull/211) |
| Skullcandy PLYR / Crusher PLYR 720 | Receivers `34F0:3210` / `5310`; battery only. SLYR Pro remains excluded. | [#207](https://github.com/HeyOkay/HaloBattery/pull/207) |
| DAREU A950; AJAZZ AJ179 V2 MAX | DAREU receiver `260D:1074`; charging stays unknown because its owner saw the same flag on and off the cable. AJAZZ `249A:5C2F` receiver / `248A:5D2E` cable; a known firmware quirk can return a stored level after reboot until replugged. | [#212](https://github.com/HeyOkay/HaloBattery/pull/212), [#246](https://github.com/HeyOkay/HaloBattery/pull/246) |
| FlyDigi Vader 5 Pro | `37D7:2401`, coarse battery and charging on its vendor channel, attributed only to a matching Windows controller. Ambiguous controllers are not queried. | [#216](https://github.com/HeyOkay/HaloBattery/pull/216) |

Already supported proposals were checked before adding identities: Logitech unit-ID
deduplication and SteelSeries Nova 3P use existing implementations; the latter's
[hardware confirmation](https://github.com/HeyOkay/HaloBattery/pull/175) is retained.
The Maxwell Xbox receiver's off-state name is also recognized
([#254](https://github.com/HeyOkay/HaloBattery/pull/254)). Proposals explicitly awaiting
hardware confirmation, including Viper V3 Pro SE and HyperX Cloud Alpha Wireless,
are not enabled by this review. Sony INZONE H9's proposed transport also needs
protocol evidence before it can be included.

After a slow Windows boot, an empty inventory is retried every ten seconds for
up to ninety seconds. Sleeping Pulsar-family mice use Windows' last-input timestamp
to request a targeted battery refresh, at most once every five seconds and outside
quiet gaming. Input contents are never read or saved. Silent WLmouse receivers
back off to every fifth scheduled poll after their initial attempts, with immediate
recovery when their collection inventory changes.

Fullscreen boost is off by default and enabled per mouse. Windows is checked
every five seconds when needed; boosting requires ten seconds of stable
fullscreen gaming, and restoration waits fifteen seconds on the desktop. Some
borderless games are missed.

Refresh and changes to other devices preserve verified original polling rates
and sleep timers. The app records originals before automatic changes, so it can
also restore them after an app restart or an interrupted session. It checks the
device identity and current setting before restoring. If Windows gives a device
a new connection identifier, recovery also requires a matching hardware serial
and Windows container, plus a fresh reading. Ambiguous devices, unavailable
proof and settings changed elsewhere are left untouched. An interrupted manual
change takes priority and may need to be checked by the user. Recovery uses
normal device events and the existing storage worker; it adds no background scan
or timer. Changes made by older builds have no saved recovery record.

With device settings enabled, launch also checks supported mice's current polling
rates in the background. Receivers have up to five minutes to become available;
failed reads get up to three attempts, at least fifteen seconds apart. A confirmed
read stops the checks. Saved-rate restoration remains one-shot: if it fails, the
app can read the current rate without repeating the change. After startup, a fresh
on-battery reading can restart these bounded, read-only checks if waking, Refresh
or a connection change left the rate unknown. Checks yield to device actions and
fullscreen boost; sleeping or charging mice are not queried for Insights recovery.
No continuous polling-rate checks or additional background timer are used.

Low-battery mode is also opt-in. It only lowers a faster polling rate or shortens
a longer sleep timer. Boost takes priority over its polling rate. Prior settings
return when charging or when battery recovers five percentage points above the
threshold. The mode stays saved but inactive on a recognized wired connection.
Manual changes take priority; uncertain writes are not automatically repeated.

On Devices, select **Device info** to read the reported firmware version. The
read-only query also works when device-setting changes are disabled. Use
**Refresh information** after a firmware update or connection change. Other brands
and unsupported Razer connections show that firmware reading is unavailable.
The standard query does not distinguish separate mouse and receiver versions;
the page labels the reported version and this limitation rather than inventing
a second version. Verification labels describe tested controls and connections;
a successful firmware response does not certify every feature. Reads are
cancelled on leaving the view, closing it, reconnecting or suspending Windows.

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
until sufficient evidence is available. If missing rate information blocks an
estimate, Insights shows the confirmed percentage and explains how checks resume.
Unknown periods in older history are not filled using saved rate preferences.

## Resource use

A native executable avoids the bundled Python runtime. Bounded workers, cached
HID discovery and tray graphics, event-driven waits and batched SQLite writes
limit background work. Charging frames are pre-rendered; the animation timer
stops when no icons need it. Dashboard graphics are released when it closes,
and hovering an icon does not query hardware. The standard executable is about
**3.32 MiB**, including HIDAPI, SQLite and the MSVC runtime.

### Matched version comparison, 18 October 2026

This comparison uses **current upstream HaloBattery 1.15.0 source**
([`2251f05`](https://github.com/HeyOkay/HaloBattery/commit/2251f0534a8e886c9692ead42056059b4f69d8c0))
and the **1.0.0–1.0.4 release tags** of Halo Continued. Each case has one
**three-minute sample after a ten-second warm-up**, with fresh data, one simulated
mouse at 73% and charging, a 60-second refresh interval and the dashboard never
opened. The two passes differ only in whether charging animation is enabled.

| Source version | CPU: animation off | CPU: animation on | Mean RAM: animation off | Mean RAM: animation on |
| --- | ---: | ---: | ---: | ---: |
| HaloBattery 1.15.0 (`2251f05`) | 0.122% | 0.148% | 33.99 MiB | 34.31 MiB |
| Halo Continued 1.0.0 | 0.009% | 0.052% | 4.50 MiB | 4.57 MiB |
| Halo Continued 1.0.1 | 0.009% | 0.052% | 4.72 MiB | 4.64 MiB |
| Halo Continued 1.0.2 | 0.026% | 0.104% | 4.46 MiB | 4.52 MiB |
| Halo Continued 1.0.3 | 0.026% | 0.061% | 4.58 MiB | 4.67 MiB |
| Halo Continued 1.0.4 | 0.009% | 0.069% | 4.60 MiB | 4.58 MiB |

CPU is kernel plus user time for the application process tree, expressed as
**a percentage of one logical core**. RAM is **mean private committed memory**,
not working set. Sampled peak private memory for Halo Continued 1.0.4 was
4.73 MiB with animation off and 4.71 MiB with it on. It used approximately
86–87% less mean private memory than upstream in these two samples.

All cases ran sequentially on Windows 11 (build 26300), a Ryzen 9 9950X3D,
restricted to the same logical core. The animation-on pass reversed the version
order. Rust used the same **1.98.0 compiler, standard x64 settings and release
profile** for every tag; AVX2/AVX512 builds were not used. Upstream ran under
**Python 3.13.15**, with hidapi 0.15.0, pystray 0.19.5 and Pillow 12.3.0, using
`pythonw.exe` directly so no virtual-environment redirectors or console host were
included. Notifications, Bluetooth, status export, automatic updates and optional
hardware controls were disabled. Real hardware battery readings were replaced
by the simulated device; no physical-device commands were sent.

These are **source-harness comparisons, not distributed-binary or live-hardware
benchmarks**. Rust used release-optimised native test harnesses running its
production monitoring and tray code; upstream used its actual Python application
loop. Startup, dashboard use, populated history, multiple devices, game frame
rates and input latency were not measured. Other desktop activity was not
controlled or included in the application counters.

Each table cell shows **one primary sample**. Small CPU or RAM differences between
Rust releases can reflect Windows accounting resolution or run-to-run variation;
the table does not establish a sustained speedup or a reliable ranking. Earlier
measurements below used a different Python launch path; use this matched series
for comparisons between the listed versions.

**Test cleanup observation:** upstream's animation-on harness completed its full
measurement window but timed out during normal exit afterward, on both the first
run and a verification repeat. Only those test processes were terminated, outside
the measured window. The table retains the first result; the repeat measured
0.122% CPU and 33.81 MiB mean private memory. A separate check confirmed that the
simulated charging icon was visible and animating. The shutdown cause was not
diagnosed, so this is a harness observation, not a confirmed product defect.

### Earlier measurements

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
.\tools\package-rust.ps1 -Version 1.1.0
```

The executable is `target/x86_64-pc-windows-msvc/release/HaloContinued.exe`.
The portable ZIP is `dist/HaloContinued-1.1.0-windows-x64.zip`. Release builds use
size optimisation, link-time optimisation, static runtime linking and stripped
symbols; local compiler paths are removed. Packages include dependency notices
and licences. Optional API/import validation uses `tools/validate-configuration.ps1`
with ripgrep and Visual Studio's dumpbin installed.

To also build the unsigned installer, install [Inno Setup](https://jrsoftware.org/isinfo.php)
6.3 or newer, then run:

```powershell
.\tools\package-installer.ps1 -Version 1.1.0
# For a portable compiler installation, add -Iscc 'path\to\ISCC.exe'.
```

This produces the portable ZIP, `dist/HaloContinued-1.1.0-windows-x64-setup.exe`,
the standalone `dist/HaloContinued-1.1.0-windows-x64.exe` and
`dist/HaloContinued-1.1.0-SHA256SUMS.txt`. The installer uses the same app and
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
[Headroom](https://github.com/patalbansishashank/Headroom),
[MousePower](https://github.com/vamyane/MousePower),
[the ULX battery implementation by voiddog30](https://github.com/voiddog30/HaloBattery),
[the Pulsar Sonix reference](https://github.com/jonkristian/pulsar-x3-python),
[the MCHOSE V9 reference](https://github.com/JoaoKSS/MCHOSE_v9_PRO_Controller),
[Linux-Arctis-Manager](https://github.com/elegos/Linux-Arctis-Manager),
[Arctis-Sound-Manager](https://github.com/loteran/Arctis-Sound-Manager) and
[hitscan-battery](https://github.com/sopparus/hitscan-battery), along with the
[SDL controller researchers](https://github.com/libsdl-org/SDL),
[sc2-research](https://github.com/CouchTurtle/sc2-research),
[steambattery](https://github.com/eva-val/steambattery) and the
[original protocol contributors](https://github.com/HeyOkay/HaloBattery/tree/2251f05).
External protocol references are not runtime dependencies. Portable packages
retain notices for the libraries they distribute.
