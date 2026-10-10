//! Bind an explicitly selected receiver to the independently validated protocol.
use crate::{
    controls::{GuardedSession, rate},
    mchose_controls,
    provider::trusted_identity,
};
use hb_core::*;

pub(crate) fn execute(
    request: &ControlRequest,
    hid: &dyn HidTransport,
    context: &PollContext<'_>,
) -> ControlOutcome {
    let reading = &request.target.device;
    if reading.source != "mchose" || !reading.online() || reading.kind != "mouse" {
        return ControlOutcome::failed(request, "select an online MCHOSE mouse to configure");
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
    let devices = match hid.enumerate(0x3837) {
        Ok(devices) => devices,
        Err(error) => return ControlOutcome::failed(request, error.message),
    };
    let mut matches = devices.iter().filter(|info| {
        mchose_controls::candidate(info)
            && reading.key == format!("mchose:3837:{}", trusted_identity(info))
    });
    let Some(info) = matches.next() else {
        return ControlOutcome::failed(
            request,
            "supported MCHOSE control collection for this exact device is unavailable",
        );
    };
    if matches.next().is_some() {
        return ControlOutcome::failed(
            request,
            "multiple control collections match this identity; refusing ambiguous target",
        );
    }
    if reading
        .serial
        .as_ref()
        .is_some_and(|serial| !serial.trim().eq_ignore_ascii_case(info.serial.trim()))
        || reading.container.as_ref().is_some_and(|container| {
            info.container
                .as_ref()
                .is_none_or(|current| !container.trim().eq_ignore_ascii_case(current.trim()))
        })
    {
        return ControlOutcome::failed(
            request,
            "device serial or container changed; refresh configuration before applying",
        );
    }
    if !context.active() || hid.generation() != generation {
        return ControlOutcome::failed(
            request,
            "device connection changed or configuration cancelled before opening",
        );
    }
    let session = match hid.open(info) {
        Ok(session) => session,
        Err(error) => return ControlOutcome::failed(request, error.message),
    };
    let mut session = GuardedSession::new(session, hid, context, generation);
    let requested = request.action.rate().map(|r| r.hz());
    let result = mchose_controls::execute_rate_checked(
        &mut session,
        context,
        info,
        requested,
        request.action.expected().map(|r| r.hz()),
    );
    let observation = result.observed_hz.map(|hz| PollingObservation {
        target: ControlTarget {
            device: reading.clone(),
            generation,
        },
        supported: result
            .supported_hz
            .iter()
            .filter_map(|hz| PollingRate::try_from(*hz).ok())
            .collect(),
        rate: rate(Some(hz)),
        timestamp: context.clock.unix(),
        evidence: "MCHOSE A7 V2 Ultra+ reference protocol; hardware unverified locally".into(),
    });
    ControlOutcome {
        request: request.request,
        key: reading.key.clone(),
        observation,
        previous: rate(result.previous_hz),
        may_have_changed: result.may_have_changed,
        failure: result
            .failure
            .map(|failure| match failure {
                mchose_controls::ProtocolFailure::UnsupportedIdentity =>
                    "The linked mouse or firmware could not be verified. MCHOSE controls currently require A7 V2 Ultra+ firmware 5.46.2.4 on its supported wireless receiver.".into(),
                mchose_controls::ProtocolFailure::ConfigurationChanged
                | mchose_controls::ProtocolFailure::ProfileChanged
                | mchose_controls::ProtocolFailure::UnrelatedConfigurationChanged =>
                    "The active device configuration changed during the request; Refresh before applying again.".into(),
                failure => format!("MCHOSE polling configuration failed: {failure:?}"),
            }),
    }
}
