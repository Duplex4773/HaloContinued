use hb_providers::protocols as p;
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    parser: String,
    data: Vec<u8>,
    level: Option<u8>,
    charging: Option<serde_json::Value>,
    label: Option<String>,
}

#[test]
fn python_reference_parser_fixtures() {
    let cases: Vec<Case> = serde_json::from_str(include_str!("parser-fixtures.json")).unwrap();
    for (index, case) in cases.iter().enumerate() {
        let r = &case.data;
        let got = match case.parser.as_str() {
            "ds4" => Some(p::ds4(r[0])),
            "dualsense" => Some(p::dualsense(r[0])),
            "eightbitdo" => p::eightbitdo(r[0]),
            "nintendo" => p::nintendo(r[0]),
            "voltage" => p::battery(
                p::voltage_to_percent(u16::from_be_bytes([r[0], r[1]])),
                None,
            ),
            "wl_feature" => p::wl_feature(r),
            "wl_heartbeat" => p::wl_heartbeat(r),
            "pulsar" => p::pulsar(r),
            "mchose" => p::mchose(r),
            "mchose_g7" => p::mchose_g7(r),
            "astro" => p::astro(r),
            "audeze" => p::audeze(r),
            "asus1" => p::asus(r, 1),
            "asus25" => p::asus(r, 25),
            "hyperx" => p::hyperx(r),
            "hyperx3" => p::hyperx3(r),
            "hyperx_alpha" => p::hyperx_alpha(r),
            "keychron" => p::keychron(r),
            "jbl" => p::jbl(r),
            "am_infinity" => p::am_infinity(r),
            "corsair" => p::corsair(r),
            "corsair_nxp" => p::corsair_nxp(r),
            "logitech" => p::logitech(u16::from_be_bytes([r[0], r[1]]), &r[2..]),
            name => p::steelseries(r, name),
        };
        assert_eq!(
            got.as_ref().map(|b| b.level),
            case.level,
            "fixture {index}: {} {r:?}",
            case.parser
        );
        if case.level.is_some()
            && let Some(label) = &case.label
        {
            assert_eq!(
                got.as_ref().unwrap().label.as_deref().unwrap_or(""),
                label,
                "label fixture {index}: {} {r:?}",
                case.parser
            );
        }
        if case.level.is_some()
            && let Some(expected) = &case.charging
        {
            let expected = expected
                .as_bool()
                .unwrap_or_else(|| expected.as_u64().unwrap() != 0);
            assert_eq!(
                got.unwrap().charging,
                Some(expected),
                "charging fixture {index}: {} {r:?}",
                case.parser
            );
        }
    }
}

#[test]
fn request_sizes_and_checksum() {
    for tid in [0x1f, 0x3f, 0xff, 0x9f, 8] {
        for command in [0x80, 0x84] {
            let packet = p::razer_request(tid, command);
            assert_eq!(packet.len(), 91);
            assert_eq!(packet[2], tid);
            assert_eq!(packet[89], packet[3..89].iter().fold(0, |a, b| a ^ b));
        }
    }
    assert_eq!(
        p::pulsar_request()
            .iter()
            .fold(0u8, |a, b| a.wrapping_add(*b)),
        0x55
    );
}

#[test]
fn python_reference_pa_reply_shapes() {
    #[derive(Deserialize)]
    struct Reply {
        barracuda: bool,
        command: u8,
        data: Vec<u8>,
        payload: Option<Vec<u8>>,
    }
    let cases: Vec<Reply> = serde_json::from_str(include_str!("pa-fixtures.json")).unwrap();
    for (index, case) in cases.iter().enumerate() {
        assert_eq!(
            p::pa_reply(&case.data, case.barracuda, case.command).map(|r| r.to_vec()),
            case.payload,
            "PA case {index} {:?}",
            case.data
        );
    }
}

#[test]
fn complete_generated_catalog_matches_python_tables() {
    let expected: Vec<(String, u16, u16, String, String, u8)> =
        serde_json::from_str(include_str!("catalog-fixtures.json")).unwrap();
    let actual: Vec<_> = hb_providers::catalog::DEVICES
        .iter()
        .map(|d| {
            (
                d.provider.into(),
                d.vid,
                d.pid,
                d.name.into(),
                d.variant.into(),
                d.parameter,
            )
        })
        .collect();
    assert_eq!(actual, expected);
}
