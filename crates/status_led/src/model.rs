use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppPhase {
    Starting,
    Ready,
    Fault,
    Stopping,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchivePhase {
    Missing,
    Starting,
    Working,
    Ready,
    Fault,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Unbound,
    Waiting,
    Configured,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Starting,
    Preparing,
    Waiting,
    Connected,
    Writing,
    Attention,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub green: bool,
    pub red: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraStamp {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub modified_ns: i128,
}
#[derive(Clone, Debug)]
pub struct Observation {
    pub archive: ArchivePhase,
    pub storage_ready: bool,
    pub link: Link,
    /// Exact UDC, LUN identity and media-transition generation.
    pub session: Option<String>,
    pub camera: Option<CameraStamp>,
}
#[derive(Default)]
pub struct Model {
    previous: Option<(String, CameraStamp)>,
    writing_until: Option<Duration>,
    missing_since: Option<Duration>,
    unbound_since: Option<Duration>,
    worker_seen: bool,
}
impl Model {
    pub fn observe(&mut self, app: AppPhase, o: &Observation, now: Duration) -> State {
        if o.archive != ArchivePhase::Missing {
            self.missing_since = None;
            self.worker_seen = true;
        }
        if o.link != Link::Unbound || o.archive != ArchivePhase::Ready {
            self.unbound_since = None;
        }
        let state = match app {
            AppPhase::Starting | AppPhase::Stopping => State::Starting,
            AppPhase::Fault => State::Attention,
            AppPhase::Ready => match o.archive {
                ArchivePhase::Fault => State::Attention,
                ArchivePhase::Missing => {
                    let since = *self.missing_since.get_or_insert(now);
                    if self.worker_seen || now.saturating_sub(since) >= Duration::from_secs(60) {
                        State::Attention
                    } else {
                        State::Preparing
                    }
                }
                _ if !o.storage_ready => State::Attention,
                ArchivePhase::Starting => State::Preparing,
                _ => match o.link {
                    Link::Configured => State::Connected,
                    Link::Waiting => State::Waiting,
                    Link::Unknown => State::Attention,
                    Link::Unbound if o.archive == ArchivePhase::Working => State::Preparing,
                    Link::Unbound => {
                        let since = *self.unbound_since.get_or_insert(now);
                        if now.saturating_sub(since) >= Duration::from_secs(15) {
                            State::Attention
                        } else {
                            State::Preparing
                        }
                    }
                },
            },
        };
        if state != State::Connected {
            self.previous = None;
            self.writing_until = None;
            return state;
        }
        match (&o.session, &o.camera) {
            (Some(session), Some(stamp)) => {
                match self.previous.as_ref() {
                    Some((old_session, old))
                        if old_session == session
                            && old.device == stamp.device
                            && old.inode == stamp.inode
                            && old.size == stamp.size
                            && stamp.modified_ns >= old.modified_ns =>
                    {
                        if stamp.modified_ns > old.modified_ns {
                            self.writing_until = Some(now + Duration::from_secs(3));
                        }
                    }
                    _ => self.writing_until = None,
                }
                self.previous = Some((session.clone(), stamp.clone()));
            }
            _ => {
                self.previous = None;
                self.writing_until = None;
            }
        }
        if self.writing_until.is_some_and(|until| now < until) {
            State::Writing
        } else {
            state
        }
    }
}
impl State {
    pub fn frame(self, elapsed: Duration) -> Frame {
        let ms = elapsed.as_millis();
        let on = match self {
            Self::Starting => ms % 2000 < 1000,
            Self::Preparing => ms % 500 < 250,
            Self::Waiting => ms % 3000 < 100,
            Self::Connected => {
                let p = ms % 3000;
                p < 100 || (250..350).contains(&p)
            }
            Self::Writing => ms % 500 < 400,
            Self::Attention => ms % 3000 < 2800,
        };
        Frame {
            green: on && self != Self::Attention,
            red: on && matches!(self, Self::Starting | Self::Preparing | Self::Attention),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn t(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }
    fn good() -> Observation {
        Observation {
            archive: ArchivePhase::Ready,
            storage_ready: true,
            link: Link::Configured,
            session: Some("udc-a/generation-1".into()),
            camera: Some(CameraStamp {
                device: 1,
                inode: 7,
                size: 1000,
                modified_ns: 1234,
            }),
        }
    }
    #[test]
    fn connected_has_exact_two_flashes_per_three_seconds() {
        for (ms, on) in [
            (0, true),
            (99, true),
            (100, false),
            (249, false),
            (250, true),
            (349, true),
            (350, false),
            (2999, false),
            (3000, true),
        ] {
            assert_eq!(
                State::Connected.frame(t(ms)),
                Frame {
                    green: on,
                    red: false
                }
            );
        }
    }
    #[test]
    fn colours_and_fallback_rhythms_are_distinct() {
        assert_eq!(
            State::Starting.frame(t(0)),
            Frame {
                green: true,
                red: true
            }
        );
        assert_eq!(
            State::Starting.frame(t(1000)),
            Frame {
                green: false,
                red: false
            }
        );
        assert!(!State::Preparing.frame(t(250)).green);
        assert!(State::Writing.frame(t(250)).green);
        assert!(!State::Writing.frame(t(400)).green);
        assert_eq!(
            State::Attention.frame(t(0)),
            Frame {
                green: false,
                red: true
            }
        );
        assert_eq!(
            State::Attention.frame(t(2800)),
            Frame {
                green: false,
                red: false
            }
        );
        assert!(!State::Waiting.frame(t(250)).green);
    }
    #[test]
    fn initial_timestamp_is_not_write_activity_and_hold_expires() {
        let mut m = Model::default();
        let mut o = good();
        assert_eq!(m.observe(AppPhase::Ready, &o, t(0)), State::Connected);
        o.camera.as_mut().unwrap().modified_ns += 1;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(500)), State::Writing);
        assert_eq!(m.observe(AppPhase::Ready, &o, t(3499)), State::Writing);
        assert_eq!(m.observe(AppPhase::Ready, &o, t(3500)), State::Connected);
    }
    #[test]
    fn disconnect_and_reconnect_do_not_replay_old_writes() {
        let mut m = Model::default();
        let mut o = good();
        m.observe(AppPhase::Ready, &o, t(0));
        o.link = Link::Waiting;
        o.camera.as_mut().unwrap().modified_ns += 1;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(100)), State::Waiting);
        o.link = Link::Configured;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(200)), State::Connected);
        o.session = Some("udc-a/generation-2".into());
        o.camera.as_mut().unwrap().modified_ns += 1;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(300)), State::Connected);
    }
    #[test]
    fn replacement_missing_camera_and_backward_time_reset_activity() {
        let mut m = Model::default();
        let mut o = good();
        m.observe(AppPhase::Ready, &o, t(0));
        o.camera.as_mut().unwrap().inode += 1;
        o.camera.as_mut().unwrap().modified_ns += 100;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(500)), State::Connected);
        o.camera.as_mut().unwrap().modified_ns -= 50;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(1000)), State::Connected);
        o.camera = None;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(1500)), State::Connected);
    }
    #[test]
    fn faults_and_startup_override_connected_hardware() {
        let mut m = Model::default();
        let mut o = good();
        assert_eq!(m.observe(AppPhase::Starting, &o, t(0)), State::Starting);
        assert_eq!(m.observe(AppPhase::Fault, &o, t(0)), State::Attention);
        o.archive = ArchivePhase::Fault;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(0)), State::Attention);
        o.archive = ArchivePhase::Ready;
        o.storage_ready = false;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(0)), State::Attention);
        o.storage_ready = true;
        o.link = Link::Unknown;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(0)), State::Attention);
    }
    #[test]
    fn missing_worker_and_failed_presentation_cannot_wait_forever() {
        let mut m = Model::default();
        let mut o = good();
        o.archive = ArchivePhase::Missing;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(0)), State::Preparing);
        assert_eq!(m.observe(AppPhase::Ready, &o, t(60000)), State::Attention);
        o.archive = ArchivePhase::Ready;
        o.link = Link::Unbound;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(61000)), State::Preparing);
        assert_eq!(m.observe(AppPhase::Ready, &o, t(76000)), State::Attention);
    }
    #[test]
    fn archive_work_with_connected_usb_does_not_hide_writes() {
        let mut m = Model::default();
        let mut o = good();
        o.archive = ArchivePhase::Working;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(0)), State::Connected);
        o.camera.as_mut().unwrap().modified_ns += 1;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(500)), State::Writing);
        o.link = Link::Unbound;
        assert_eq!(m.observe(AppPhase::Ready, &o, t(1000)), State::Preparing);
    }
}
