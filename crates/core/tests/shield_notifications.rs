use hb_core::*;
fn row(level: u8, charging: Option<bool>, time: i64) -> Reading {
    let mut r = Reading::new(
        "shield:board:SYNTHETIC",
        "NVIDIA SHIELD Controller",
        "shield",
        time,
    );
    r.level = Some(level);
    r.charging = charging;
    r.kind = "gamepad".into();
    r
}
fn engine() -> Engine {
    Engine::new(
        Settings {
            low_sound: true,
            ..Settings::default()
        },
        Estimator::default(),
    )
}

#[test]
fn unknown_shield_charging_does_not_raise_sound_or_alert_and_later_confirmation_does() {
    let mut e = engine();
    assert!(
        e.apply("shield", Ok(vec![row(19, None, 0)]), 0.0, false)
            .is_empty()
    );
    assert_eq!(e.take_low_battery_sound(), None);
    let n = e.apply("shield", Ok(vec![row(19, Some(false), 60)]), 60.0, false);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].kind, NotificationKind::Low);
    assert_eq!(e.take_low_battery_sound(), Some(19));
}
#[test]
fn unknown_charger_retains_held_alert_until_confirmation_or_recovery() {
    for recovered in [false, true] {
        let mut e = engine();
        e.apply("shield", Ok(vec![row(19, Some(false), 0)]), 0.0, true);
        assert!(e.has_held_notifications());
        assert!(
            e.apply("shield", Ok(vec![row(19, None, 60)]), 60.0, false)
                .is_empty()
        );
        assert!(e.has_held_notifications());
        let n = e.apply(
            "shield",
            Ok(vec![row(if recovered { 80 } else { 19 }, Some(false), 120)]),
            120.0,
            false,
        );
        assert_eq!(n.len(), usize::from(!recovered));
        assert!(!e.has_held_notifications());
    }
}
