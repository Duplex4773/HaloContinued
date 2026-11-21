# Unreleased changes since v1.1.8

Provisional changelog for the `main` branch.

## Device identity and recovery

- Game boost and Battery saver now remember the mouse's Windows device identity separately. If either was enabled before this change, turn it on once on your usual wireless connection.
- Changing polling or sleep settings on a replacement mouse preserves the original mouse’s recovery settings, even when both report the same serial. A manual change during USB charging can override the wireless restore only on the identified cable connection. Switching to another cable-connected mouse preserves the original restore.
- Startup polling restoration now checks the Windows device identity saved when you applied the rate. If you saved a mouse rate in v1.1.8, apply it once on your usual connection to enable restoration for that connection.
- A manual rate change on a replacement receiver leaves the original mouse’s queued startup restore intact. After waking Windows or re-enabling polling controls, Halo can check the current rate again without reapplying the startup setting.
- When older Game boost or Battery saver settings need confirmation, Devices shows the saved rate, threshold and sleep time for review. Unsupported choices are marked unavailable. Settings already bound to a different mouse stay separate.
- Razer mice that report the same serial keep separate history and tray icons after a restart, even when only one is plugged in. A replacement Logitech G535 receiver with a different HID serial also gets its own battery history.
- Logitech receivers recheck which device occupies each slot. If one receiver or headset battery interface stops responding, Halo Continued keeps its last reading marked stale while other devices continue updating. Game boost and Battery saver can resume restoration when the matching mouse responds again.
- Busy Logitech receivers can now report battery levels even when unrelated messages arrive ahead of the reply. Halo reports a connection error if the required receiver interface is missing. A silent replacement G535 receiver no longer shows the previous headset’s sleeping battery level.
- Disabling Bluetooth monitoring releases its connections. Enabling it again starts fresh discovery. Sleeping devices can retain their last known percentage, and the tray only shows charging when the connection confirms it.

## History, Insights and alerts

- Both Calendar time and Time used keep the last confirmed point of a flat stretch before drawing a battery drop on a dense graph. Calendar history also preserves sleep and wake transitions when fitting many readings onto the screen.
- Time used now keeps both ends of each battery-level plateau when they fit on the graph. For example, a two-hour stretch at 80% remains visible between readings of 100% and 60%.
- History and battery-life estimates start again when a different physical device reuses a receiver slot. Insights excludes ambiguous boost activity when two sessions share a timestamp. Repeated checks of an unchanged Bluetooth reading no longer add usage evidence.
- Insights marks approximate battery percentages consistently with Devices. A late polling error from a previous connection can no longer clear the replacement mouse’s confirmed rate in Insights or interrupt its usage session. The dashboard and tray also discard late replies from a replaced device.
- Devices and the tray menu explain that hiding a device icon also mutes its battery alerts and pauses its time-left estimate.
- Devices sharing a receiver keep independent low-battery sound reminders. Settings also makes it clearer when notification pop-ups are disabled.

## Resource use and packaging

- Game boost skips Windows fullscreen checks while its configured mice are absent. Startup polling skips rebuilding the device list for missing mice and older rate choices with no confirmed owner. In a regression test, 100 startup eligibility checks made zero allocations.
- Worker events share a dashboard wake-up when they arrive together. In a test, 32 queued events needed one Windows message. Logitech also reuses its receiver slot information. In a separate test, 100 repeat slot-cache checks made zero allocations.
- Silent Logitech receivers use fewer battery-report reads while keeping the existing timeouts. In a simulated HID++ 1.0 slot test, the same 1.2-second timeout took 72 reads, down from 480.
- In a simulation with eight devices and one low battery, 100 refreshes made 1,800 allocations with sounds enabled, down from 14,100. The sound-disabled run also made 1,800.
- Settings creates the brand checkboxes when needed. Bursts of device updates share a Windows fullscreen check, and unchanged diagnostics avoid unnecessary allocations.
- The packaged README links to the matching version of the changelog and device catalog. Packaging tests check that local links resolve.

# Halo Continued 1.1.8

Released 20 November 2026 for Windows 11 x64.

## Device monitoring and dashboard

- Reconnects are picked up even during a burst of Windows device notifications. Bluetooth battery changes trigger a fresh check. If a connected device stops supplying a battery level, its last reading keeps its original timestamp.
- PlayStation and 8BitDo controllers now report communication errors when a read fails. The dashboard identifies a wired gamepad as USB when Windows confirms the connection type.
- Dashboard scrolling respects the Windows mouse-wheel setting, including scrolling by a page or disabling wheel scrolling.

## History and settings

- Calendar history starts a replacement device at its first known battery level. In the Time used view, the graph and its total now agree even if a reading changes while the graph is loading.
- Damaged battery estimates are preserved for recovery. Large settings files are saved in a compact format, and a save that exceeds the size limit leaves the previous settings intact.

## Performance

- Time used history updates its index as new readings arrive. With 42,000 simulated readings, loading a ten-minute chart after one new reading fell from 51.1 ms to 0.077 ms (median of seven runs).
- Idle PlayStation and 8BitDo controllers wait for incoming reports with fewer wakeups. In a simulated 400 ms idle interval, an 8BitDo controller needed 8 reads, down from 80.
- Routine device polls reuse the registered list of hardware vendors, avoiding a scan of the device catalog on each poll.

# Halo Continued 1.1.7

Released 17 November 2026 for Windows 11 x64.

## Device monitoring

- MCHOSE skips incompatible HID collections and waits before retrying silent, unlisted receivers. A reconnect allows an immediate retry.
- ASUS ROG OMNI receivers keep reporting responding devices when another paired device or receiver is unavailable.
- AJAZZ AK820 Max Ultra can find a working battery interface again if the one it last used stops responding.
- Routine Windows property updates no longer restart discovery for every provider. Actual connection changes still invalidate old readings and device-control requests.

## Battery estimates and History

- A cached ATK V75X reading no longer adds estimated usage each time the app checks the receiver.
- An oversized saved battery estimate survives a restart so the original data remains available for recovery.
- Recording one device's battery level no longer forces History to rescan another device's readings.
- Devices that report battery level without charging status now open History in Calendar time by default. You can still choose your preferred time scale.

## Startup and resource use

- Startup checks the notification icon using bounded memory, even if the existing icon file is unusually large.
- In three-minute simulated runs with the dashboard closed, the static tray icon averaged 0.017% of one logical CPU core and 4.72 MiB of private RAM. With charging animation enabled, the averages were 0.052% and 4.82 MiB. Results on physical hardware can vary.

# Halo Continued 1.1.6

Released 14 November 2026 for Windows 11 x64.

## New battery support

| Device | Connection | Hardware report |
| --- | --- | --- |
| ASUS ROG Harpe II Ace | SPEEDNOVA receiver `0B05:1AD0` | [#364](https://github.com/HeyOkay/HaloBattery/pull/364) |
| ASUS ROG Keris II Ace, ROG Azoth, ROG Strix Scope II 96 Wireless and ROG Strix Morph 96 Wireless | ROG OMNI receiver `0B05:1ACE` | [#231](https://github.com/HeyOkay/HaloBattery/issues/231), [#364](https://github.com/HeyOkay/HaloBattery/pull/364), [#340](https://github.com/HeyOkay/HaloBattery/issues/340) |
| Delux M900 Pro | Receiver `1D57:FA65`; identified by its passive battery report | [#407](https://github.com/HeyOkay/HaloBattery/pull/407) |
| EWEADN X23 Pro | Receiver `04F3:026F`; uses a feature read and separate input collection | [#406](https://github.com/HeyOkay/HaloBattery/pull/406) |
| MCHOSE K7 V2 Ultra+ | Receiver `3837:1018`, including the MagDock receiver | [#403](https://github.com/HeyOkay/HaloBattery/pull/403) |
| AJAZZ AK820 Max Ultra | Receiver `0C45:FEFC` with the matching product name and HID collection | [#256](https://github.com/HeyOkay/HaloBattery/issues/256) |
| AULA F108 Pro | Receiver `05AC:024F` with the matching product name and HID collection; battery level only | [#402](https://github.com/HeyOkay/HaloBattery/pull/402) |
| VGN N75 PRO | Receiver `320F:5088` with the matching product name; battery level only | [#407](https://github.com/HeyOkay/HaloBattery/pull/407) |
| Corsair Void Elite Wireless | Receiver `1B1C:0A55` | [#330](https://github.com/HeyOkay/HaloBattery/issues/330) |
| Turtle Beach Atlas Air | Receiver `10F5:225E` | [#310](https://github.com/HeyOkay/HaloBattery/issues/310) |
| BEITONG KP40D | Receiver `20BC:515C` | [#376](https://github.com/HeyOkay/HaloBattery/pull/376) |
| GameSir G7 Pro 8K Aimlabs | USB cable `3537:10C5` and receiver `3537:10C6` | [#390](https://github.com/HeyOkay/HaloBattery/pull/390) |

The BEITONG and GameSir readers require one Windows gamepad, one XInput slot and a unique matching HID collection to attribute the battery reading.

## Connection and compatibility updates

- **ATK V75X:** Add USB cable battery and charging monitoring on `320F:5055`. Prefer the cable when the cable and receiver share a trusted identity. On the receiver, reuse successful readings for five minutes and wait at least 30 seconds before retrying failures. [#368](https://github.com/HeyOkay/HaloBattery/pull/368)
- **Poly Voyager 8200 UC:** Add BT600 adapter revision `047F:02EE`. [HaloBattery 1.17.0](https://github.com/HeyOkay/HaloBattery/releases/tag/v1.17.0)
- **SteelSeries Arctis Pro Wireless:** Add an alternate HID report format for base stations on `1038:1290` that do not respond to the original format. [#389](https://github.com/HeyOkay/HaloBattery/pull/389)

Hardware confirmation comes from the linked HaloBattery reports. Halo Continued tested the Rust readers with simulated transports. Thanks to HeyOkay, the HaloBattery contributors and the device owners who shared reports and captures.

# Halo Continued 1.1.5

Released 14 November 2026 for Windows 11 x64.

## Performance

- Skip charging-animation timer wakeups while consecutive frames show the same image. Preserve the original animation timing and use one shared timer across devices, with no extra frame allocations or hardware queries.
- Avoid rebuilding identical dashboard snapshots for devices whose disconnected icons are already retained. Reconnects, changed errors and device removal still update normally.

## Insights and device controls

- Keep Battery saver usage attributed correctly when Game boost restores the saver rate. Count the restoration without recording another saver activation.
- Preserve rate attribution across Refresh, failed reads, suspend and restart only when fresh readback confirms the same device, connection and owned temporary setting. Ignore obsolete read completions without discarding newer evidence.
- Show fullscreen boost availability from the current connection's supported rates, and update the device information page after a rate read without requerying firmware.

## Reliability and security

- Retain failed battery-notification deliveries when the runtime queue is temporarily full, and retry through existing UI events. Retries are bounded, coalesced and checked for relevance before delivery; no background timer is added.
- Preserve earlier damaged-settings backups when corruption happens again, instead of overwriting the original recovery file. Keep the source configuration if it cannot be safely moved.
- Update rusqlite to 0.41.1 and bundled SQLite to 3.54.1, addressing RUSTSEC-2026-0459. Update rustix to 1.1.6 and require Rust 1.89 or newer for source builds.
- Make dashboard resource checks account for Windows' initial theme caches, then check every subsequent reopen cycle for handle growth.

The installer remains unsigned and installs for the current Windows account without administrator privileges. Existing settings, history and reconnect safety checks are preserved. No new device support is added in this release.

Thanks to HeyOkay, the HaloBattery contributors and the community members who shared protocols, tests and hardware reports.

# Halo Continued 1.1.4

## Device monitoring

- Add battery monitoring for Corsair M75 Wireless, ATK X1 Pro, Endgame Gear XM2we, i-Rocks M31R, Glorious Model O Wireless, VAXEE E1 Wireless, Alienware Pro Wireless Gaming Keyboard, URX Core75, AULA F87 Pro, Sony INZONE H9, Poly Voyager 8200 UC and Razer Barracuda X (2022).
- Extend Logitech G535 monitoring with charge-register support. See the [device support table](README.md#battery-support-from-halobattery-forks) for connection details and hardware reports. The Rust readers were tested with simulated transports.
- Clear inferred Glorious charging when the cable is removed, and keep the original battery sample timestamp when the cable is detected without a fresh percentage.
- Keep receiver discovery and identity checks separate for devices with similar hardware, and retain cached metadata when a collection is temporarily inaccessible.

## Insights and dashboard

- Preserve Game boost and Battery saver usage attribution after Refresh when a fresh read confirms the same rate and hardware connection.
- Preserve that attribution through sleep while excluding sleep time from the usage estimate.
- Keep older Battery saver events in the normal-rate calculation while counting only recent changes in the activity summary.
- Read firmware information after Refresh updates the device connection state, so Refresh no longer cancels its own request.
- Keep the existing device polling schedule when editing appearance or device names.

# Halo Continued 1.1.3

Released 7 November 2026 for Windows 11 x64.

## Device monitoring

- Retry alternate eligible HID collections on a subsequent poll if the preferred collection becomes inaccessible.
- Cache an unavailable Jabra headset name for the current connection, so optional name requests cannot delay every battery poll.
- Clear an old BlackShark V3 charging flag if a fresh battery query does not return charging information.

## Dashboard and performance

- Preserve the History graph when only the window height changes. Reload samples when the graph width changes.
- Redraw History when the last device disappears.
- Avoid allocating copies of unchanged device captions and updating controls just before rebuilding the dashboard.

# Halo Continued 1.1.2

Released 7 November 2026 for Windows 11 x64.

## New battery support

| Device | Connection and USB identity | Upstream report |
| --- | --- | --- |
| Corsair HS80 RGB Wireless | USB, USB charging while powered off, and receiver (`1B1C:0A69`, `0A6A`, `0A6B`) | [#359](https://github.com/HeyOkay/HaloBattery/pull/359) |
| Cougar Revenger Pro 4K | Both receivers and USB (`3554:F5DF`, `F5DC`, `F5DE`) | [#354](https://github.com/HeyOkay/HaloBattery/pull/354) |
| EWEADN S9 | Receiver (`089D:062F`) | [#352](https://github.com/HeyOkay/HaloBattery/pull/352) |
| MCHOSE K87 | Receiver (`41E4:2001`) | [#352](https://github.com/HeyOkay/HaloBattery/pull/352) |
| MonsGeek FUN60 Pro | Receiver (`3151:5026`) | [#352](https://github.com/HeyOkay/HaloBattery/pull/352) |
| Razer BlackShark V3 | Receiver (`1532:057A`) | [#349](https://github.com/HeyOkay/HaloBattery/pull/349) |
| Jabra Evolve2 65 | Jabra Link 380 (`0B0E:24C7`) | [#337](https://github.com/HeyOkay/HaloBattery/pull/337) |
| Razer Viper V4 Pro | USB and receiver (`1532:00E5`, `00E6`) | [#363](https://github.com/HeyOkay/HaloBattery/pull/363) |
| VGN Dragonfly 4K | Receiver (`3554:F505`) | [#363](https://github.com/HeyOkay/HaloBattery/pull/363) |

The HS80 infers charging from USB presence and combines cable and receiver readings when their hardware identities match. The BlackShark V3 listens for pushed updates and limits active queries to protect its wireless link. The device additions are for battery monitoring. Hardware evidence comes from upstream reports, and local device tests used simulated HID.

## Dashboard and device controls

- Use the device workspace to see connected devices and their settings together.
- Check Windows **Enhance pointer precision** under **Windows mouse acceleration** and open Mouse Settings from the dashboard. The check is read-only and refreshes on UI events.
- Keep firmware and verification text readable, resize device rows after sleep/wake changes, and update both alert controls when a threshold is saved.
- Share confirmed polling rates and undo values between the tray and dashboard, including when a write finishes after the dashboard closes.
- Recover tray icons when Explorer restarts, and preserve an in-progress startup rate check when unrelated settings change.

## History, reliability and performance

- Account for longer configured polling intervals when calculating usage history and battery estimates. Preserve distinct polling events within the same second.
- Protect saved estimates and settings during temporary storage access failures, and cancel obsolete History queries promptly.
- Avoid temporary allocations for unchanged labels and skip snapshot construction when a full UI queue cannot accept it. Deliver the latest state once space is available and keep status exports current.

Thanks to HeyOkay, the HaloBattery contributors, and the community members who shared protocols and hardware reports.
