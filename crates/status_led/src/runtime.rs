use crate::{
    driver::{self, Driver},
    model::{AppPhase, Model, State},
    observe::Paths,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Inner {
    phase: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
}
impl Inner {
    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.lock().unwrap_or_else(|p| p.into_inner()).take() {
            // Broken storage can stall a metadata syscall. Never let an advisory
            // observer block the server's shutdown; systemd performs crash cleanup.
            let deadline = Instant::now() + Duration::from_millis(250);
            while !join.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if join.is_finished() {
                let _ = join.join();
            }
        }
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.stop();
    }
}
#[derive(Clone)]
pub struct Controller(Arc<Inner>);
impl Controller {
    /// Starts on supported hardware, deferring ownership while a legacy archive
    /// worker is running. A refreshed script does not replace that worker's code.
    /// The callback must only inspect storage. A detached LED never gates startup.
    pub fn start(storage_ready: impl Fn() -> bool + Send + 'static) -> Option<Self> {
        Self::start_at(Paths::default(), storage_ready)
    }
    fn start_at(paths: Paths, storage_ready: impl Fn() -> bool + Send + 'static) -> Option<Self> {
        if !Driver::available(&paths) {
            return None;
        }
        let phase = Arc::new(AtomicU8::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_phase, thread_stop) = (phase.clone(), stop.clone());
        let join = thread::Builder::new()
            .name("status-led".into())
            .spawn(move || {
                let mut driver: Option<Driver> = None;
                let mut announced_wait = false;
                let mut model = Model::default();
                let start = Instant::now();
                let mut state = State::Starting;
                let mut changed = Instant::now();
                let mut next_sample = Instant::now();
                while !thread_stop.load(Ordering::Acquire) {
                    let now = Instant::now();
                    if now >= next_sample {
                        next_sample = now + Duration::from_millis(500);
                        if driver.is_none() {
                            match Driver::claim(&paths) {
                                Ok(Some(owner)) => { driver = Some(owner); changed = now; }
                                Ok(None) => {
                                    if !announced_wait {
                                        eprintln!("status LED waiting for archive handoff or LED ownership");
                                        announced_wait = true;
                                    }
                                    thread::sleep(Duration::from_millis(20));
                                    continue;
                                }
                                Err(e) => { eprintln!("status LED unavailable: {e}"); break; }
                            }
                        }
                        let app = match thread_phase.load(Ordering::Acquire) {
                            0 => AppPhase::Starting,
                            1 => AppPhase::Ready,
                            _ => AppPhase::Fault,
                        };
                        let observation = paths.observe(if app == AppPhase::Starting {
                            false
                        } else {
                            storage_ready()
                        });
                        let new = model.observe(app, &observation, start.elapsed());
                        if new != state {
                            state = new;
                            changed = now;
                        }
                    }
                    if thread_stop.load(Ordering::Acquire) { break; }
                    if let Some(driver) = &mut driver {
                        if let Err(e) = driver.set(state.frame(changed.elapsed())) {
                            eprintln!("status LED disabled: {e}");
                            break;
                        }
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                // Driver drop restores the captured triggers, including during normal shutdown.
            })
            .ok()?;
        Some(Self(Arc::new(Inner {
            phase,
            stop,
            join: Mutex::new(Some(join)),
        })))
    }
    pub fn ready(&self) {
        let _ = self
            .0
            .phase
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    }
    pub fn fault(&self) {
        self.0.phase.store(2, Ordering::Release);
    }
    pub fn stop(&self) {
        self.0.stop();
    }
}

/// Brackets real USB gadget mutations so local image maintenance cannot be
/// mistaken for host writes, even if unbind/rebind happens between samples.
pub struct MediaTransition;
fn media_generation(phase: &str) {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let paths = Paths::default();
    if driver::ensure_run(&paths.run).is_err() {
        return;
    }
    let pid = std::process::id();
    let start = crate::observe::process_start(&paths.proc, pid).unwrap_or_default();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let _ = driver::atomic_write(
        &paths.run.join("media-generation"),
        format!("{phase} {pid}:{start}:{sequence}\n").as_bytes(),
    );
}
impl MediaTransition {
    pub fn begin() -> Self {
        media_generation("busy");
        Self
    }
}
impl Drop for MediaTransition {
    fn drop(&mut self) {
        media_generation("ready");
    }
}
pub fn restore() {
    if let Err(e) = driver::restore(&Paths::default()) {
        eprintln!("status LED restore: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_stalled_observation_cannot_hold_up_shutdown() {
        let (send, recv) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let _ = recv.recv_timeout(Duration::from_secs(3));
        });
        let inner = Inner {
            phase: Arc::new(AtomicU8::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            join: Mutex::new(Some(worker)),
        };
        let start = Instant::now();
        inner.stop();
        let elapsed = start.elapsed();
        let _ = send.send(());
        assert!(
            elapsed < Duration::from_secs(1),
            "LED sampling blocked shutdown for {elapsed:?}"
        );
    }
    #[test]
    fn announcing_http_ready_cannot_clear_a_degraded_database_fault() {
        let c = Controller(Arc::new(Inner {
            phase: Arc::new(AtomicU8::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            join: Mutex::new(None),
        }));
        c.fault();
        c.ready();
        assert_eq!(c.0.phase.load(Ordering::Acquire), 2);
    }
    #[test]
    fn controller_retries_after_legacy_worker_exits_without_server_restart() {
        use std::fs;
        let fixture = tempfile::tempdir().unwrap();
        let p = Paths {
            leds: fixture.path().join("leds"),
            model: fixture.path().join("model"),
            run: fixture.path().join("run"),
            proc: fixture.path().join("proc"),
            archive_script: fixture.path().join("archiveloop"),
            gadget: fixture.path().join("gadget"),
            cycle_lock: fixture.path().join("cycle.lock"),
            ..Paths::default()
        };
        fs::create_dir_all(p.leds.join("ACT")).unwrap();
        fs::write(p.leds.join("ACT/trigger"), "none [mmc0] timer").unwrap();
        fs::write(p.leds.join("ACT/brightness"), "0").unwrap();
        fs::write(p.leds.join("ACT/max_brightness"), "1").unwrap();
        fs::write(
            &p.archive_script,
            "# sentryusb-status-led: cooperative-v1\n",
        )
        .unwrap();
        fs::create_dir_all(p.proc.join("42")).unwrap();
        fs::write(p.proc.join("42/cmdline"), b"bash\0/root/bin/archiveloop\0").unwrap();
        fs::write(
            p.proc.join("42/stat"),
            format!("42 (archiveloop) S {} 123 0", vec!["0"; 18].join(" ")),
        )
        .unwrap();
        let controller =
            Controller::start_at(p.clone(), || true).expect("deferred controller can keep waiting");
        thread::sleep(Duration::from_millis(80));
        assert_eq!(
            fs::read_to_string(p.leds.join("ACT/trigger")).unwrap(),
            "none [mmc0] timer"
        );
        fs::remove_dir_all(p.proc.join("42")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while fs::read_to_string(p.leds.join("ACT/trigger")).unwrap() != "none"
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            fs::read_to_string(p.leds.join("ACT/trigger")).unwrap(),
            "none"
        );
        controller.stop();
        assert_eq!(
            fs::read_to_string(p.leds.join("ACT/trigger")).unwrap(),
            "mmc0"
        );
    }
}
