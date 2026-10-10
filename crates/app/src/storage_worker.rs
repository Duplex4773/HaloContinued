//! A failed history database must not disable configuration or status export.
use super::runtime::{Event, Events, Storage};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use hb_core::*;
use hb_storage::Store;
use std::{
    collections::VecDeque,
    path::PathBuf,
    time::{Duration, Instant},
};
const MAX_RECOVERY_DEVICES: usize = 512;
const MAX_RECOVERY_ACTIVITIES: usize = 256;

fn flush_activities(
    database: &mut Result<Store, ProviderError>,
    pending: &mut VecDeque<PollingActivity>,
) -> Result<(), ProviderError> {
    if pending.is_empty() {
        return Ok(());
    }
    let db = database.as_mut().map_err(|error| error.clone())?;
    while let Some(activity) = pending.front() {
        db.record_polling_activity(activity)?;
        pending.pop_front();
    }
    Ok(())
}

fn record_activity(
    database: &mut Result<Store, ProviderError>,
    activity: PollingActivity,
    pending: &mut VecDeque<PollingActivity>,
) -> Result<(), ProviderError> {
    // The normal path needs no recovery allocation. SQL's unique event key
    // makes replay safe even if a prior commit had an ambiguous outcome.
    if pending.is_empty() {
        let result = database
            .as_mut()
            .map_err(|e| e.clone())
            .and_then(|db| db.record_polling_activity(&activity));
        if result.is_err() {
            pending.push_back(activity);
        }
        return result;
    }
    let overflow = pending.len() == MAX_RECOVERY_ACTIVITIES;
    if overflow {
        pending.pop_front();
    }
    pending.push_back(activity);
    let result = flush_activities(database, pending);
    if overflow {
        Err(ProviderError::new(
            "Polling history recovery is full; the oldest unsaved event was discarded",
        ))
    } else {
        result
    }
}

/// Once a batch cannot be recorded, retain its latest unprocessed observation
/// per device. Outages must neither silently discard the tail nor grow RAM
/// with every changed battery sample.
fn retain_usage(
    pending: &mut Vec<UsageObservation>,
    observations: impl IntoIterator<Item = UsageObservation>,
) {
    for observation in observations {
        if let Some(old) = pending
            .iter_mut()
            .find(|old| old.reading.key == observation.reading.key)
        {
            *old = observation;
        } else if pending.len() < MAX_RECOVERY_DEVICES {
            pending.push(observation);
        }
    }
}

fn record_batch(
    db: &mut Store,
    observations: Vec<UsageObservation>,
    pending: &mut Vec<UsageObservation>,
) -> Result<(), ProviderError> {
    let mut observations = observations.into_iter();
    while let Some(observation) = observations.next() {
        if let Err(error) = db.record_usage(&observation) {
            retain_usage(pending, std::iter::once(observation).chain(observations));
            return Err(error);
        }
    }
    Ok(())
}
fn save_latest<T>(
    pending: &mut Option<T>,
    save: impl FnOnce(&T) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    if let Some(state) = pending.as_ref() {
        save(state)?;
        *pending = None;
    }
    Ok(())
}

fn record_in_order(
    db: &mut Store,
    observations: Vec<UsageObservation>,
    pending: &mut Vec<UsageObservation>,
) -> Result<(), ProviderError> {
    let recovered = std::mem::take(pending);
    if let Err(error) = record_batch(db, recovered, pending) {
        retain_usage(pending, observations);
        return Err(error);
    }
    record_batch(db, observations, pending)
}

fn recover_and_flush(
    database: &mut Result<Store, ProviderError>,
    path: &std::path::Path,
    pending_usage: &mut Vec<UsageObservation>,
    estimator: &mut Option<Estimator>,
) -> Result<(), ProviderError> {
    if database.is_err() {
        *database = Store::open(path);
    }
    let db = database.as_mut().map_err(|error| error.clone())?;
    db.flush()?;
    let recovered = std::mem::take(pending_usage);
    record_batch(db, recovered, pending_usage)?;
    save_latest(estimator, |state| db.save_estimator(state))?;
    db.flush()
}
pub(super) fn run(
    folder: PathBuf,
    messages: Receiver<Storage>,
    boot: Sender<Estimator>,
    events: Events,
) {
    let mut database = Store::open(&folder.join("history.db"));
    let _ = boot.send(
        database
            .as_ref()
            .map_or_else(|_| Estimator::default(), Store::load_estimator),
    );
    let clock = SystemClock::default();
    if let Ok(db) = &mut database {
        let _ = db.prune(clock.unix());
    } else if let Err(e) = &database {
        let _ = events.send(Event::Error(format!("History: {e}")));
    }
    let mut flush = Instant::now();
    let mut estimator = None;
    let mut pending_settings = None;
    let mut pending_activities = VecDeque::new();
    let mut pending_usage: Vec<UsageObservation> = Vec::new();
    loop {
        let message =
            messages.recv_timeout(Duration::from_secs(60).saturating_sub(flush.elapsed()));
        let result = match message {
            Ok(Storage::DpiBackup(backup, reply)) => {
                let result = hb_storage::save_dpi_backup(&folder, &backup);
                let _ = reply.send(result);
                Ok(())
            }
            Ok(Storage::PollingActivity(activity)) => {
                record_activity(&mut database, activity, &mut pending_activities)
            }
            Ok(Storage::UsageSample(observations)) => {
                if let Ok(db) = &mut database {
                    record_in_order(db, observations, &mut pending_usage)
                } else {
                    retain_usage(&mut pending_usage, observations);
                    Ok(())
                }
            }
            Ok(Storage::Insights(key, until, id)) => {
                let result =
                    flush_activities(&mut database, &mut pending_activities).and_then(|_| {
                        match &mut database {
                            Ok(db) => db.flush().and_then(|_| db.query_insights(&key, until)),
                            Err(e) => Err(e.clone()),
                        }
                    });
                let _ = events.send(Event::Insights(id, result.map(Box::new)));
                Ok(())
            }
            Ok(Storage::Save(settings)) => {
                pending_settings = Some(settings);
                save_latest(&mut pending_settings, |settings| {
                    hb_storage::save_settings(&folder.join("config.json"), settings)
                })
            }
            Ok(Storage::Status(snapshot, running)) => {
                hb_storage::write_status(&folder.join("status.json"), &snapshot, running)
            }
            Ok(Storage::RemoveStatus) => match std::fs::remove_file(folder.join("status.json")) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            },
            Ok(Storage::State(state)) => {
                // A newer checkpoint supersedes any failed/deferred one. Keep
                // exactly the latest state until persistence succeeds.
                estimator = Some(state);
                if let Ok(db) = &mut database {
                    save_latest(&mut estimator, |state| db.save_estimator(state))
                } else {
                    Ok(())
                }
            }
            Ok(Storage::History(key, since, until, width, id)) => {
                let result = match &mut database {
                    Ok(db) => db
                        .flush()
                        .and_then(|_| db.query_with_baseline(&key, since, until, width))
                        .map(|readings| HistorySeries::calendar(readings, since, until)),
                    Err(e) => Err(e.clone()),
                };
                let _ = events.send(Event::History(id, result));
                Ok(())
            }
            Ok(Storage::UsageHistory(key, seconds, until, width, id)) => {
                let result = match &mut database {
                    Ok(db) => db
                        .flush()
                        .and_then(|_| db.query_usage(&key, until, seconds, width)),
                    Err(e) => Err(e.clone()),
                };
                let _ = events.send(Event::History(id, result));
                Ok(())
            }
            Ok(Storage::Quit) | Err(RecvTimeoutError::Disconnected) => {
                if let Err(e) = save_latest(&mut pending_settings, |settings| {
                    hb_storage::save_settings(&folder.join("config.json"), settings)
                }) {
                    let _ = events.send(Event::Error(format!("Settings save: {e}")));
                }
                if let Err(e) = recover_and_flush(
                    &mut database,
                    &folder.join("history.db"),
                    &mut pending_usage,
                    &mut estimator,
                )
                .and_then(|_| flush_activities(&mut database, &mut pending_activities))
                {
                    let _ = events.send(Event::Error(format!("History flush: {e}")));
                }
                break;
            }
            Err(RecvTimeoutError::Timeout) => Ok(()),
        };
        if let Err(e) = result {
            let _ = events.send(Event::Error(format!("Storage: {e}")));
        }
        if flush.elapsed() >= Duration::from_secs(60) {
            if let Err(e) = save_latest(&mut pending_settings, |settings| {
                hb_storage::save_settings(&folder.join("config.json"), settings)
            }) {
                let _ = events.send(Event::Error(format!("Settings save: {e}")));
            }
            let result = recover_and_flush(
                &mut database,
                &folder.join("history.db"),
                &mut pending_usage,
                &mut estimator,
            )
            .and_then(|_| flush_activities(&mut database, &mut pending_activities))
            .and_then(|_| database.as_mut().unwrap().prune_expired(clock.unix()));
            if let Err(e) = result {
                let _ = events.send(Event::Error(format!("History: {e}")));
            }
            flush = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_settings_save_retries_only_latest_choices() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        std::fs::create_dir(&path).unwrap();
        let mut pending = Some(Settings {
            low: 25,
            ..Settings::default()
        });
        assert!(save_latest(&mut pending, |s| hb_storage::save_settings(&path, s)).is_err());
        assert_eq!(pending.as_ref().unwrap().low, 25);
        pending = Some(Settings {
            low: 15,
            ..Settings::default()
        });
        assert!(save_latest(&mut pending, |s| hb_storage::save_settings(&path, s)).is_err());
        std::fs::remove_dir(&path).unwrap();
        save_latest(&mut pending, |s| hb_storage::save_settings(&path, s)).unwrap();
        assert!(pending.is_none());
        assert_eq!(hb_storage::load_settings(&path).low, 15);
    }

    fn activity(timestamp: i64, kind: PollingActivityKind) -> PollingActivity {
        PollingActivity {
            key: "synthetic:mouse".into(),
            timestamp,
            session: 1,
            kind,
            rate: PollingRate::try_from(4000).unwrap(),
            previous: PollingRate::try_from(1000).ok(),
        }
    }

    #[test]
    fn failed_polling_events_recover_without_losing_or_duplicating_insights() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.db");
        drop(Store::open(&path).unwrap());
        let mut database = Store::read_only(&path);
        let mut pending = VecDeque::new();
        assert!(
            record_activity(
                &mut database,
                activity(100, PollingActivityKind::Boost),
                &mut pending
            )
            .is_err()
        );
        assert!(
            record_activity(
                &mut database,
                activity(200, PollingActivityKind::Restore),
                &mut pending
            )
            .is_err()
        );
        assert_eq!(pending.len(), 2);
        database = Store::open(&path);
        // This duplicate represents replay after an uncertain previous commit.
        record_activity(
            &mut database,
            activity(100, PollingActivityKind::Boost),
            &mut pending,
        )
        .unwrap();
        assert!(pending.is_empty());
        let insights = database
            .as_ref()
            .unwrap()
            .query_insights("synthetic:mouse", 300)
            .unwrap();
        assert_eq!(insights.recent.boost_sessions, 1);
        assert_eq!(insights.recent.restores, 1);
        flush_activities(&mut database, &mut pending).unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn prolonged_event_storage_failure_has_a_fixed_recovery_limit() {
        let mut database = Err(ProviderError::new("Synthetic unavailable history"));
        let mut pending = VecDeque::new();
        for timestamp in 0..(MAX_RECOVERY_ACTIVITIES as i64 + 1) {
            assert!(
                record_activity(
                    &mut database,
                    activity(timestamp, PollingActivityKind::Boost),
                    &mut pending
                )
                .is_err()
            );
        }
        assert_eq!(pending.len(), MAX_RECOVERY_ACTIVITIES);
        assert_eq!(pending.front().unwrap().timestamp, 1);
        assert_eq!(
            pending.back().unwrap().timestamp,
            MAX_RECOVERY_ACTIVITIES as i64
        );
    }

    fn estimator(usage: f64) -> Estimator {
        let mut state = Estimator::default();
        let discharge = state.devices.entry("synthetic".into()).or_default();
        discharge.usage = usage;
        discharge.seen = SystemClock::default().unix();
        state
    }

    #[test]
    fn newer_checkpoint_supersedes_deferred_state_after_write_failure() {
        let mut pending = Some(estimator(10.0));
        assert!(
            save_latest(&mut pending, |_| {
                Err(ProviderError::new("Synthetic write failure"))
            })
            .is_err()
        );
        assert_eq!(pending.as_ref().unwrap().devices["synthetic"].usage, 10.0);
        // Storage::State always replaces pending before attempting to save.
        pending = Some(estimator(20.0));
        save_latest(&mut pending, |state| {
            assert_eq!(state.devices["synthetic"].usage, 20.0);
            Ok(())
        })
        .unwrap();
        assert!(pending.is_none());
        save_latest(&mut pending, |_| {
            panic!("An obsolete checkpoint must not be replayed")
        })
        .unwrap();
    }

    #[test]
    fn final_recovery_reopens_database_and_saves_deferred_reading_and_estimator() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.db");
        std::fs::create_dir(&path).unwrap();
        let mut database = Store::open(&path);
        assert!(database.is_err());
        let mut pending = vec![observation("synthetic", 100, 77)];
        let mut state = Some(estimator(120.0));
        assert!(recover_and_flush(&mut database, &path, &mut pending, &mut state).is_err());
        assert_eq!(pending.len(), 1);
        assert!(state.is_some());
        std::fs::remove_dir(&path).unwrap();
        recover_and_flush(&mut database, &path, &mut pending, &mut state).unwrap();
        assert!(pending.is_empty());
        assert!(state.is_none());
        let store = Store::read_only(&path).unwrap();
        assert_eq!(
            store.query("synthetic", 0, 200, 10).unwrap()[0].level,
            Some(77)
        );
        assert_eq!(store.load_estimator().devices["synthetic"].usage, 120.0);
    }

    fn observation(key: &str, timestamp: i64, level: u8) -> UsageObservation {
        let mut reading = Reading::new(key, "Synthetic device", "simulation", timestamp);
        reading.level = Some(level);
        UsageObservation {
            reading,
            polling_rate: None,
            session: None,
        }
    }

    #[test]
    fn failed_batch_retains_unprocessed_devices_for_bounded_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.db");
        drop(Store::open(&path).unwrap());
        // A read-only SQLite connection accepts staged records but rejects
        // their first flush, giving a deterministic write failure.
        let mut database = Store::read_only(&path).unwrap();
        for timestamp in 0..4095 {
            database
                .record_usage(&observation("mouse", timestamp, (timestamp % 100) as u8))
                .unwrap();
        }
        let mut pending = Vec::new();
        assert!(
            record_batch(
                &mut database,
                vec![
                    observation("mouse", 5000, 10),
                    observation("headset", 5000, 20),
                    observation("controller", 5000, 30),
                ],
                &mut pending,
            )
            .is_err()
        );
        assert_eq!(pending.len(), 3, "The whole unprocessed tail must survive");
        retain_usage(&mut pending, [observation("headset", 5060, 19)]);
        let mut recovered = Store::open(&path).unwrap();
        let retry = std::mem::take(&mut pending);
        record_batch(&mut recovered, retry, &mut pending).unwrap();
        recovered.flush().unwrap();
        assert!(pending.is_empty());
        for (key, level) in [("mouse", 10), ("headset", 19), ("controller", 30)] {
            let readings = recovered.query(key, 0, 6000, 10).unwrap();
            assert_eq!(readings.len(), 1);
            assert_eq!(readings[0].level, Some(level));
        }
        retain_usage(
            &mut pending,
            (0..700).map(|index| observation(&format!("synthetic:{index}"), 6000, 50)),
        );
        assert_eq!(pending.len(), MAX_RECOVERY_DEVICES);
        retain_usage(&mut pending, [observation("synthetic:0", 6060, 49)]);
        assert_eq!(pending.len(), MAX_RECOVERY_DEVICES);
        assert_eq!(pending[0].reading.level, Some(49));
    }

    #[test]
    fn new_samples_follow_recovery_so_older_same_second_data_cannot_overwrite_them() {
        let directory = tempfile::tempdir().unwrap();
        let mut db = Store::open(&directory.path().join("history.db")).unwrap();
        let mut pending = vec![observation("mouse", 5000, 70)];
        record_in_order(&mut db, vec![observation("mouse", 5000, 69)], &mut pending).unwrap();
        db.flush().unwrap();
        assert!(pending.is_empty());
        assert_eq!(
            db.query("mouse", 5000, 5000, 10).unwrap()[0].level,
            Some(69)
        );
        // Pacing must track the latest recovered state too.
        record_in_order(&mut db, vec![observation("mouse", 5001, 69)], &mut pending).unwrap();
        db.flush().unwrap();
        assert_eq!(db.query("mouse", 5000, 5001, 10).unwrap().len(), 1);
    }
}
