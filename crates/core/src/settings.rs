use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DevicePreferences {
    pub low_battery_mode: Option<LowBatteryMode>,
    pub name: Option<String>,
    pub hidden: bool,
    pub icon: Option<String>,
    pub low: Option<u8>,
    pub requested_polling_rate: Option<crate::PollingRate>,
    /// Opt-in target for Windows-reported exclusive fullscreen applications.
    pub fullscreen_boost_rate: Option<crate::PollingRate>,
}

/// Opt-in automatic reductions; absent fields leave that hardware setting unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LowBatteryMode {
    pub threshold: u8,
    pub polling_rate: Option<crate::PollingRate>,
    pub sleep_seconds: Option<u16>,
}
impl LowBatteryMode {
    pub fn valid(&self) -> bool {
        (1..=95).contains(&self.threshold)
            && self
                .sleep_seconds
                .is_none_or(|s| (60..=1200).contains(&s) && s % 60 == 0)
            && (self.polling_rate.is_some() || self.sleep_seconds.is_some())
    }
}

/// A device label is one bounded line in menus, notifications and native edits.
pub fn device_name(value: &str) -> Option<String> {
    let name: String = value
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub interval: u64,
    pub low: u8,
    /// Visual warning threshold; zero disables orange. Red low-alert color has priority.
    pub warning_level: u8,
    pub notify: bool,
    /// Optional Windows battery sound, repeated on fresh low readings every five minutes.
    pub low_sound: bool,
    pub full_alert: bool,
    pub bluetooth: bool,
    pub animation: bool,
    pub badges: bool,
    pub icon_theme: String,
    pub fluent_menu: bool,
    pub time_left: bool,
    pub percent_in_icon: bool,
    pub quiet_fullscreen: bool,
    pub status_file: bool,
    pub playstation_full_mode: bool,
    pub polling_controls: bool,
    pub restore_polling_on_startup: bool,
    pub disabled_providers: BTreeSet<String>,
    pub devices: BTreeMap<String, DevicePreferences>,
    /// Proven unit-serial aliases; hardware identities remain connection-specific.
    pub device_links: BTreeMap<String, String>,
    pub update_check: bool,
    pub release_repository: Option<String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            interval: 60,
            low: 20,
            warning_level: 30,
            notify: true,
            low_sound: false,
            full_alert: true,
            bluetooth: true,
            animation: true,
            badges: true,
            icon_theme: "auto".into(),
            fluent_menu: true,
            time_left: true,
            percent_in_icon: false,
            quiet_fullscreen: true,
            status_file: false,
            playstation_full_mode: false,
            polling_controls: false,
            restore_polling_on_startup: false,
            disabled_providers: BTreeSet::new(),
            devices: BTreeMap::new(),
            device_links: BTreeMap::new(),
            update_check: false,
            release_repository: None,
        }
    }
}
impl Settings {
    /// Invalid individual properties retain their defaults rather than invalidating
    /// the entire file. Unknown properties are ignored for forward compatibility.
    pub fn from_value(value: serde_json::Value) -> Result<Self, String> {
        let obj = value.as_object().ok_or("settings must be a JSON object")?;
        let mut base = serde_json::to_value(Self::default()).map_err(|e| e.to_string())?;
        for (key, val) in obj {
            let Some(default) = base.get(key) else {
                continue;
            };
            let valid = match key.as_str() {
                "interval" => val.as_u64().is_some_and(|n| (5..=3600).contains(&n)),
                "low" | "warning_level" => val.as_u64().is_some_and(|n| n <= 100),
                "icon_theme" => val
                    .as_str()
                    .is_some_and(|s| ["auto", "white", "black", "windows", "topbar"].contains(&s)),
                "disabled_providers" => {
                    serde_json::from_value::<BTreeSet<String>>(val.clone()).is_ok()
                }
                "devices" => val.is_object(),
                "device_links" => {
                    serde_json::from_value::<BTreeMap<String, String>>(val.clone()).is_ok()
                }
                "release_repository" => val.is_null() || val.as_str().is_some_and(valid_repository),
                _ => {
                    (default.is_boolean() && val.is_boolean())
                        || (default.is_string() && val.is_string())
                }
            };
            if valid {
                base[key] = if key == "devices" {
                    let devices: BTreeMap<String, DevicePreferences> = val
                        .as_object()
                        .unwrap()
                        .iter()
                        .filter_map(|(key, value)| {
                            let fields = value.as_object()?;
                            Some((
                                key.clone(),
                                DevicePreferences {
                                    low_battery_mode: fields
                                        .get("low_battery_mode")
                                        .and_then(|v| {
                                            serde_json::from_value::<LowBatteryMode>(v.clone()).ok()
                                        })
                                        .filter(LowBatteryMode::valid),
                                    name: fields
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .map(str::to_owned),
                                    hidden: fields
                                        .get("hidden")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false),
                                    icon: fields
                                        .get("icon")
                                        .and_then(|v| v.as_str())
                                        .map(str::to_owned),
                                    low: fields
                                        .get("low")
                                        .and_then(|v| v.as_u64())
                                        .filter(|n| *n <= 100)
                                        .map(|n| n as u8),
                                    requested_polling_rate: fields
                                        .get("requested_polling_rate")
                                        .and_then(|v| v.as_u64())
                                        .and_then(|n| u32::try_from(n).ok())
                                        .and_then(|n| crate::PollingRate::try_from(n).ok()),
                                    fullscreen_boost_rate: fields
                                        .get("fullscreen_boost_rate")
                                        .and_then(|v| v.as_u64())
                                        .filter(|n| *n > 1000)
                                        .and_then(|n| u32::try_from(n).ok())
                                        .and_then(|n| crate::PollingRate::try_from(n).ok()),
                                },
                            ))
                        })
                        .collect();
                    serde_json::to_value(devices).map_err(|e| e.to_string())?
                } else {
                    val.clone()
                };
            }
        }
        let mut settings: Self = serde_json::from_value(base).map_err(|e| e.to_string())?;
        let mut links = 0;
        settings.device_links.retain(|alias, target| {
            let keep = links < 512 && crate::controls::valid_preference_link(alias, target);
            if keep {
                links += 1;
            }
            keep
        });
        for d in settings.devices.values_mut() {
            if d.low.is_some_and(|n| n > 100) {
                d.low = None;
            }
            if d.icon.as_deref().is_some_and(|s| {
                ![
                    "mouse",
                    "headset",
                    "keyboard",
                    "gamepad",
                    "bluetooth",
                    "dualshock",
                    "dualsense",
                ]
                .contains(&s)
            }) {
                d.icon = None;
            }
            d.name = d.name.as_deref().and_then(device_name);
        }
        if settings.release_repository.is_none() {
            settings.update_check = false;
        }
        Ok(settings)
    }
    pub fn low_for(&self, key: &str) -> u8 {
        self.device(key).and_then(|d| d.low).unwrap_or(self.low)
    }
    pub fn preference_key<'a>(&'a self, key: &'a str) -> &'a str {
        self.device_links
            .get(key)
            .map(String::as_str)
            .unwrap_or(key)
    }
    pub fn device(&self, key: &str) -> Option<&DevicePreferences> {
        self.devices.get(self.preference_key(key))
    }
    pub fn device_mut(&mut self, key: &str) -> &mut DevicePreferences {
        let key = self.preference_key(key).to_owned();
        self.devices.entry(key).or_default()
    }
    pub fn set_device(&mut self, key: &str, preferences: DevicePreferences) {
        let key = self.preference_key(key).to_owned();
        self.devices.insert(key, preferences);
    }
    pub fn reset_device(&mut self, key: &str) {
        let canonical = self.preference_key(key).to_owned();
        self.devices.remove(&canonical);
        for (alias, target) in &self.device_links {
            if target == &canonical {
                self.devices.remove(alias);
            }
        }
    }
    pub fn link_device(&mut self, device: &crate::ConfigurationDevice) {
        let Some(canonical) = device.shared_preference_key() else {
            return;
        };
        let wired_pid = match canonical.split(':').nth(1) {
            Some("00bf") => "00be",
            Some("00b7") => "00b6",
            Some("00c1") => "00c0",
            _ => return,
        };
        let serial = canonical.splitn(3, ':').nth(2).unwrap();
        let wired = format!("razer:{wired_pid}:{serial}");
        if self.device_links.len() >= 512 && !self.device_links.contains_key(&wired) {
            return;
        }
        if let Some(old) = self.devices.remove(&wired) {
            self.devices.entry(canonical.clone()).or_insert(old);
        }
        self.device_links.entry(wired).or_insert(canonical);
    }
    pub fn enabled(&self, provider: &str) -> bool {
        !self.disabled_providers.contains(provider) && (provider != "bluetooth" || self.bluetooth)
    }
}
pub fn valid_repository(s: &str) -> bool {
    let parts: Vec<_> = s.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && *p != "."
                && *p != ".."
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
}
#[test]
fn fullscreen_boost_defaults_off_and_validates_saved_rates() {
    use serde_json::json;
    assert!(DevicePreferences::default().fullscreen_boost_rate.is_none());
    for value in [
        json!(null),
        json!("2000"),
        json!(-1),
        json!(1000),
        json!(3000),
        json!(999999),
    ] {
        let settings =
            Settings::from_value(json!({"devices":{"test":{"fullscreen_boost_rate":value}}}))
                .unwrap();
        assert!(settings.devices["test"].fullscreen_boost_rate.is_none());
    }
    for hz in [2000, 4000, 8000] {
        let settings =
            Settings::from_value(json!({"devices":{"test":{"fullscreen_boost_rate":hz}}})).unwrap();
        assert_eq!(
            settings.devices["test"].fullscreen_boost_rate.unwrap().hz(),
            hz
        );
        let restored = Settings::from_value(serde_json::to_value(&settings).unwrap()).unwrap();
        assert_eq!(
            restored.devices["test"].fullscreen_boost_rate,
            settings.devices["test"].fullscreen_boost_rate
        );
        assert!(!restored.polling_controls);
    }
}

#[cfg(test)]
mod low_battery_mode_tests {
    use super::*;
    #[test]
    fn opt_in_roundtrip_and_invalid_settings_are_isolated() {
        let s=Settings::from_value(serde_json::json!({"devices":{"mouse":{"name":"Test","low_battery_mode":{"threshold":20,"polling_rate":125,"sleep_seconds":60}}}})).unwrap();
        let roundtrip = Settings::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(
            roundtrip.devices["mouse"].low_battery_mode,
            s.devices["mouse"].low_battery_mode
        );
        for mode in [
            serde_json::json!({"threshold":101,"polling_rate":125}),
            serde_json::json!({"threshold":20,"sleep_seconds":0}),
            serde_json::json!({"threshold":20}),
            serde_json::json!({"threshold":20,"polling_rate":123}),
        ] {
            let s = Settings::from_value(
                serde_json::json!({"devices":{"mouse":{"name":"Test","low_battery_mode":mode}}}),
            )
            .unwrap();
            assert!(s.devices["mouse"].low_battery_mode.is_none());
            assert_eq!(s.devices["mouse"].name.as_deref(), Some("Test"));
        }
        assert!(DevicePreferences::default().low_battery_mode.is_none());
    }
}
