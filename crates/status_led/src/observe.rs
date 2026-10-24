//! Read-only Linux observations, with injectable roots for synthetic tests.
use crate::model::{ArchivePhase, CameraStamp, Link, Observation};
use std::os::unix::fs::MetadataExt;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Paths {
    pub leds: PathBuf,
    pub model: PathBuf,
    pub run: PathBuf,
    pub proc: PathBuf,
    pub gadget: PathBuf,
    pub udcs: PathBuf,
    pub camera: PathBuf,
    pub cycle_lock: PathBuf,
    pub archive_script: PathBuf,
}
impl Default for Paths {
    fn default() -> Self {
        Self {
            leds: "/sys/class/leds".into(),
            model: "/proc/device-tree/model".into(),
            run: "/run/sentryusb-led".into(),
            proc: "/proc".into(),
            gadget: "/sys/kernel/config/usb_gadget/sentryusb".into(),
            udcs: "/sys/class/udc".into(),
            camera: "/backingfiles/cam_disk.bin".into(),
            cycle_lock: "/tmp/sentryusb_gadget_cycle.lock".into(),
            archive_script: "/root/bin/archiveloop".into(),
        }
    }
}

pub fn process_start(proc: &Path, pid: u32) -> Option<String> {
    let process = process_info(proc, pid)?;
    process.alive.then_some(process.start)
}
struct ProcessInfo {
    parent: u32,
    start: String,
    alive: bool,
}
fn process_info(proc: &Path, pid: u32) -> Option<ProcessInfo> {
    let stat = fs::read_to_string(proc.join(pid.to_string()).join("stat")).ok()?;
    // comm may contain spaces and parentheses; fields after its final ')' are stable.
    let tail = stat
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    Some(ProcessInfo {
        parent: tail.get(1)?.parse().ok()?,
        start: tail.get(19)?.to_string(),
        alive: !matches!(*tail.first()?, "Z" | "X" | "x"),
    })
}

struct ArchiveRecord {
    pid: u32,
    start: String,
    phase: ArchivePhase,
}

impl Paths {
    pub(crate) fn archive_handoff_safe(&self) -> bool {
        // A refreshed on-disk script does not replace an old shell's loaded
        // functions. Check both the installed protocol and all live workers.
        match fs::read_to_string(&self.archive_script) {
            Ok(script)
                if !script
                    .lines()
                    .any(|line| line == "# sentryusb-status-led: cooperative-v1") =>
            {
                return false;
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return false,
            _ => {}
        }
        let worker = self.archive_record();
        let Ok(processes) = fs::read_dir(&self.proc) else {
            return false;
        };
        for entry in processes {
            let Ok(entry) = entry else {
                return false;
            };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let command = match fs::read(entry.path().join("cmdline")) {
                Ok(command) => command,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return false,
            };
            if !command.split(|b| *b == 0).any(|arg| {
                std::str::from_utf8(arg)
                    .ok()
                    .and_then(|s| Path::new(s).file_name())
                    == Some(std::ffi::OsStr::new("archiveloop"))
            }) {
                continue;
            }
            let Some(process) = process_info(&self.proc, pid) else {
                if !entry.path().exists() {
                    continue;
                }
                return false;
            };
            if !process.alive {
                continue;
            }
            let Some(worker) = &worker else {
                return false;
            };
            // A cooperative shell's flock parents and helper subshells belong
            // to that same lifetime. A separate old worker still defers us.
            if !self.is_ancestor(pid, worker.pid) && !self.is_ancestor(worker.pid, pid) {
                return false;
            }
        }
        true
    }
    fn is_ancestor(&self, ancestor: u32, mut pid: u32) -> bool {
        for _ in 0..64 {
            if pid == ancestor {
                return true;
            }
            let Some(process) = process_info(&self.proc, pid) else {
                return false;
            };
            if !process.alive || process.parent <= 1 {
                return false;
            }
            pid = process.parent;
        }
        false
    }
    fn maintenance_active(&self) -> bool {
        // Observe the existing whole-cycle lock without acquiring it or adding
        // a second authority. The kernel clears it on drop, cancellation or death.
        let Ok(meta) = fs::metadata(&self.cycle_lock) else {
            return false;
        };
        let Ok(locks) = fs::read_to_string(self.proc.join("locks")) else {
            return false;
        };
        locks.lines().any(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            // Blocked requests include "->" and must not count as an owner.
            if fields.len() < 8 || fields[1] != "FLOCK" || fields[3] != "WRITE" {
                return false;
            }
            let device = fields[5].split(':').collect::<Vec<_>>();
            device.len() == 3
                && u64::from_str_radix(device[0], 16).ok()
                    == Some(u64::from(libc::major(meta.dev())))
                && u64::from_str_radix(device[1], 16).ok()
                    == Some(u64::from(libc::minor(meta.dev())))
                && device[2].parse::<u64>().ok() == Some(meta.ino())
        })
    }
    pub fn archive_phase(&self) -> ArchivePhase {
        self.archive_record()
            .map_or(ArchivePhase::Missing, |r| r.phase)
    }
    fn archive_record(&self) -> Option<ArchiveRecord> {
        let text = fs::read_to_string(self.run.join("archive")).ok()?;
        let parts = text.split_whitespace().collect::<Vec<_>>();
        if parts.len() != 3 {
            return None;
        }
        let pid = parts[0].parse::<u32>().ok().filter(|pid| *pid > 1)?;
        if process_start(&self.proc, pid).as_deref() != Some(parts[1]) {
            return None;
        }
        let phase = match parts[2] {
            "starting" => ArchivePhase::Starting,
            "working" => ArchivePhase::Working,
            "ready" => ArchivePhase::Ready,
            "fault" => ArchivePhase::Fault,
            _ => return None,
        };
        Some(ArchiveRecord {
            pid,
            start: parts[1].into(),
            phase,
        })
    }
    pub fn observe(&self, storage_ready: bool) -> Observation {
        let worker = self.archive_record();
        let mut o = Observation {
            archive: worker.as_ref().map_or(ArchivePhase::Missing, |r| r.phase),
            storage_ready,
            link: Link::Unbound,
            session: None,
            camera: None,
            maintenance: self.maintenance_active(),
        };
        let generation = fs::read_to_string(self.run.join("media-generation")).unwrap_or_default();
        if generation.starts_with("busy ") {
            return o;
        }
        let udc = fs::read_to_string(self.gadget.join("UDC")).unwrap_or_default();
        let name = udc.trim();
        if name.is_empty() {
            return o;
        }
        if name.contains('/') || name == ".." || name == "." {
            o.link = Link::Unknown;
            return o;
        }
        let function = self.gadget.join("functions/mass_storage.0");
        let linked = self.gadget.join("configs/c.1/mass_storage.0");
        let primary = fs::read_to_string(function.join("lun.0/file")).unwrap_or_default();
        let valid = fs::canonicalize(&function)
            .ok()
            .zip(fs::canonicalize(linked).ok())
            .is_some_and(|(a, b)| a == b)
            && fs::metadata(primary.trim()).is_ok_and(|m| m.is_file() && m.len() > 0)
            && fs::read_to_string(function.join("lun.0/ro")).is_ok_and(|s| s.trim() == "0");
        if !valid {
            o.link = Link::Unknown;
            return o;
        }
        let state = fs::read_to_string(self.udcs.join(name).join("state")).unwrap_or_default();
        o.link = match state.trim() {
            "configured" => Link::Configured,
            "not attached" | "not-attached" | "attached" | "powered" | "default" | "addressed"
            | "suspended" | "reconnecting" | "unauthenticated" => Link::Waiting,
            _ => Link::Unknown,
        };
        if o.link != Link::Configured {
            return o;
        }
        if let Ok(metadata) = fs::metadata(&function) {
            // Phase is advisory and can change while the same host keeps writing.
            // Only a different worker lifetime invalidates the session baseline.
            let worker = worker
                .map(|r| format!("{}:{}", r.pid, r.start))
                .unwrap_or_default();
            o.session = Some(format!(
                "{name}:{}:{}:{generation}:{worker}",
                metadata.dev(),
                metadata.ino()
            ));
        }
        for n in 0..8 {
            let lun = function.join(format!("lun.{n}"));
            let file = fs::read_to_string(lun.join("file")).unwrap_or_default();
            if Path::new(file.trim()) == self.camera
                && fs::read_to_string(lun.join("ro")).is_ok_and(|s| s.trim() == "0")
            {
                o.camera = fs::metadata(&self.camera)
                    .ok()
                    .filter(|m| m.is_file() && m.len() > 0)
                    .map(|m| CameraStamp {
                        device: m.dev(),
                        inode: m.ino(),
                        size: m.len(),
                        modified_ns: i128::from(m.mtime()) * 1_000_000_000
                            + i128::from(m.mtime_nsec()),
                    });
                break;
            }
        }
        // A gadget mutation during the sample invalidates the whole write baseline.
        if fs::read_to_string(self.run.join("media-generation")).unwrap_or_default() != generation
            || fs::read_to_string(self.gadget.join("UDC")).unwrap_or_default() != udc
            || fs::read_to_string(self.udcs.join(name).join("state")).unwrap_or_default() != state
        {
            o.link = Link::Unbound;
            o.session = None;
            o.camera = None;
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fn fixture() -> (tempfile::TempDir, Paths) {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let p = Paths {
            leds: r.join("leds"),
            model: r.join("model"),
            run: r.join("run"),
            proc: r.join("proc"),
            gadget: r.join("gadget"),
            udcs: r.join("udcs"),
            camera: r.join("camera.bin"),
            cycle_lock: r.join("cycle.lock"),
            archive_script: r.join("archiveloop"),
        };
        write(&p.camera, "fake image");
        write(&p.gadget.join("UDC"), "usb-a\n");
        write(
            &p.gadget.join("functions/mass_storage.0/lun.0/file"),
            p.camera.to_str().unwrap(),
        );
        write(&p.gadget.join("functions/mass_storage.0/lun.0/ro"), "0");
        fs::create_dir_all(p.gadget.join("configs/c.1")).unwrap();
        std::os::unix::fs::symlink(
            p.gadget.join("functions/mass_storage.0"),
            p.gadget.join("configs/c.1/mass_storage.0"),
        )
        .unwrap();
        write(&p.udcs.join("usb-a/state"), "configured\n");
        (d, p)
    }
    #[test]
    fn uses_only_the_bound_udc_and_requires_a_populated_writable_lun() {
        let (_d, p) = fixture();
        assert_eq!(p.observe(true).link, Link::Configured);
        assert!(p.observe(true).camera.is_some());
        write(&p.udcs.join("unrelated/state"), "configured");
        write(&p.udcs.join("usb-a/state"), "suspended");
        assert_eq!(p.observe(true).link, Link::Waiting);
        fs::remove_file(p.udcs.join("usb-a/state")).unwrap();
        assert_eq!(p.observe(true).link, Link::Unknown);
        write(&p.udcs.join("usb-a/state"), "configured");
        write(&p.gadget.join("functions/mass_storage.0/lun.0/ro"), "1");
        assert_ne!(p.observe(true).link, Link::Configured);
    }
    #[test]
    fn unknown_state_and_unlinked_function_cannot_show_connected() {
        let (_d, p) = fixture();
        write(&p.udcs.join("usb-a/state"), "future-state");
        assert_eq!(p.observe(true).link, Link::Unknown);
        write(&p.udcs.join("usb-a/state"), "configured");
        fs::remove_file(p.gadget.join("configs/c.1/mass_storage.0")).unwrap();
        assert_ne!(p.observe(true).link, Link::Configured);
    }
    #[test]
    fn only_camera_image_can_supply_recording_activity() {
        let (_d, p) = fixture();
        let music = p.camera.with_file_name("music.bin");
        write(&music, "music");
        write(
            &p.gadget.join("functions/mass_storage.0/lun.0/file"),
            music.to_str().unwrap(),
        );
        let o = p.observe(true);
        assert_eq!(o.link, Link::Configured);
        assert!(o.camera.is_none());
    }
    #[test]
    fn media_transition_suppresses_sampling_and_changes_session() {
        let (_d, p) = fixture();
        let initial = p.observe(true).session;
        write(&p.run.join("media-generation"), "busy 22:100:1");
        assert_ne!(p.observe(true).link, Link::Configured);
        write(&p.run.join("media-generation"), "ready 22:100:2");
        assert_ne!(p.observe(true).session, initial);
    }
    #[test]
    fn archive_hint_rejects_dead_reused_and_zombie_processes() {
        let (_d, p) = fixture();
        write(&p.run.join("archive"), "42 123 ready\n");
        assert_eq!(p.archive_phase(), ArchivePhase::Missing);
        let stat = format!("42 (worker (name)) S {} 123 0", vec!["0"; 18].join(" "));
        write(&p.proc.join("42/stat"), &stat);
        assert_eq!(p.archive_phase(), ArchivePhase::Ready);
        write(&p.run.join("archive"), "42 124 ready\n");
        assert_eq!(p.archive_phase(), ArchivePhase::Missing);
        write(&p.run.join("archive"), "42 123 ready\n");
        write(&p.proc.join("42/stat"), &stat.replace(") S ", ") Z "));
        assert_eq!(p.archive_phase(), ArchivePhase::Missing);
    }

    fn archive_worker(p: &Paths, phase: &str) {
        write(
            &p.proc.join("42/stat"),
            &format!("42 (archiveloop) S {} 123 0", vec!["0"; 18].join(" ")),
        );
        write(&p.run.join("archive"), &format!("42 123 {phase}\n"));
    }
    fn camera_modified(p: &Paths, seconds: u64) {
        fs::File::options()
            .write(true)
            .open(&p.camera)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds)),
            )
            .unwrap();
    }
    #[test]
    fn archive_phase_changes_preserve_recent_write_hold() {
        use crate::model::{AppPhase, Model, State};
        use std::time::Duration;
        for (before, after) in [("ready", "working"), ("working", "ready")] {
            let (_d, p) = fixture();
            let mut model = Model::default();
            archive_worker(&p, before);
            camera_modified(&p, 1000);
            assert_eq!(
                model.observe(AppPhase::Ready, &p.observe(true), Duration::ZERO),
                State::Connected
            );
            camera_modified(&p, 1001);
            assert_eq!(
                model.observe(
                    AppPhase::Ready,
                    &p.observe(true),
                    Duration::from_millis(500)
                ),
                State::Writing
            );
            archive_worker(&p, after);
            assert_eq!(
                model.observe(
                    AppPhase::Ready,
                    &p.observe(true),
                    Duration::from_millis(1000)
                ),
                State::Writing
            );
            assert_eq!(
                model.observe(
                    AppPhase::Ready,
                    &p.observe(true),
                    Duration::from_millis(3500)
                ),
                State::Connected
            );
        }
    }
    #[test]
    fn archive_phase_change_and_simultaneous_write_are_both_observed() {
        use crate::model::{AppPhase, Model, State};
        use std::time::Duration;
        for (before, after) in [("ready", "working"), ("working", "ready")] {
            let (_d, p) = fixture();
            let mut model = Model::default();
            archive_worker(&p, before);
            camera_modified(&p, 1000);
            model.observe(AppPhase::Ready, &p.observe(true), Duration::ZERO);
            archive_worker(&p, after);
            camera_modified(&p, 1001);
            assert_eq!(
                model.observe(
                    AppPhase::Ready,
                    &p.observe(true),
                    Duration::from_millis(500)
                ),
                State::Writing
            );
        }
    }
    #[test]
    fn full_maintenance_cycle_stays_preparing_then_recovers_or_faults() {
        use crate::model::{AppPhase, Model, State};
        use std::{os::fd::AsRawFd, time::Duration};
        for reconnect in [true, false] {
            let (_d, p) = fixture();
            archive_worker(&p, "ready");
            let mut model = Model::default();
            camera_modified(&p, 1000);
            model.observe(AppPhase::Ready, &p.observe(true), Duration::ZERO);
            // This is the same kernel flock held by CycleGuard across the API's
            // disable/work/enable window. Use only a disposable fixture file.
            let lock = fs::File::create(&p.cycle_lock).unwrap();
            assert_eq!(
                unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            write(
                &p.proc.join("locks"),
                &fs::read_to_string("/proc/locks").unwrap(),
            );
            camera_modified(&p, 1001);
            assert_eq!(
                model.observe(
                    AppPhase::Ready,
                    &p.observe(true),
                    Duration::from_millis(500)
                ),
                State::Writing
            );
            write(&p.gadget.join("UDC"), "");
            assert_eq!(
                model.observe(AppPhase::Ready, &p.observe(true), Duration::from_secs(1)),
                State::Preparing
            );
            assert_eq!(
                model.observe(AppPhase::Ready, &p.observe(true), Duration::from_secs(90)),
                State::Preparing
            );
            drop(lock); // Error/cancellation also releases the kernel-owned lifetime.
            write(
                &p.proc.join("locks"),
                &fs::read_to_string("/proc/locks").unwrap(),
            );
            if reconnect {
                write(&p.gadget.join("UDC"), "usb-a\n");
                camera_modified(&p, 2000);
                assert_eq!(
                    model.observe(AppPhase::Ready, &p.observe(true), Duration::from_secs(91)),
                    State::Connected
                );
            } else {
                assert_eq!(
                    model.observe(AppPhase::Ready, &p.observe(true), Duration::from_secs(91)),
                    State::Preparing
                );
                assert_eq!(
                    model.observe(AppPhase::Ready, &p.observe(true), Duration::from_secs(106)),
                    State::Attention
                );
            }
        }
    }
    #[test]
    fn only_an_owned_flock_on_the_exact_cycle_file_counts_as_maintenance() {
        let (_d, p) = fixture();
        fs::write(&p.cycle_lock, "").unwrap();
        let meta = fs::metadata(&p.cycle_lock).unwrap();
        let identity = format!(
            "{:02x}:{:02x}:{}",
            libc::major(meta.dev()),
            libc::minor(meta.dev()),
            meta.ino()
        );
        for record in [
            format!("1: -> FLOCK ADVISORY WRITE 42 {identity} 0 EOF\n"),
            format!("1: POSIX ADVISORY WRITE 42 {identity} 0 EOF\n"),
            format!("1: FLOCK ADVISORY READ 42 {identity} 0 EOF\n"),
            format!("1: FLOCK ADVISORY WRITE 42 ff:ff:{} 0 EOF\n", meta.ino()),
            "malformed\n".into(),
        ] {
            write(&p.proc.join("locks"), &record);
            assert!(
                !p.observe(true).maintenance,
                "unrelated or blocked lock counted: {record}"
            );
        }
        fs::remove_file(p.proc.join("locks")).unwrap();
        assert!(!p.observe(true).maintenance);
    }
}
