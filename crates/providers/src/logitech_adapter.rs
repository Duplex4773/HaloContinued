//! Resolve one proven HID++ unit, then delegate an explicit guarded transaction.
use crate::{
    controls::{GuardedSession, rate},
    logitech_controls::{self, Capabilities, ProtocolFailure},
    provider::receiver_key,
};
use hb_core::*;
use std::collections::BTreeMap;
fn failure_message(f: &ProtocolFailure) -> String {
    match f {
 ProtocolFailure::OnboardMode=>"Switch to software control mode in G HUB before changing the live polling rate; onboard profiles are preserved".into(),
 ProtocolFailure::UnknownMode=>"The device control mode could not be verified; polling changes were refused".into(),
 ProtocolFailure::IdentityMismatch=>"The paired mouse changed; refresh configuration before applying".into(),
 _=>format!("Logitech polling configuration failed: {f:?}"),
}
}
pub(crate) fn execute(
    request: &ControlRequest,
    hid: &dyn HidTransport,
    context: &PollContext<'_>,
) -> ControlOutcome {
    let reading = &request.target.device;
    if reading.source != "logitech" || !reading.online() || reading.kind != "mouse" {
        return ControlOutcome::failed(request, "select an online Logitech mouse to configure");
    }
    let Some(unit) = reading
        .serial
        .as_deref()
        .map(|s| s.trim().to_ascii_uppercase())
        .filter(|s| {
            s.len() == 8 && s.bytes().all(|b| b.is_ascii_hexdigit()) && s.bytes().any(|b| b != b'0')
        })
    else {
        return ControlOutcome::failed(
            request,
            "a verified HID++ hardware unit ID is required for polling controls",
        );
    };
    if !reading
        .key
        .eq_ignore_ascii_case(&format!("logitech:{unit}"))
    {
        return ControlOutcome::failed(
            request,
            "the selected reading does not have a verified HID++ unit identity",
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
    let devices = match hid.enumerate(0x046d) {
        Ok(d) => d,
        Err(e) => return ControlOutcome::failed(request, e.message),
    };
    let mut groups: BTreeMap<(u16, String), Vec<&HidInfo>> = BTreeMap::new();
    for info in &devices {
        if info.vendor_id == 0x046d
            && info.usage_page == 0xff00
            && [1, 2].contains(&info.usage)
            && matches!(info.product_id, 0xc54d | 0xc53a | 0xc09b | 0xc0a0 | 0xc0a8)
        {
            groups
                .entry((info.product_id, receiver_key(info)))
                .or_default()
                .push(info);
        }
    }
    struct Route<'a> {
        info: &'a HidInfo,
        capability: Capabilities,
        long: GuardedSession<'a>,
        short: Option<GuardedSession<'a>>,
    }
    let mut routes = Vec::new();
    let mut uncertainty = None;
    for entries in groups.values() {
        let long_entries: Vec<_> = entries
            .iter()
            .copied()
            .filter(|i| logitech_controls::candidate(i))
            .collect();
        if long_entries.is_empty() {
            continue;
        }
        if long_entries.len() != 1 {
            return ControlOutcome::failed(
                request,
                "multiple long control collections share one receiver identity; target is ambiguous",
            );
        }
        let info = long_entries[0];
        if reading.container.as_ref().is_some_and(|c| {
            info.container
                .as_ref()
                .is_none_or(|v| !c.trim().eq_ignore_ascii_case(v.trim()))
        }) {
            continue;
        }
        if !context.active() || hid.generation() != generation {
            return ControlOutcome::failed(
                request,
                "device connection changed or configuration cancelled before opening",
            );
        }
        let long = match hid.open(info) {
            Ok(s) => s,
            Err(e) => {
                uncertainty = Some(e.message);
                continue;
            }
        };
        let mut long = GuardedSession::new(long, hid, context, generation);
        let short_entries: Vec<_> = entries.iter().copied().filter(|i| i.usage == 1).collect();
        if short_entries.len() > 1 {
            return ControlOutcome::failed(
                request,
                "multiple short control collections share one receiver identity; target is ambiguous",
            );
        }
        let mut short = short_entries
            .first()
            .and_then(|i| hid.open(i).ok())
            .map(|s| GuardedSession::new(s, hid, context, generation));
        let slots: Vec<_> = if matches!(info.product_id, 0xc54d | 0xc53a) {
            (1..=6).collect()
        } else {
            vec![255]
        };
        let mut found = None;
        for slot in slots {
            let mut borrowed_short = short.as_mut().map(|s| s as &mut dyn HidSession);
            match logitech_controls::discover(
                &mut long,
                &mut borrowed_short,
                context,
                info,
                slot,
                &unit,
            ) {
                Ok(cap) => {
                    if found.is_some() {
                        return ControlOutcome::failed(
                            request,
                            "the same unit was reported by multiple paired slots; target is ambiguous",
                        );
                    }
                    found = Some(cap)
                }
                Err(
                    ProtocolFailure::IdentityMismatch
                    | ProtocolFailure::Unsupported
                    | ProtocolFailure::DeviceError(8),
                ) => {}
                Err(e) => {
                    uncertainty = Some(failure_message(&e));
                }
            }
        }
        if let Some(capability) = found {
            routes.push(Route {
                info,
                capability,
                long,
                short,
            });
        }
    }
    if routes.len() != 1 {
        return ControlOutcome::failed(
            request,
            if routes.is_empty() {
                uncertainty.unwrap_or_else(|| {
                    "no supported paired slot matches this exact mouse unit and container".into()
                })
            } else {
                "multiple receiver routes match this unit; refusing ambiguous target".into()
            },
        );
    }
    let mut route = routes.pop().unwrap();
    if let Some(error) = uncertainty {
        return ControlOutcome::failed(
            request,
            format!("Receiver identity could not be fully resolved: {error}"),
        );
    }
    let requested = request.action.rate().map(|r| r.hz());
    let previous = route.capability.observed_hz;
    let result = if requested.is_none() {
        logitech_controls::PollingResult {
            observed_hz: Some(previous),
            previous_hz: route
                .capability
                .supported_hz
                .contains(&previous)
                .then_some(previous),
            software_mode: route.capability.onboard_mode == Some(2),
            may_have_changed: false,
            failure: None,
        }
    } else {
        let mut short = route.short.as_mut().map(|s| s as &mut dyn HidSession);
        logitech_controls::execute_rate_checked(
            &mut route.long,
            &mut short,
            context,
            route.info,
            &route.capability,
            requested,
            request.action.expected().map(|r| r.hz()),
        )
    };
    let mode_verified = result.software_mode;
    let mode_failure = if mode_verified {
        None
    } else {
        Some(failure_message(
            &if route.capability.onboard_mode == Some(1) {
                ProtocolFailure::OnboardMode
            } else {
                ProtocolFailure::UnknownMode
            },
        ))
    };
    let observation = result.observed_hz.map(|hz| PollingObservation {
        target: ControlTarget { device: reading.clone(), generation },
        supported: route.capability.supported_hz.iter().filter_map(|h| PollingRate::try_from(*h).ok()).collect(),
        rate: if mode_verified {rate(Some(hz))} else {None},
        timestamp: context.clock.unix(),
        evidence: if mode_verified {
            "Solaar HID++ 8061; software-control configured rate readback, hardware unverified locally".into()
        } else {format!("Onboard or unknown mode; effective polling rate unavailable; software-control rate {hz} Hz")},
    });
    ControlOutcome {
        request: request.request,
        key: reading.key.clone(),
        observation,
        previous: if mode_verified {
            rate(result.previous_hz)
        } else {
            None
        },
        may_have_changed: result.may_have_changed,
        failure: result
            .failure
            .as_ref()
            .map(failure_message)
            .or(mode_failure),
    }
}
