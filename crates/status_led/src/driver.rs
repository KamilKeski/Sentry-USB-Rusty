//! Exclusive, reversible LED ownership. Linux sysfs handles GPIO polarity.
use crate::{model::Frame, observe::Paths};
use serde::{Deserialize, Serialize};
use std::os::{
    fd::AsRawFd,
    unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
};
use std::{collections::BTreeMap, fs, io, path::Path};

#[derive(Clone, Serialize, Deserialize)]
struct Saved {
    name: String,
    trigger: String,
    brightness: u32,
    maximum: u32,
    attributes: BTreeMap<String, String>,
}
pub struct Driver {
    paths: Paths,
    saved: Vec<Saved>,
    lock: Option<fs::File>,
    last: Option<Frame>,
}
pub(crate) fn ensure_run(run: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(run)?;
    let m = fs::symlink_metadata(run)?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
        return Err(io::Error::other("unsafe LED runtime directory"));
    }
    Ok(())
}
pub(crate) fn atomic_write(path: &Path, content: &[u8]) -> io::Result<()> {
    // All writers hold ownership or use process-specific files in a private directory.
    let tmp = path.with_extension(format!("new-{}", std::process::id()));
    let result = (|| {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(content)?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
fn lock(paths: &Paths) -> io::Result<Option<fs::File>> {
    ensure_run(&paths.run)?;
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(paths.run.join("owner.lock"))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(e);
    }
    Ok(Some(f))
}
fn capture(paths: &Paths, name: &str) -> io::Result<Saved> {
    let p = paths.leds.join(name);
    let text = fs::read_to_string(p.join("trigger"))?;
    let trigger = text
        .split_whitespace()
        .find_map(|s| s.strip_prefix('[')?.strip_suffix(']'))
        .or_else(|| {
            if text.split_whitespace().count() == 1 {
                Some(text.trim())
            } else {
                None
            }
        })
        .ok_or_else(|| io::Error::other("LED has no active trigger"))?
        .to_string();
    let brightness = fs::read_to_string(p.join("brightness"))?
        .trim()
        .parse::<u32>()
        .map_err(io::Error::other)?;
    let maximum = fs::read_to_string(p.join("max_brightness"))?
        .trim()
        .parse::<u32>()
        .map_err(io::Error::other)?;
    if maximum == 0 || brightness > maximum {
        return Err(io::Error::other("invalid LED brightness range"));
    }
    let mut attributes = BTreeMap::new();
    for attr in match trigger.as_str() {
        "timer" => vec!["delay_on", "delay_off"],
        "heartbeat" => vec!["invert"],
        _ => vec![],
    } {
        attributes.insert(attr.into(), fs::read_to_string(p.join(attr))?);
    }
    Ok(Saved {
        name: name.into(),
        trigger,
        brightness,
        maximum,
        attributes,
    })
}
fn restore_saved(paths: &Paths, saved: &[Saved]) -> io::Result<()> {
    let mut error = None;
    for s in saved {
        let r = (|| {
            if ![
                "ACT",
                "led0",
                "status",
                "user-led2",
                "radxa-zero:green",
                "PWR",
            ]
            .contains(&s.name.as_str())
            {
                return Err(io::Error::other("invalid saved LED name"));
            }
            let p = paths.leds.join(&s.name);
            fs::write(p.join("trigger"), "none")?;
            fs::write(p.join("brightness"), s.brightness.to_string())?;
            fs::write(p.join("trigger"), &s.trigger)?;
            for (a, v) in &s.attributes {
                if !["delay_on", "delay_off", "invert"].contains(&a.as_str()) {
                    return Err(io::Error::other("invalid saved LED attribute"));
                }
                fs::write(p.join(a), v)?;
            }
            Ok(())
        })();
        if let Err(e) = r {
            error = Some(e);
        }
    }
    if let Some(e) = error { Err(e) } else { Ok(()) }
}
fn restore_locked(paths: &Paths) -> io::Result<()> {
    let path = paths.run.join("restore.json");
    let content = match fs::read(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let saved: Vec<Saved> = serde_json::from_slice(&content).map_err(io::Error::other)?;
    restore_saved(paths, &saved)?;
    fs::remove_file(path)
}
impl Driver {
    pub fn claim(paths: &Paths) -> io::Result<Option<Self>> {
        let Some(name) = ["ACT", "led0", "status", "user-led2", "radxa-zero:green"]
            .into_iter()
            .find(|n| paths.leds.join(n).is_dir())
        else {
            return Ok(None);
        };
        let Some(lock) = lock(paths)? else {
            return Ok(None);
        };
        // Recover an earlier crashed owner before taking a fresh native-state snapshot.
        restore_locked(paths)?;
        let mut saved = vec![capture(paths, name)?];
        let model = fs::read_to_string(&paths.model).unwrap_or_default();
        if model
            .trim_end_matches('\0')
            .starts_with("Raspberry Pi 5 Model B")
            && name == "ACT"
            && paths.leds.join("PWR").is_dir()
        {
            saved.push(capture(paths, "PWR")?);
        }
        atomic_write(
            &paths.run.join("restore.json"),
            &serde_json::to_vec(&saved).map_err(io::Error::other)?,
        )?;
        let owner = Self {
            paths: paths.clone(),
            saved,
            lock: Some(lock),
            last: None,
        };
        for s in &owner.saved {
            fs::write(paths.leds.join(&s.name).join("trigger"), "none")?;
        }
        Ok(Some(owner))
    }
    pub fn set(&mut self, frame: Frame) -> io::Result<()> {
        if self.last == Some(frame) {
            return Ok(());
        }
        let colour = self.saved.len() == 2;
        for (i, s) in self.saved.iter().enumerate() {
            let on = if i == 1 {
                frame.red
            } else if colour {
                frame.green
            } else {
                frame.green || frame.red
            };
            fs::write(
                self.paths.leds.join(&s.name).join("brightness"),
                if on {
                    s.maximum.to_string()
                } else {
                    "0".into()
                },
            )?;
        }
        self.last = Some(frame);
        Ok(())
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        if self.lock.is_some() && restore_saved(&self.paths, &self.saved).is_ok() {
            let _ = fs::remove_file(self.paths.run.join("restore.json"));
        }
    }
}
pub fn restore(paths: &Paths) -> io::Result<()> {
    if !paths.run.exists() {
        return Ok(());
    }
    let Some(_lock) = lock(paths)? else {
        return Ok(());
    };
    restore_locked(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn led(p: &Path) {
        fs::create_dir_all(p).unwrap();
        fs::write(p.join("trigger"), "none [mmc0] timer").unwrap();
        fs::write(p.join("brightness"), "0").unwrap();
        fs::write(p.join("max_brightness"), "255").unwrap();
    }
    fn fixture(model: &str) -> (tempfile::TempDir, Paths) {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let p = Paths {
            leds: r.join("leds"),
            model: r.join("model"),
            run: r.join("run"),
            ..Paths::default()
        };
        fs::write(&p.model, model).unwrap();
        led(&p.leds.join("ACT"));
        led(&p.leds.join("PWR"));
        (d, p)
    }
    fn brightness(p: &Paths, name: &str) -> String {
        fs::read_to_string(p.leds.join(name).join("brightness")).unwrap()
    }
    #[test]
    fn pi5_can_show_amber_green_red_and_restores_original_triggers() {
        let (_d, p) = fixture("Raspberry Pi 5 Model B Rev 1.0\0");
        let mut owner = Driver::claim(&p).unwrap().unwrap();
        owner
            .set(Frame {
                green: true,
                red: true,
            })
            .unwrap();
        assert_eq!(brightness(&p, "ACT"), "255");
        assert_eq!(brightness(&p, "PWR"), "255");
        owner
            .set(Frame {
                green: true,
                red: false,
            })
            .unwrap();
        assert_eq!(brightness(&p, "PWR"), "0");
        owner
            .set(Frame {
                green: false,
                red: true,
            })
            .unwrap();
        assert_eq!(brightness(&p, "ACT"), "0");
        drop(owner);
        assert_eq!(
            fs::read_to_string(p.leds.join("ACT/trigger")).unwrap(),
            "mmc0"
        );
        assert_eq!(brightness(&p, "PWR"), "0");
    }
    #[test]
    fn other_boards_never_take_power_led_and_red_fault_falls_back_to_green() {
        for model in ["Raspberry Pi 4 Model B", "Raspberry Pi Zero 2 W", "unknown"] {
            let (_d, p) = fixture(model);
            let mut owner = Driver::claim(&p).unwrap().unwrap();
            owner
                .set(Frame {
                    green: false,
                    red: true,
                })
                .unwrap();
            assert_eq!(brightness(&p, "ACT"), "255");
            assert_eq!(
                fs::read_to_string(p.leds.join("PWR/trigger")).unwrap(),
                "none [mmc0] timer"
            );
            assert_eq!(brightness(&p, "PWR"), "0");
        }
    }
    #[test]
    fn second_owner_and_cleanup_cannot_override_live_owner() {
        let (_d, p) = fixture("Raspberry Pi 5 Model B");
        let mut owner = Driver::claim(&p).unwrap().unwrap();
        owner
            .set(Frame {
                green: true,
                red: true,
            })
            .unwrap();
        assert!(Driver::claim(&p).unwrap().is_none());
        restore(&p).unwrap();
        assert_eq!(brightness(&p, "ACT"), "255");
        drop(owner);
        restore(&p).unwrap();
        assert_eq!(brightness(&p, "ACT"), "0");
    }
    #[test]
    fn partial_claim_failure_rolls_back_activity_led() {
        let (_d, p) = fixture("Raspberry Pi 5 Model B");
        fs::remove_file(p.leds.join("PWR/trigger")).unwrap();
        fs::create_dir(p.leds.join("PWR/trigger")).unwrap();
        assert!(Driver::claim(&p).is_err());
        assert_eq!(
            fs::read_to_string(p.leds.join("ACT/trigger")).unwrap(),
            "none [mmc0] timer"
        );
    }
    #[test]
    fn missing_led_is_a_supported_noop() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths {
            leds: d.path().join("absent"),
            run: d.path().join("run"),
            ..Paths::default()
        };
        assert!(Driver::claim(&p).unwrap().is_none());
    }
    #[test]
    fn crash_snapshot_is_recovered_before_reacquiring() {
        let (_d, p) = fixture("Raspberry Pi 5 Model B");
        let mut owner = Driver::claim(&p).unwrap().unwrap();
        owner
            .set(Frame {
                green: true,
                red: true,
            })
            .unwrap();
        // Simulate process death: release flock without running LED cleanup.
        drop(owner.lock.take());
        std::mem::forget(owner);
        let next = Driver::claim(&p).unwrap().unwrap();
        drop(next);
        assert_eq!(brightness(&p, "ACT"), "0");
        assert_eq!(brightness(&p, "PWR"), "0");
        assert_eq!(
            fs::read_to_string(p.leds.join("ACT/trigger")).unwrap(),
            "mmc0"
        );
    }
}
