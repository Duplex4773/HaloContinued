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
    #[serde(rename = "kind")]
    _kind: CheckedText,
    #[serde(rename = "source")]
    _source: CheckedText,
    #[serde(rename = "via")]
    _via: CheckedText,
    #[serde(rename = "approx")]
    _approx: Option<CheckedText>,
    #[serde(rename = "serial")]
    _serial: Option<CheckedText>,
    #[serde(rename = "container")]
    _container: Option<CheckedText>,
}
impl TimingReading<'_> {
    pub fn awake(&self) -> bool {
        self.connection == Connection::Online
            && self.level.is_some_and(|level| level <= 100)
            && self.charging == Some(false)
            && !self.charging_inferred
    }
}

#[derive(Clone, Default)]
pub(super) struct Sample {
    payload: String,
    pub timestamp: i64,
    pub level: Option<u8>,
    position: i64,
}
impl Sample {
    pub fn replace(&mut self, payload: &str, reading: &TimingReading<'_>, position: i64) {
        // Flat portions and off-screen history reuse these few buffers instead
        // of allocating a complete Reading and all its strings for every row.
        self.payload.clear();
        self.payload.push_str(payload);
        self.timestamp = reading.timestamp;
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
        sample.replace(&payload, &timing, 42);
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
