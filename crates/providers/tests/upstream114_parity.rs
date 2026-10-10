use hb_providers::protocols as p;
use serde_json::{Value, json};
#[test]
fn separate_upstream114_parser_fixtures_match_external_reference() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("upstream114-fixtures.json")).unwrap();
    for (index, case) in cases.iter().enumerate() {
        let r: Vec<u8> = serde_json::from_value(case["data"].clone()).unwrap();
        let got = match case["parser"].as_str().unwrap() {
            "gwolves.parse_old" => p::gwolves_old(&r).map(|b| json!([b.level, b.charging])),
            "hyperx_cloud3s.parse" => p::hyperx3s(&r)
                .map(|(cmd, n)| json!([if cmd == 6 { "battery" } else { "charging" }, n])),
            "steelseries_elite.parse" => p::steelseries_elite(&r).map(|(b, state)| {
                if let Some(b) = b {
                    json!(["battery", b.level, b.charging])
                } else {
                    json!(["power", state, false])
                }
            }),
            "steelseries_elite.parse_reply" => p::steelseries_elite(&r)
                .map(|(b, state)| json!([b.as_ref().map(|b| b.level), r[15] == 2, state])),
            "logitech_centurion.parse_battery" => {
                p::centurion_battery(&r).map(|b| json!([b.level, b.charging]))
            }
            "logitech_centurion.parse_legacy" => {
                p::centurion_legacy(&r).map(|b| json!([b.level, b.charging]))
            }
            "logitech_centurion.payload_of" => p::centurion_payload(&r).map(|p| json!(p)),
            name => panic!("unknown delta parser {name}"),
        }
        .unwrap_or(Value::Null);
        assert_eq!(
            got, case["result"],
            "delta case {index}: {} {r:?}",
            case["parser"]
        );
    }
}
