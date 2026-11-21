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
    #[serde(default)]
    pub next_poll_seconds: Option<u16>,
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
        self.via == reading.via && self.same_battery(reading)
    }
    pub fn same_battery(&self, reading: &TimingReading<'_>) -> bool {
        self.kind == reading.kind
            && self.source == reading.source
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
    online: bool,
    charging: Option<bool>,
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
        self.online = reading.connection == Connection::Online;
        self.charging = reading.charging;
    }
    pub fn same_run(&self, reading: &TimingReading<'_>) -> bool {
        self.level == reading.level
            && self.online == (reading.connection == Connection::Online)
            && self.charging == reading.charging
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

/// Keep both ends of every flat run until the plot actually exceeds its pixel budget.
type ChartBucket = Option<(Sample, Sample, Option<Sample>)>;
pub(super) struct ChartEdges {
    limit: usize,
    since: i64,
    span: i64,
    edges: Vec<(Sample, bool)>,
    buckets: Option<Vec<ChartBucket>>,
    first: Option<Sample>,
}
impl ChartEdges {
    pub fn new(limit: usize, since: i64, until: i64) -> Self {
        Self {
            limit,
            since,
            span: (until - since).max(1),
            edges: Vec::new(),
            buckets: None,
            first: None,
        }
    }
    pub fn push(&mut self, sample: &Sample, before_drop: bool) {
        if self.buckets.is_none() {
            if self.edges.len() < self.limit {
                self.edges.push((sample.clone(), before_drop));
                return;
            }
            let slots = self.limit - 2;
            self.buckets = Some(vec![None; if slots == 2 { 1 } else { slots / 3 }]);
            let edges = std::mem::take(&mut self.edges);
            self.first = edges.first().map(|(first, _)| first.clone());
            for (edge, drop) in edges {
                self.bucket(&edge, drop);
            }
        }
        self.bucket(sample, before_drop);
    }
    fn bucket(&mut self, sample: &Sample, before_drop: bool) {
        let buckets = self.buckets.as_mut().unwrap();
        if buckets.is_empty() {
            return;
        }
        let index = ((sample.position.saturating_sub(self.since).max(0) as i128
            * buckets.len() as i128)
            / self.span as i128)
            .min(buckets.len() as i128 - 1) as usize;
        let (low, high, anchor) =
            buckets[index].get_or_insert_with(|| (sample.clone(), sample.clone(), None));
        if sample.level < low.level {
            low.clone_from(sample);
        }
        if sample.level > high.level {
            high.clone_from(sample);
        }
        if self.limit >= 5 && before_drop && anchor.is_none() {
            *anchor = Some(sample.clone());
        }
    }
    pub fn finish(self, last: Option<Sample>) -> Vec<Sample> {
        let Some(buckets) = self.buckets else {
            return self.edges.into_iter().map(|(s, _)| s).collect();
        };
        let mut samples = Vec::with_capacity(self.limit);
        samples.extend(self.first);
        for (low, high, anchor) in buckets.into_iter().flatten() {
            samples.extend([low, high]);
            samples.extend(anchor);
        }
        samples.extend(last);
        samples.sort_unstable_by_key(|s| (s.timestamp, s.ordinal));
        samples.dedup_by_key(|s| (s.timestamp, s.ordinal));
        samples
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
