//! Archive selection based on the recording car's embedded SEI telemetry.
use std::{collections::{BTreeMap, HashSet}, path::{Component, Path}, time::{Duration, Instant}};
use anyhow::{Context, Result};
use crate::{extract::extract_gps_from_file, grouper::telemetry_has_driving};

/// Keep every camera angle for a RecentClips timestamp if any available angle
/// proves driving. Other archive categories pass through unchanged. Missing,
/// corrupt and metadata-free recordings never qualify on their own.
pub fn filter_driving_recent(root: &Path, candidates: &str, min_speed_mps: f32) -> Vec<String> {
    let mut scanned = 0usize;
    let mut last_report = Instant::now();
    filter_with(candidates, |relative| {
        if scanned == 0 || last_report.elapsed() >= Duration::from_secs(15) {
            eprintln!("archive driving filter: scanned {scanned} camera files; reading {relative}");
            last_report = Instant::now();
        }
        scanned += 1;
        match extract_gps_from_file(&root.join(relative).to_string_lossy()) {
            Ok(data) => telemetry_has_driving(
                &data.gear_runs, &data.gear_states, &[],
                data.raw_park_count, data.raw_frame_count,
            ) || data.speeds.iter().any(|s| s.abs() > min_speed_mps),
            Err(error) => {
                tracing::warn!(clip = relative, %error, "Cannot read archive driving metadata; skipping angle");
                false
            }
        }
    })
}

fn recording_key(relative: &str) -> Option<String> {
    let path = Path::new(relative);
    if path.components().any(|c| !matches!(c, Component::Normal(_))) { return None; }
    let name = path.file_name()?.to_str()?;
    if !name.ends_with(".mp4") || name.as_bytes().get(19) != Some(&b'-') { return None; }
    let timestamp = name.get(..19)?;
    chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d_%H-%M-%S").ok()?;
    Some(format!("{}/{timestamp}", path.parent()?.display()))
}

fn filter_with(candidates: &str, mut driving: impl FnMut(&str) -> bool) -> Vec<String> {
    // Nearby recording timestamps usually share a snapshot. Random hash-map
    // iteration revisits automounts and scatters reads across backing images.
    let mut groups: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for line in candidates.lines().filter(|line| line.starts_with("RecentClips/")) {
        if let Some(key) = recording_key(line) { groups.entry(key).or_default().push(line); }
    }
    let mut keep = HashSet::new();
    for (key, mut angles) in groups {
        // The front camera normally carries the same telemetry as the others.
        angles.sort_by_key(|path| !path.ends_with("-front.mp4"));
        if angles.into_iter().any(&mut driving) { keep.insert(key); }
    }
    candidates.lines().filter(|line| {
        !line.starts_with("RecentClips/")
            || recording_key(line).is_some_and(|key| keep.contains(&key))
    }).map(str::to_owned).collect()
}

/// Write only after all candidates have been classified. The caller replaces
/// its candidate list only on success, so an I/O failure cannot allow unfiltered
/// RecentClips through to an archive backend.
pub fn filter_file(root: &Path, input: &Path, output: &Path, min_speed_mps: f32) -> Result<()> {
    anyhow::ensure!(min_speed_mps.is_finite() && min_speed_mps >= 0.0, "Minimum archive speed must be a finite, non-negative number");
    anyhow::ensure!(input != output, "archive filter input and output must differ");
    let candidates = std::fs::read_to_string(input).context("Read archive candidates")?;
    let selected = filter_driving_recent(root, &candidates, min_speed_mps);
    let skipped = candidates.lines().count() - selected.len();
    eprintln!("archive driving filter: scan complete; keeping {} candidates, skipping {skipped}", selected.len());
    let mut result = selected.join("\n");
    if !result.is_empty() { result.push('\n'); }
    std::fs::write(output, result).context("Write filtered archive candidates")?;
    tracing::info!(skipped, "Filtered RecentClips without driving metadata");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_recordings_in_order_but_preserves_candidate_output_order() {
        let later = "RecentClips/2026-10-24_13-00-00-front.mp4";
        let earlier = "RecentClips/2026-10-24_12-00-00-front.mp4";
        let mut scanned = Vec::new();
        let selected = filter_with(&format!("{later}\n{earlier}\n"), |path| {
            scanned.push(path.to_owned());
            true
        });
        assert_eq!(scanned, [earlier, later]);
        assert_eq!(selected, [later, earlier]);
    }

    #[test]
    fn keeps_all_angles_of_driving_recordings_and_other_categories() {
        let prefix = "RecentClips/2026-10-24/2026-10-24_12-00-00";
        let front = format!("{prefix}-front.mp4");
        let rear = format!("{prefix}-back.mp4");
        let parked = "RecentClips/2026-10-24/2026-10-24_13-00-00-front.mp4";
        let saved = "SavedClips/event/front.mp4";
        let input = format!("{rear}\n{front}\n{parked}\n{saved}\nRecentClips/unrecognized.mp4\n");
        let mut scans = Vec::new();
        let selected = filter_with(&input, |path| { scans.push(path.to_owned()); path == front });
        assert_eq!(selected, [rear, front.clone(), saved.to_owned()]);
        assert!(scans.contains(&front));
        assert!(!scans.iter().any(|path| path.ends_with("-back.mp4")));
    }

    #[test]
    fn falls_back_to_another_angle_when_front_has_no_evidence() {
        let front = "RecentClips/2026-10-24/2026-10-24_12-00-00-front.mp4";
        let left = "RecentClips/2026-10-24/2026-10-24_12-00-00-left-repeater.mp4";
        assert_eq!(filter_with(&format!("{front}\n{left}\n"), |p| p == left), [front, left]);
    }

    // Synthetic length-prefixed H.264 SEI in a real MP4 mdat box, fed through
    // the production extractor rather than a mocked driving verdict.
    fn mp4(gear: u8, speed: f32) -> Vec<u8> {
        let mut proto = vec![0x10, gear, 0x25];
        proto.extend(speed.to_le_bytes());
        proto.push(0x59);
        proto.extend(37.5f64.to_le_bytes());
        proto.push(0x61);
        proto.extend((-122.25f64).to_le_bytes());
        let mut nal = vec![0x06, 0x05, 0x00, 0x42, 0x42, 0x42, 0x69];
        nal.extend(proto);
        nal.push(0x80);
        let mut result = ((12 + nal.len()) as u32).to_be_bytes().to_vec();
        result.extend(b"mdat");
        result.extend((nal.len() as u32).to_be_bytes());
        result.extend(nal);
        result
    }

    #[test]
    fn embedded_gear_and_speed_control_selection() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("RecentClips")).unwrap();
        for (gear, speed, expected) in [(0, 0.0, false), (1, 0.0, true),
            (2, 0.2, true), (0, 0.5, false), (0, 0.6, true)] {
            let clip = "RecentClips/2026-10-24_12-00-00-front.mp4";
            std::fs::write(root.path().join(clip), mp4(gear, speed)).unwrap();
            assert_eq!(!filter_driving_recent(root.path(), clip, 0.5).is_empty(), expected,
                "gear={gear}, speed={speed}");
        }
    }

    #[test]
    fn configurable_threshold_changes_speed_evidence_but_preserves_gear_rule() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("RecentClips")).unwrap();
        let clip = "RecentClips/2026-10-24_12-00-00-front.mp4";
        std::fs::write(root.path().join(clip), mp4(0, 2.0)).unwrap();
        for (threshold, expected) in [(0.0, true), (1.0, true), (2.0, false), (3.0, false)] {
            assert_eq!(!filter_driving_recent(root.path(), clip, threshold).is_empty(), expected);
        }
        std::fs::write(root.path().join(clip), mp4(1, 0.0)).unwrap();
        assert!(!filter_driving_recent(root.path(), clip, 100.0).is_empty());
        let input = root.path().join("input");
        let output = root.path().join("output");
        std::fs::write(&input, clip).unwrap();
        for invalid in [-1.0, f32::NAN, f32::INFINITY] {
            assert!(filter_file(root.path(), &input, &output, invalid).is_err());
            assert!(!output.exists());
        }
    }

    #[test]
    fn missing_and_metadata_free_clips_are_excluded() {
        let root = tempfile::tempdir().unwrap();
        let clip = "RecentClips/2026-10-24_12-00-00-front.mp4";
        assert!(filter_driving_recent(root.path(), clip, 0.5).is_empty());
        std::fs::create_dir(root.path().join("RecentClips")).unwrap();
        std::fs::write(root.path().join(clip), b"\0\0\0\x08mdat").unwrap();
        assert!(filter_driving_recent(root.path(), clip, 0.5).is_empty());
    }
}
