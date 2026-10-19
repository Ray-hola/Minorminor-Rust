//! Maintenance benchmark for an owned, closed SQLite snapshot.
use std::{path::PathBuf, time::Instant};
fn main() -> minorminor_core::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    assert!(
        (2..=3).contains(&args.len()),
        "compress-index SOURCE NEW_DESTINATION [LEVEL]"
    );
    let level = args
        .get(2)
        .map(|s| s.to_string_lossy().parse::<u32>())
        .transpose()?
        .unwrap_or(6);
    let start = Instant::now();
    let mut last = Instant::now();
    let mut phase = String::new();
    let result = minorminor_core::fs_compression::copy_with_level(
        &PathBuf::from(&args[0]),
        &PathBuf::from(&args[1]),
        level,
        |stage, done, total| {
            if phase != stage || last.elapsed().as_secs() >= 5 {
                eprintln!("{stage}: {done}/{total}");
                phase = stage.into();
                last = Instant::now();
            }
            Ok(())
        },
    )?;
    println!(
        "{}",
        serde_json::json!({"level":level,"seconds":start.elapsed().as_secs_f64(),"result":result})
    );
    Ok(())
}
