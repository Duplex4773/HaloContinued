//! Read-only discovery timing; no battery or configuration reports are sent.
//! Pass catalog vendor IDs in hexadecimal. Each round invalidates metadata to
//! measure a complete discovery burst, including vendors with no attached device.
use hb_core::HidTransport;
use hb_windows::WindowsHid;
use std::{collections::BTreeSet, time::Instant};
use windows::Win32::{
    Foundation::FILETIME,
    System::Threading::{GetCurrentProcess, GetProcessTimes},
};

fn cpu_seconds() -> windows::core::Result<f64> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )?;
    }
    let ticks = |v: FILETIME| (u64::from(v.dwHighDateTime) << 32) | u64::from(v.dwLowDateTime);
    Ok((ticks(kernel) + ticks(user)) as f64 / 10_000_000.0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let vendors = std::env::args()
        .skip(1)
        .map(|s| u16::from_str_radix(s.trim_start_matches("0x"), 16))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if vendors.is_empty() {
        return Err("Pass hexadecimal vendor IDs to measure".into());
    }
    let hid = WindowsHid::new()?;
    let mut rows = Vec::new();
    for _ in 0..7 {
        hid.invalidate();
        let start_cpu = cpu_seconds()?;
        let start = Instant::now();
        let mut collections = 0;
        for &vendor in &vendors {
            collections += hid.enumerate(vendor)?.len();
        }
        rows.push(serde_json::json!({
            "wall_ms": start.elapsed().as_secs_f64() * 1000.0,
            "cpu_ms": (cpu_seconds()? - start_cpu) * 1000.0,
            "collections": collections,
        }));
    }
    println!(
        "{}",
        serde_json::json!({"vendors": vendors.len(), "rounds": rows})
    );
    Ok(())
}
