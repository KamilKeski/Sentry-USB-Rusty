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
        }
    }
}

pub fn process_start(proc: &Path, pid: u32) -> Option<String> {
    let stat = fs::read_to_string(proc.join(pid.to_string()).join("stat")).ok()?;
    // comm may contain spaces and parentheses; fields after its final ')' are stable.
    let tail = stat
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    if matches!(*tail.first()?, "Z" | "X" | "x") {
        return None;
    }
    Some(tail.get(19)?.to_string())
}

impl Paths {
    pub fn archive_phase(&self) -> ArchivePhase {
        let text = fs::read_to_string(self.run.join("archive")).unwrap_or_default();
        let parts = text.split_whitespace().collect::<Vec<_>>();
        if parts.len() != 3 {
            return ArchivePhase::Missing;
        }
        let Some(pid) = parts[0].parse::<u32>().ok().filter(|pid| *pid > 1) else {
            return ArchivePhase::Missing;
        };
        if process_start(&self.proc, pid).as_deref() != Some(parts[1]) {
            return ArchivePhase::Missing;
        }
        match parts[2] {
            "starting" => ArchivePhase::Starting,
            "working" => ArchivePhase::Working,
            "ready" => ArchivePhase::Ready,
            "fault" => ArchivePhase::Fault,
            _ => ArchivePhase::Missing,
        }
    }
    pub fn observe(&self, storage_ready: bool) -> Observation {
        let mut o = Observation {
            archive: self.archive_phase(),
            storage_ready,
            link: Link::Unbound,
            session: None,
            camera: None,
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
            let worker = fs::read_to_string(self.run.join("archive")).unwrap_or_default();
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
}
