//! Explicit hardware configuration, serialized by the application's HID owner.
use crate::{
    provider::{is_bluetooth, receiver_key, trusted_identity},
    razer_controls,
};
use hb_core::*;
use std::time::Duration;

/// Configuration routes are independent of the battery catalog. Adding battery
/// support must never grant permission to send configuration commands.
pub const POLLING_PROVIDERS: &[&str] = &["razer", "logitech", "mchose"];

/// Pure menu visibility hint for a battery mouse. This neither discovers a
/// capability nor authorizes commands: the controller must still resolve the
/// exact collection and verify connection capabilities when executing a request.
/// `name` is the provider's reading name, never the user's display-name override.
pub fn polling_menu_candidate(device: &ConfigurationDevice) -> bool {
    if device.kind != "mouse" || !matches!(device.capability, PollingCapability::ReadWrite) {
        return false;
    }
    if device.source == "simulation" {
        return device.via.is_empty() || device.via == "usb";
    }
    match device.source.as_str() {
        "razer" if device.via == "usb" => {
            let Some(rest) = device.key.strip_prefix("razer:") else {
                return false;
            };
            let Some((pid, identity)) = rest.split_once(':') else {
                return false;
            };
            // Mirrors the mouse-only PID scope in razer_controls::protocol.
            !identity.trim().is_empty()
                && matches!(pid, "00be" | "00bf" | "009f" | "00c1" | "00b6" | "00b7")
        }
        "logitech" if device.via.is_empty() || device.via == "usb" => {
            // The HID++ battery provider currently leaves `via` empty and keeps
            // the unit identity, but not the model pair, in ConfigurationDevice.
            // Exact provider names are only hints for an explicit guarded request.
            let Some(unit) = device.serial.as_deref().map(str::trim) else {
                return false;
            };
            unit.len() == 8
                && unit.bytes().all(|b| b.is_ascii_hexdigit())
                && unit.bytes().any(|b| b != b'0')
                && device.key.eq_ignore_ascii_case(&format!("logitech:{unit}"))
                && matches!(
                    device.name.trim().to_ascii_uppercase().as_str(),
                    "PRO X 2"
                        | "LOGITECH PRO X 2"
                        | "PRO X SUPERLIGHT 2"
                        | "LOGITECH G PRO X SUPERLIGHT 2"
                        | "PRO X 2 DEX"
                        | "LOGITECH PRO X 2 DEX"
                        | "PRO X SUPERLIGHT 2 DEX"
                        | "LOGITECH G PRO X SUPERLIGHT 2 DEX"
                        | "PRO X2 SUPERSTRIKE"
                        | "PRO X 2 SUPERSTRIKE"
                        | "LOGITECH PRO X2 SUPERSTRIKE"
                        | "LOGITECH G PRO X2 SUPERSTRIKE"
                )
        }
        "mchose" if device.via == "usb" => {
            device
                .key
                .strip_prefix("mchose:3837:")
                .is_some_and(|identity| !identity.trim().is_empty())
                && matches!(
                    device.name.trim().to_ascii_uppercase().as_str(),
                    "MCHOSE A7 V2 ULTRA" | "MCHOSE A7 V2 ULTRA+"
                )
        }
        _ => false,
    }
}

/// Possible menu choices from passive metadata, without HID discovery or I/O.
/// These hints are not observed supported rates: the selected model, advertised
/// mask, connection ceiling and control mode are verified during execution.
/// In particular, Logitech's stable unit key does not identify its receiver, so
/// a hint above that connection's ceiling may be refused without sending a SET.
pub fn polling_menu_rates(device: &ConfigurationDevice) -> &'static [u32] {
    if !polling_menu_candidate(device) {
        return &[];
    }
    match device.source.as_str() {
        "razer"
            if device.key.starts_with("razer:00b6:") || device.key.starts_with("razer:00b7:") =>
        {
            razer_controls::LEGACY_RATES
        }
        "razer" => razer_controls::EXTENDED_RATES,
        "mchose" => crate::mchose_controls::RATES,
        // Mirrors the possible indexed mask values in logitech_controls. The
        // selected connection may expose only a subset, which execution checks.
        "logitech" => &[125, 250, 500, 1000, 2000, 4000, 8000],
        "simulation" => &[125, 500, 1000, 2000, 4000, 8000],
        _ => &[],
    }
}

#[derive(Default)]
pub struct HidDeviceController;

/// Enforce the enumeration epoch and cancellation before and after every call,
/// including every write in a multi-packet operation.
pub(crate) struct GuardedSession<'a> {
    session: Box<dyn HidSession>,
    hid: &'a dyn HidTransport,
    context: &'a PollContext<'a>,
    generation: u64,
}
impl<'a> GuardedSession<'a> {
    pub(crate) fn new(
        session: Box<dyn HidSession>,
        hid: &'a dyn HidTransport,
        context: &'a PollContext<'a>,
        generation: u64,
    ) -> Self {
        Self {
            session,
            hid,
            context,
            generation,
        }
    }
    fn active(&self) -> Result<(), ProviderError> {
        if !self.context.active() {
            return Err(ProviderError::new(
                "configuration cancelled or deadline reached",
            ));
        }
        if self.hid.generation() != self.generation {
            return Err(ProviderError::new(
                "device connection changed; refresh configuration before applying",
            ));
        }
        Ok(())
    }
}
impl HidSession for GuardedSession<'_> {
    fn write(&mut self, d: &[u8]) -> Result<(), ProviderError> {
        self.active()?;
        let result = self.session.write(d);
        self.active()?;
        result
    }
    fn read(&mut self, n: usize, t: Duration) -> Result<Vec<u8>, ProviderError> {
        self.active()?;
        let result = self.session.read(n, t);
        self.active()?;
        result
    }
    fn send_feature(&mut self, d: &[u8]) -> Result<(), ProviderError> {
        self.active()?;
        let result = self.session.send_feature(d);
        self.active()?;
        result
    }
    fn feature(&mut self, id: u8, n: usize) -> Result<Vec<u8>, ProviderError> {
        self.active()?;
        let result = self.session.feature(id, n);
        self.active()?;
        result
    }
    fn input_report(&mut self, id: u8, n: usize) -> Result<Vec<u8>, ProviderError> {
        self.active()?;
        let result = self.session.input_report(id, n);
        self.active()?;
        result
    }
}
pub(crate) fn rate(hz: Option<u32>) -> Option<PollingRate> {
    hz.and_then(|h| PollingRate::try_from(h).ok())
}

impl DeviceController for HidDeviceController {
    fn execute(
        &mut self,
        request: &ControlRequest,
        hid: &dyn HidTransport,
        context: &PollContext<'_>,
    ) -> ControlOutcome {
        let reading = &request.target.device;
        if let PollingCapability::Unavailable(reason) = &reading.capability {
            return ControlOutcome::failed(request, reason.clone());
        }
        if reading.source == "logitech" {
            return crate::logitech_adapter::execute(request, hid, context);
        }
        if reading.source == "mchose" {
            return crate::mchose_adapter::execute(request, hid, context);
        }
        if reading.source != "razer" {
            return ControlOutcome::failed(
                request,
                "hardware polling controls are unavailable for this provider",
            );
        }
        if !reading.online() || !matches!(reading.kind.as_str(), "mouse" | "keyboard") {
            return ControlOutcome::failed(
                request,
                "select an online mouse or keyboard to configure",
            );
        }
        if reading.kind == "keyboard" && reading.via != "usb" {
            return ControlOutcome::failed(
                request,
                "keyboard polling controls require a wired USB device",
            );
        }
        if !context.active() {
            return ControlOutcome::failed(request, "configuration cancelled or deadline reached");
        }
        let generation = hid.generation();
        if request.action.rate().is_some() && request.target.generation != generation {
            return ControlOutcome::failed(
                request,
                "device connection changed; refresh configuration before applying",
            );
        }
        let devices = match hid.enumerate(0x1532) {
            Ok(d) => d,
            Err(e) => return ControlOutcome::failed(request, e.message),
        };
        let mut matching = devices.iter().filter(|info| {
            razer_controls::protocol(info).is_some_and(|protocol| {
                (protocol == razer_controls::Protocol::KeyboardExtended)
                    == (reading.kind == "keyboard")
                    && (reading.kind != "keyboard" || !is_bluetooth(&info.path))
            }) && reading.key == format!("razer:{:04x}:{}", info.product_id, trusted_identity(info))
        });
        let Some(info) = matching.next() else {
            return ControlOutcome::failed(
                request,
                "supported control collection for this exact device is unavailable",
            );
        };
        if matching.next().is_some() {
            return ControlOutcome::failed(
                request,
                "multiple control collections match this identity; refusing ambiguous target",
            );
        }
        // Display names are not identities. Preserve independently observed
        // serial/container values even when the stable key uses only one.
        if reading
            .serial
            .as_ref()
            .is_some_and(|s| !s.trim().eq_ignore_ascii_case(info.serial.trim()))
            || reading.container.as_ref().is_some_and(|c| {
                info.container
                    .as_ref()
                    .is_none_or(|i| !c.trim().eq_ignore_ascii_case(i.trim()))
            })
        {
            return ControlOutcome::failed(
                request,
                "device serial or container changed; refresh configuration before applying",
            );
        }
        if reading.kind == "keyboard"
            && devices.iter().any(|other| {
                other.vendor_id == info.vendor_id
                    && other.product_id == info.product_id
                    && trusted_identity(other) == trusted_identity(info)
                    && receiver_key(other) != receiver_key(info)
            })
        {
            return ControlOutcome::failed(
                request,
                "multiple physical devices share this identity; refusing ambiguous target",
            );
        }
        let protocol = razer_controls::protocol(info).unwrap();
        let requested = request.action.rate().map(|r| r.hz());
        if requested.is_some_and(|r| !protocol.rates().contains(&r)) {
            return ControlOutcome::failed(request, "polling rate is unsupported on this device");
        }
        if !context.active() || hid.generation() != generation {
            return ControlOutcome::failed(
                request,
                "device connection changed or configuration cancelled before opening",
            );
        }
        let session = match hid.open(info) {
            Ok(s) => s,
            Err(e) => return ControlOutcome::failed(request, e.message),
        };
        let mut guarded = GuardedSession::new(session, hid, context, generation);
        let result = razer_controls::execute_rate_checked(
            &mut guarded,
            context,
            protocol,
            requested,
            request.action.expected().map(|r| r.hz()),
        );
        let failure = result
            .failure
            .map(|f| format!("Razer polling configuration failed: {f:?}"));
        let observation = result.observed_hz.map(|hz| PollingObservation {
            target: ControlTarget {
                device: reading.clone(),
                generation,
            },
            supported: protocol
                .rates()
                .iter()
                .filter_map(|h| PollingRate::try_from(*h).ok())
                .collect(),
            rate: rate(Some(hz)),
            timestamp: context.clock.unix(),
            evidence: match protocol {
                _ if info.product_id == 0x00bf => {
                    "Hardware verified (user tested): wireless DeathAdder V4 Pro, all supported polling rates"
                }
                razer_controls::Protocol::Extended | razer_controls::Protocol::ExtendedWireless => {
                    "Razer high-rate polling; hardware unverified locally"
                }
                razer_controls::Protocol::KeyboardExtended => {
                    "Razer keyboard polling; hardware unverified locally"
                }
                razer_controls::Protocol::Legacy => {
                    "OpenMouse legacy polling reference; hardware unverified locally"
                }
            }
            .into(),
        });
        ControlOutcome {
            request: request.request,
            key: reading.key.clone(),
            observation,
            previous: rate(result.previous_hz),
            may_have_changed: result.may_have_changed,
            failure,
        }
    }
}
