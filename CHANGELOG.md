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
