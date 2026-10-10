//! Direct2D resources only exist while the History page exists.
use crate::dashboard_theme::Palette;
use hb_core::{HistoryAxis, HistorySample, HistorySeries};
use windows::Win32::{
    Foundation::HWND,
    Graphics::Direct2D::{Common::*, *},
};
use windows::{
    Win32::{Graphics::DirectWrite::*, UI::HiDpi::GetDpiForWindow},
    core::w,
};
use windows_numerics::Vector2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HistoryVertex {
    timestamp: i64,
    level: u8,
    measured: bool,
}

/// Display a last-known-value step trace, without turning missing readings into
/// measurements. A predecessor can seed the left boundary; no value is invented
/// before the first available percentage. Cached sleeping levels seed an empty
/// trace but cannot replace a level already observed in this interval.
fn history_trace(
    points: &[HistorySample],
    since: i64,
    until: i64,
) -> impl Iterator<Item = HistoryVertex> + '_ {
    let mut samples = points.iter();
    let mut previous: Option<HistoryVertex> = None;
    let mut pending = None;
    let mut finished = until < since;
    std::iter::from_fn(move || {
        if finished {
            return None;
        }
        if let Some(point) = pending.take() {
            previous = Some(point);
            return Some(point);
        }
        for sample in samples.by_ref() {
            let reading = &sample.reading;
            if sample.position > until {
                continue;
            }
            let timestamp = sample.position.max(since);
            if previous.is_some_and(|p| timestamp < p.timestamp)
                || previous.is_some() && !reading.online()
            {
                continue;
            }
            let Some(level) = reading.level.filter(|level| *level <= 100) else {
                continue;
            };
            let point = HistoryVertex {
                timestamp,
                level,
                measured: reading.online() && sample.position >= since,
            };
            if let Some(p) = previous {
                pending = Some(point);
                return Some(HistoryVertex {
                    timestamp,
                    measured: false,
                    ..p
                });
            }
            previous = Some(point);
            return Some(point);
        }
        finished = true;
        previous.take().map(|p| HistoryVertex {
            timestamp: until,
            measured: false,
            ..p
        })
    })
}

fn duration_label(seconds: i64, use_days: bool) -> String {
    let seconds = seconds.max(0);
    let (unit, singular, plural) = if use_days && seconds >= 86400 {
        (86400, "day", "days")
    } else if seconds >= 3600 {
        (3600, "hour", "hours")
    } else {
        return format!("{} min", seconds / 60);
    };
    let amount = seconds as f64 / unit as f64;
    if seconds % unit == 0 {
        format!(
            "{} {}",
            seconds / unit,
            if seconds == unit { singular } else { plural }
        )
    } else {
        format!("{amount:.1} {plural}")
    }
}

fn axis_labels(axis: HistoryAxis, span: i64) -> [String; 5] {
    std::array::from_fn(|index| match axis {
        HistoryAxis::Usage => duration_label(span.saturating_mul(index as i64) / 4, false),
        HistoryAxis::Calendar if index == 4 => "Now".into(),
        HistoryAxis::Calendar => {
            format!(
                "{} ago",
                duration_label(span.saturating_mul(4 - index as i64) / 4, true)
            )
        }
    })
}

pub struct Chart {
    factory: ID2D1Factory,
    stroke: ID2D1StrokeStyle,
    target: ID2D1HwndRenderTarget,
    font: IDWriteTextFormat,
    scale: f32,
    grid: ID2D1SolidColorBrush,
    line: ID2D1SolidColorBrush,
    label: ID2D1SolidColorBrush,
}
impl Chart {
    pub fn new(hwnd: HWND, width: u32, height: u32) -> windows::core::Result<Self> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let target = factory.CreateHwndRenderTarget(
                &D2D1_RENDER_TARGET_PROPERTIES {
                    r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                    dpiX: 96.,
                    dpiY: 96.,
                    ..Default::default()
                },
                &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd,
                    pixelSize: D2D_SIZE_U { width, height },
                    presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                },
            )?;
            let stroke = factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    miterLimit: 1.0,
                    ..Default::default()
                },
                None,
            )?;
            let scale = GetDpiForWindow(hwnd) as f32 / 96.;
            let writer: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let font = writer.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                13. * scale,
                w!("en-US"),
            )?;
            font.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let palette = Palette::new(false, false);
            let grid = target.CreateSolidColorBrush(&Palette::d2d(palette.border), None)?;
            let line = target.CreateSolidColorBrush(&Palette::d2d(palette.accent), None)?;
            let label = target.CreateSolidColorBrush(&Palette::d2d(palette.text), None)?;
            Ok(Self {
                factory,
                stroke,
                target,
                font,
                scale,
                grid,
                line,
                label,
            })
        }
    }
    #[cfg(test)]
    pub fn paint(
        &self,
        width: u32,
        height: u32,
        series: &HistorySeries,
    ) -> windows::core::Result<()> {
        self.paint_with_palette(width, height, series, &Palette::new(false, false))
    }
    pub fn paint_with_palette(
        &self,
        width: u32,
        height: u32,
        series: &HistorySeries,
        palette: &Palette,
    ) -> windows::core::Result<()> {
        let since = series.since;
        let until = series.until;
        unsafe {
            let size = self.target.GetPixelSize();
            if size.width != width || size.height != height {
                self.target.Resize(&D2D_SIZE_U { width, height })?;
            }
            self.grid.SetColor(&Palette::d2d(palette.border));
            self.grid
                .SetOpacity(if palette.high_contrast { 1.0 } else { 0.45 });
            self.line.SetColor(&Palette::d2d(palette.accent));
            self.label.SetColor(&Palette::d2d(if palette.high_contrast {
                palette.text
            } else {
                palette.disabled
            }));
            let left = 60. * self.scale;
            let right = width as f32 - 25. * self.scale;
            let top = 246. * self.scale;
            let bottom = height as f32 - 95. * self.scale;
            let samples = if series.axis == HistoryAxis::Usage && until <= since {
                &[][..]
            } else {
                &series.samples[..]
            };
            // One continuous stroke avoids seams and overlapping sample dots.
            // Geometry is temporary, built only for a visible-page repaint from
            // the existing width-bounded history; raw samples remain unchanged.
            let mut trace = history_trace(samples, since, until).peekable();
            let empty = trace.peek().is_none();
            let mut isolated_point = None;
            let geometry = if empty {
                None
            } else {
                let path = self.factory.CreatePathGeometry()?;
                let sink = path.Open()?;
                let mut started = false;
                let mut previous = None;
                for point in trace {
                    let p = Vector2 {
                        X: left
                            + (right - left) * point.timestamp.saturating_sub(since) as f32
                                / until.saturating_sub(since).max(1) as f32,
                        Y: bottom - (bottom - top) * point.level as f32 / 100.,
                    };
                    if previous == Some((p.X, p.Y)) {
                        continue;
                    }
                    if started {
                        sink.AddLine(p);
                        isolated_point = None;
                    } else {
                        sink.BeginFigure(p, D2D1_FIGURE_BEGIN_HOLLOW);
                        started = true;
                        isolated_point = Some(p);
                    }
                    previous = Some((p.X, p.Y));
                }
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
                sink.Close()?;
                Some(path)
            };
            self.target
                .SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            self.target.BeginDraw();
            self.target.Clear(Some(&Palette::d2d(palette.background)));
            let grid = &self.grid;
            let line = &self.line;
            for level in [0, 25, 50, 75, 100] {
                let y = bottom - (bottom - top) * level as f32 / 100.;
                self.target.DrawLine(
                    Vector2 { X: left, Y: y },
                    Vector2 { X: right, Y: y },
                    grid,
                    1.,
                    None,
                );
            }
            let label = &self.label;
            let text = |text: &str, x: f32, y: f32, w: f32, alignment| {
                self.font.SetTextAlignment(alignment)?;
                let chars: Vec<u16> = text.encode_utf16().collect();
                self.target.DrawText(
                    &chars,
                    &self.font,
                    &D2D_RECT_F {
                        left: x,
                        top: y,
                        right: x + w,
                        bottom: y + 24. * self.scale,
                    },
                    label,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
                windows::core::Result::Ok(())
            };
            text(
                "Battery level",
                left,
                top - 30. * self.scale,
                180. * self.scale,
                DWRITE_TEXT_ALIGNMENT_LEADING,
            )?;
            if let Some(level) = samples.iter().rev().find_map(|s| s.reading.level) {
                text(
                    &format!("Last recorded · {level}%"),
                    right - 230. * self.scale,
                    top - 30. * self.scale,
                    230. * self.scale,
                    DWRITE_TEXT_ALIGNMENT_TRAILING,
                )?;
            }
            for level in [0, 25, 50, 75, 100] {
                let y = bottom - (bottom - top) * level as f32 / 100.;
                text(
                    &format!("{level}%"),
                    8. * self.scale,
                    y - 10. * self.scale,
                    48. * self.scale,
                    DWRITE_TEXT_ALIGNMENT_LEADING,
                )?;
            }
            for (index, caption) in axis_labels(series.axis, until.saturating_sub(since))
                .iter()
                .enumerate()
            {
                let x = left + (right - left) * index as f32 / 4.;
                let label_width = 128. * self.scale;
                let (x, alignment) = match index {
                    0 => (x, DWRITE_TEXT_ALIGNMENT_LEADING),
                    4 => (x - label_width, DWRITE_TEXT_ALIGNMENT_TRAILING),
                    _ => (x - label_width / 2., DWRITE_TEXT_ALIGNMENT_CENTER),
                };
                text(
                    caption,
                    x,
                    bottom + 12. * self.scale,
                    label_width,
                    alignment,
                )?;
            }
            if empty {
                text(
                    if series.axis == HistoryAxis::Usage {
                        "No usage history yet. Readings appear as the device is used."
                    } else {
                        "No battery readings for this period yet."
                    },
                    left,
                    top + (bottom - top) / 2. - 12. * self.scale,
                    right - left,
                    DWRITE_TEXT_ALIGNMENT_CENTER,
                )?;
            }
            if let Some(geometry) = geometry {
                self.target.DrawGeometry(
                    &geometry,
                    line,
                    if palette.high_contrast { 2.0 } else { 1.5 } * self.scale,
                    &self.stroke,
                );
                if let Some(point) = isolated_point {
                    self.target.FillEllipse(
                        &D2D1_ELLIPSE {
                            point,
                            radiusX: 1.5 * self.scale,
                            radiusY: 1.5 * self.scale,
                        },
                        line,
                    );
                }
            }
            self.target.EndDraw(None, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hb_core::{Connection, Reading};
    use windows::Win32::UI::WindowsAndMessaging::*;

    fn sample(timestamp: i64, level: Option<u8>, connection: Connection) -> Reading {
        let mut reading = Reading::new("test:mouse", "Test mouse", "test", timestamp);
        reading.level = level;
        reading.connection = connection;
        reading
    }

    fn calendar_trace(points: &[Reading], since: i64, until: i64) -> Vec<HistoryVertex> {
        let series = HistorySeries::calendar(points.to_vec(), since, until);
        history_trace(&series.samples, since, until).collect()
    }

    #[test]
    fn sleep_and_missing_samples_hold_the_last_level_until_wake_and_now() {
        let rows = [
            sample(10, Some(80), Connection::Online),
            sample(20, Some(65), Connection::Sleeping),
            sample(30, None, Connection::Stale),
            sample(60, Some(75), Connection::Online),
            sample(90, None, Connection::Sleeping),
        ];
        let trace = calendar_trace(&rows, 0, 100);
        assert_eq!(
            trace
                .iter()
                .map(|p| (p.timestamp, p.level, p.measured))
                .collect::<Vec<_>>(),
            [
                (10, 80, true),
                (60, 80, false),
                (60, 75, true),
                (100, 75, false)
            ]
        );
    }

    #[test]
    fn predecessor_seeds_a_completely_sleeping_interval_without_measured_dots() {
        let rows = [
            sample(5, Some(27), Connection::Sleeping),
            sample(30, None, Connection::Sleeping),
            sample(90, None, Connection::Stale),
        ];
        let trace = calendar_trace(&rows, 10, 100);
        assert_eq!(trace.len(), 2);
        assert_eq!((trace[0].timestamp, trace[0].level), (10, 27));
        assert_eq!((trace[1].timestamp, trace[1].level), (100, 27));
        assert!(trace.iter().all(|p| !p.measured));
    }

    #[test]
    fn unknown_invalid_and_future_readings_do_not_invent_an_initial_level() {
        let rows = [
            sample(10, None, Connection::Online),
            sample(20, Some(101), Connection::Online),
            sample(110, Some(70), Connection::Online),
        ];
        assert!(calendar_trace(&rows, 0, 100).is_empty());
        assert!(calendar_trace(&rows, 100, 0).is_empty());
        let rows = [sample(30, Some(0), Connection::Online)];
        let trace = calendar_trace(&rows, 0, 100);
        assert_eq!((trace[0].timestamp, trace[0].level), (30, 0));
        assert_eq!((trace[1].timestamp, trace[1].level), (100, 0));
    }

    #[test]
    fn charging_and_discharge_changes_are_steps_at_actual_reading_times() {
        let rows = [
            sample(0, Some(25), Connection::Online),
            sample(20, Some(100), Connection::Online),
            sample(40, Some(99), Connection::Online),
        ];
        let trace = calendar_trace(&rows, 0, 50);
        assert_eq!(
            trace
                .iter()
                .map(|p| (p.timestamp, p.level))
                .collect::<Vec<_>>(),
            [(0, 25), (20, 25), (20, 100), (40, 100), (40, 99), (50, 99)]
        );
        assert_eq!(trace.iter().filter(|p| p.measured).count(), 3);
    }

    #[test]
    fn usage_positions_do_not_overwrite_real_timestamps_or_use_day_labels() {
        let samples = [
            HistorySample {
                reading: sample(1000, Some(30), Connection::Online),
                position: 0,
            },
            HistorySample {
                reading: sample(10000, None, Connection::Sleeping),
                position: 60,
            },
            HistorySample {
                reading: sample(20000, Some(27), Connection::Online),
                position: 60,
            },
        ];
        let trace: Vec<_> = history_trace(&samples, 0, 120).collect();
        assert_eq!(
            trace
                .iter()
                .map(|p| (p.timestamp, p.level))
                .collect::<Vec<_>>(),
            [(0, 30), (60, 30), (60, 27), (120, 27)]
        );
        assert_eq!(samples[2].reading.timestamp, 20000);
        assert_eq!(
            axis_labels(HistoryAxis::Usage, 86400),
            ["0 min", "6 hours", "12 hours", "18 hours", "24 hours"]
        );
        assert_eq!(axis_labels(HistoryAxis::Calendar, 86400)[0], "1 day ago");
        assert_eq!(axis_labels(HistoryAxis::Calendar, 86400)[4], "Now");
        assert_eq!(axis_labels(HistoryAxis::Usage, 7200)[2], "1 hour");
        assert_eq!(
            axis_labels(HistoryAxis::Calendar, 7 * 86400)[1],
            "5.2 days ago"
        );
    }

    #[test]
    fn history_target_can_paint_and_release_repeatedly() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Halo chart test"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                840,
                820,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            for _ in 0..5 {
                let chart = Chart::new(hwnd, 840, 820).unwrap();
                assert_eq!(chart.stroke.GetLineJoin(), D2D1_LINE_JOIN_ROUND);
                assert_eq!(chart.stroke.GetStartCap(), D2D1_CAP_STYLE_ROUND);
                chart.paint(840, 820, &HistorySeries::default()).unwrap();
                for palette in [Palette::new(true, false), Palette::new(true, true)] {
                    chart
                        .paint_with_palette(840, 820, &HistorySeries::default(), &palette)
                        .unwrap();
                    let series = HistorySeries::calendar(
                        (0..=1440)
                            .map(|i| {
                                sample(i * 60, Some(52 - (i * 28 / 1440) as u8), Connection::Online)
                            })
                            .collect(),
                        0,
                        86400,
                    );
                    chart
                        .paint_with_palette(840, 820, &series, &palette)
                        .unwrap();
                    chart
                        .paint_with_palette(
                            840,
                            820,
                            &HistorySeries::calendar(
                                vec![sample(86400, Some(24), Connection::Online)],
                                0,
                                86400,
                            ),
                            &palette,
                        )
                        .unwrap();
                }
                chart.paint(960, 900, &HistorySeries::default()).unwrap();
                let size = chart.target.GetPixelSize();
                assert_eq!((size.width, size.height), (960, 900));
                chart
                    .paint(
                        840,
                        820,
                        &HistorySeries::calendar(
                            vec![
                                sample(0, Some(30), Connection::Online),
                                sample(3600, None, Connection::Sleeping),
                                sample(7200, Some(27), Connection::Online),
                            ],
                            0,
                            86400,
                        ),
                    )
                    .unwrap();
                drop(chart);
            }
            DestroyWindow(hwnd).unwrap();
        }
    }
}
