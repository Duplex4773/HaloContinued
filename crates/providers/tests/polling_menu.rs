use hb_core::{ConfigurationDevice, PollingCapability, Reading};
use hb_providers::controls::{polling_menu_candidate, polling_menu_rates};

#[test]
fn passive_rate_hints_match_model_protocols_without_authorizing_support() {
    for pid in ["00b6", "00b7"] {
        assert_eq!(
            polling_menu_rates(&mouse("razer", &format!("razer:{pid}:unit"), "Mouse")),
            &[125, 500, 1000]
        );
    }
    for pid in ["00be", "00bf", "009f", "00c1"] {
        assert_eq!(
            polling_menu_rates(&mouse("razer", &format!("razer:{pid}:unit"), "Mouse")),
            &[125, 500, 1000, 2000, 4000, 8000]
        );
    }
    assert_eq!(
        polling_menu_rates(&mouse("mchose", "mchose:3837:unit", "MCHOSE A7 V2 Ultra+")),
        &[125, 500, 1000, 2000, 4000, 8000]
    );
    let mut logitech = mouse("logitech", "logitech:01020304", "PRO X 2");
    logitech.serial = Some("01020304".into());
    // Metadata omits route/mask. These are possible values, including 250 Hz;
    // actual supported values are still checked in the existing controller.
    assert_eq!(
        polling_menu_rates(&logitech),
        &[125, 250, 500, 1000, 2000, 4000, 8000]
    );
    assert_eq!(
        polling_menu_rates(&mouse("simulation", "simulated:mouse", "Mouse")),
        &[125, 500, 1000, 2000, 4000, 8000]
    );
}

#[test]
fn non_candidates_have_no_passive_rate_choices() {
    let mut device = mouse("razer", "razer:00be:unit", "Mouse");
    device.kind = "keyboard".into();
    assert!(polling_menu_rates(&device).is_empty());
    device.kind = "mouse".into();
    device.via = "bluetooth".into();
    assert!(polling_menu_rates(&device).is_empty());
    assert!(polling_menu_rates(&mouse("razer", "razer:0099:unit", "Mouse")).is_empty());
    assert!(polling_menu_rates(&mouse("corsair", "corsair:unit", "Mouse")).is_empty());
    assert!(polling_menu_rates(&mouse("mchose", "mchose:3837:unit", "MCHOSE M7 Ultra")).is_empty());
}

fn mouse(source: &str, key: &str, name: &str) -> ConfigurationDevice {
    let mut reading = Reading::new(key, name, source, 0);
    reading.kind = "mouse".into();
    reading.via = "usb".into();
    ConfigurationDevice::from_reading(&reading)
}

#[test]
fn razer_uses_exact_mouse_pid_scope_and_provider_metadata() {
    for pid in ["00be", "00bf", "009f", "00c1", "00b6", "00b7"] {
        assert!(polling_menu_candidate(&mouse(
            "razer",
            &format!("razer:{pid}:unit"),
            "Provider mouse name"
        )));
    }
    for key in [
        "razer:0099:unit",
        "razer:028d:unit",
        "razer:00be:",
        "razer:00be",
        "other:00be:unit",
    ] {
        assert!(!polling_menu_candidate(&mouse(
            "razer",
            key,
            "Razer Viper V3 Pro"
        )));
    }
    let mut device = mouse("razer", "razer:00be:unit", "Anything");
    device.via = "bluetooth".into();
    assert!(!polling_menu_candidate(&device));
    device.via = String::new();
    assert!(!polling_menu_candidate(&device));
}

#[test]
fn logitech_names_offer_only_known_model_probes_with_verified_unit_keys() {
    let mut device = mouse("logitech", "logitech:01020304", "PRO X 2");
    device.serial = Some("01020304".into());
    for name in [
        "PRO X 2",
        "Logitech PRO X 2 DEX",
        "PRO X2 SUPERSTRIKE",
        "Logitech G PRO X SUPERLIGHT 2",
    ] {
        device.name = name.into();
        assert!(polling_menu_candidate(&device));
    }
    device.via = String::new(); // Current HID++ battery provider metadata.
    assert!(polling_menu_candidate(&device));
    for name in [
        "Logitech device",
        "G502",
        "PRO X SUPERLIGHT",
        "PRO X SUPERLIGHT 2 SE",
        "PRO X SUPERLIGHT 2c",
        "PRO 2",
        "My PRO X 2",
    ] {
        device.name = name.into();
        assert!(!polling_menu_candidate(&device));
    }
    device.name = "PRO X 2".into();
    device.key = "logitech:receiver:c54d:1".into();
    assert!(!polling_menu_candidate(&device));
    device.key = "logitech:01020304".into();
    device.serial = Some("05060708".into());
    assert!(!polling_menu_candidate(&device));
    device.serial = None;
    assert!(!polling_menu_candidate(&device));
    device.serial = Some("00000000".into());
    device.key = "logitech:00000000".into();
    assert!(!polling_menu_candidate(&device));
    device.serial = Some("01020304".into());
    device.key = "logitech:01020304".into();
    device.via = "bluetooth".into();
    assert!(!polling_menu_candidate(&device));
}

#[test]
fn mchose_requires_catalog_family_and_exact_vendor_key() {
    let mut device = mouse("mchose", "mchose:3837:unit", "MCHOSE A7 V2 Ultra");
    assert!(polling_menu_candidate(&device));
    device.name = "MCHOSE A7 V2 Ultra+".into();
    assert!(polling_menu_candidate(&device));
    for name in [
        "MCHOSE M7 Ultra",
        "MCHOSE mouse (0x0010)",
        "A7",
        "My MCHOSE A7 V2 Ultra+",
    ] {
        device.name = name.into();
        assert!(!polling_menu_candidate(&device));
    }
    device.name = "MCHOSE A7 V2 Ultra+".into();
    for key in ["mchose:5253:unit", "mchose:3837:", "mchose:3837"] {
        device.key = key.into();
        assert!(!polling_menu_candidate(&device));
    }
    device.key = "mchose:3837:unit".into();
    device.via = "bluetooth".into();
    assert!(!polling_menu_candidate(&device));
}

#[test]
fn unavailable_non_mouse_and_other_providers_are_excluded() {
    let mut device = mouse("simulation", "simulated:mouse", "Simulated mouse");
    device.via = String::new();
    assert!(polling_menu_candidate(&device));
    for kind in ["keyboard", "headset", "gamepad", ""] {
        device.kind = kind.into();
        assert!(!polling_menu_candidate(&device));
    }
    device.kind = "mouse".into();
    device.capability = PollingCapability::Unavailable("No supported collection".into());
    assert!(!polling_menu_candidate(&device));
    for source in ["corsair", "steelseries", "unknown"] {
        assert!(!polling_menu_candidate(&mouse(
            source,
            "razer:00be:unit",
            "PRO X 2"
        )));
    }
}
