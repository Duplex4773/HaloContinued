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
