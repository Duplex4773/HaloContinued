//! Validate all stored reading fields without allocating their display strings.
//! Only the bounded set of chart representatives is decoded into owned readings.
use hb_core::{Connection, HistorySample, Precision, ProviderError, Reading};
use serde::{Deserialize, Deserializer, de};
use std::{borrow::Cow, fmt};

// Unlike IgnoredAny, this rejects a missing/non-string field just as Reading
// does. A damaged name/source must still break history continuity.
struct CheckedText;
impl<'de> Deserialize<'de> for CheckedText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TextVisitor;
        impl de::Visitor<'_> for TextVisitor {
            type Value = CheckedText;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string")
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<CheckedText, E> {
                Ok(CheckedText)
            }
        }
        deserializer.deserialize_str(TextVisitor)
    }
}

// Keep the same required fields and value types as Reading. Tests compare the
// two decoders, including malformed and missing display-only fields.
#[derive(Deserialize)]
pub(super) struct TimingReading<'a> {
    #[serde(borrow)]
    pub key: Cow<'a, str>,
    pub level: Option<u8>,
    pub charging: Option<bool>,
    pub charging_inferred: bool,
    pub connection: Connection,
    pub timestamp: i64,
    #[serde(rename = "precision")]
    _precision: Precision,
    #[serde(rename = "name")]
    _name: CheckedText,
    #[serde(borrow)]
    kind: Cow<'a, str>,
    #[serde(borrow)]
    source: Cow<'a, str>,
    #[serde(borrow)]
    via: Cow<'a, str>,
    #[serde(rename = "approx")]
    _approx: Option<CheckedText>,
    #[serde(default, borrow, deserialize_with = "optional_text")]
    serial: Option<Cow<'a, str>>,
    #[serde(default, borrow, deserialize_with = "optional_text")]
    container: Option<Cow<'a, str>>,
}
fn optional_text<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Cow<'de, str>>, D::Error> {
    #[derive(Deserialize)]
    struct Text<'a>(#[serde(borrow)] Cow<'a, str>);
    Option::<Text<'_>>::deserialize(deserializer).map(|value| value.map(|text| text.0))
}
impl TimingReading<'_> {
    pub fn awake(&self) -> bool {
        self.connection == Connection::Online
            && self.level.is_some_and(|level| level <= 100)
            && self.charging == Some(false)
            && !self.charging_inferred
    }
}

/// Keep only the previous identity, copying its few strings when it changes.
/// Ordinary JSON fields borrow SQLite text, so stable rows allocate no identities.
pub(super) struct TimingIdentity {
    kind: String,
    source: String,
    via: String,
    serial: Option<String>,
    container: Option<String>,
}
impl TimingIdentity {
    pub fn matches(&self, reading: &TimingReading<'_>) -> bool {
        self.kind == reading.kind
            && self.source == reading.source
            && self.via == reading.via
            && self.serial.as_deref() == reading.serial.as_deref()
            && self.container.as_deref() == reading.container.as_deref()
    }
}
impl From<&TimingReading<'_>> for TimingIdentity {
    fn from(reading: &TimingReading<'_>) -> Self {
        Self {
            kind: reading.kind.to_string(),
            source: reading.source.to_string(),
            via: reading.via.to_string(),
            serial: reading.serial.as_deref().map(str::to_owned),
            container: reading.container.as_deref().map(str::to_owned),
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct Sample {
    payload: String,
    pub timestamp: i64,
    pub ordinal: i64,
    pub level: Option<u8>,
    position: i64,
}
impl Sample {
    pub fn replace(
        &mut self,
        payload: &str,
        reading: &TimingReading<'_>,
        position: i64,
        ordinal: i64,
    ) {
        // Flat portions and off-screen history reuse these few buffers instead
        // of allocating a complete Reading and all its strings for every row.
        self.payload.clear();
        self.payload.push_str(payload);
        self.timestamp = reading.timestamp;
        self.ordinal = ordinal;
        self.level = reading.level;
        self.position = position;
    }
    pub fn decode(self) -> Result<HistorySample, ProviderError> {
        let reading = serde_json::from_str::<Reading>(&self.payload)
            .map_err(|e| ProviderError::new(format!("History reading: {e}")))?;
        Ok(HistorySample {
            reading,
            position: self.position,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_text_borrows_unescaped_json_and_ignores_display_name_changes() {
        let mut reading = Reading::new("synthetic", "Mouse", "test", 120);
        reading.serial = Some("UNIT-1234".into());
        reading.container = Some("CONTAINER-1234".into());
        reading.via = "usb".into();
        reading.kind = "mouse".into();
        let payload = serde_json::to_string(&reading).unwrap();
        let timing: TimingReading<'_> = serde_json::from_str(&payload).unwrap();
        assert!(matches!(timing.source, Cow::Borrowed(_)));
        assert!(matches!(timing.kind, Cow::Borrowed(_)));
        assert!(matches!(timing.via, Cow::Borrowed(_)));
        assert!(matches!(timing.serial, Some(Cow::Borrowed(_))));
        assert!(matches!(timing.container, Some(Cow::Borrowed(_))));
        let identity = TimingIdentity::from(&timing);
        reading.name = "Renamed mouse".into();
        let payload = serde_json::to_string(&reading).unwrap();
        let timing: TimingReading<'_> = serde_json::from_str(&payload).unwrap();
        assert!(identity.matches(&timing));
    }

    #[test]
    fn timing_decoder_preserves_full_reading_validation_and_escaped_text() {
        let mut reading = Reading::new("test:\"unit\"", "Mouse 雪\n", "simulation", 120);
        reading.level = Some(50);
        reading.charging = Some(false);
        reading.serial = Some("escaped\\unit".into());
        let original = serde_json::to_value(&reading).unwrap();
        let payload = original.to_string();
        let timing: TimingReading<'_> = serde_json::from_str(&payload).unwrap();
        assert_eq!(timing.key, reading.key);
        assert!(timing.awake());
        let mut sample = Sample::default();
        sample.replace(&payload, &timing, 42, 0);
        let decoded = sample.decode().unwrap();
        assert_eq!(decoded.reading, reading);
        assert_eq!(decoded.position, 42);

        for key in original.as_object().unwrap().keys() {
            let mut missing = original.clone();
            missing.as_object_mut().unwrap().remove(key);
            let mut variants = vec![missing];
            for value in [
                serde_json::json!(null),
                serde_json::json!(false),
                serde_json::json!(0),
                serde_json::json!(-1),
                serde_json::json!(256),
                serde_json::json!("text\n雪"),
                serde_json::json!([]),
                serde_json::json!({}),
            ] {
                let mut changed = original.clone();
                changed[key] = value;
                variants.push(changed);
            }
            for variant in variants {
                let payload = variant.to_string();
                assert_eq!(
                    serde_json::from_str::<Reading>(&payload).is_ok(),
                    serde_json::from_str::<TimingReading<'_>>(&payload).is_ok(),
                    "validation differs for {key}: {payload}"
                );
            }
        }
    }
}
