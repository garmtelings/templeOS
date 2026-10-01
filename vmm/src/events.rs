//! What both machines do with the board's news between runs: report the PC
//! speaker's pitch, print the SeaBIOS debug console, recognize boot
//! milestones and pass on reset requests.

use std::io::Write;
use std::time::Instant;

use devices::pc::Pc;

use crate::types::{Milestone, SpeakerFn, Stop};

/// The board's news since the last look, taken with the board locked.
pub(crate) struct BoardNews {
    speaker_hz: Option<f64>,
    debugcon: Vec<u8>,
    kernel_timer: bool,
    reset: bool,
}

impl BoardNews {
    pub(crate) fn take(pc: &mut Pc) -> Self {
        BoardNews {
            speaker_hz: pc.speaker_hz(),
            debugcon: pc.take_debugcon(),
            kernel_timer: pc.kernel_timer_programmed(),
            reset: pc.take_reset_request(),
        }
    }
}

pub(crate) struct Events {
    start: Instant,
    pub(crate) reached: Vec<Milestone>,
    /// Debug console output not yet split into lines.
    debugcon_line: Vec<u8>,
    debugcon_sink: Option<Box<dyn Write>>,
    echo_debugcon: bool,
    pub(crate) speaker: Option<SpeakerFn>,
    speaker_hz: Option<f64>,
}

impl Events {
    pub(crate) fn new(start: Instant) -> Self {
        Events {
            start,
            reached: Vec::new(),
            debugcon_line: Vec::new(),
            debugcon_sink: None,
            echo_debugcon: true,
            speaker: None,
            speaker_hz: None,
        }
    }

    pub(crate) fn set_debugcon_sink(&mut self, w: Box<dyn Write>, echo: bool) {
        self.debugcon_sink = Some(w);
        self.echo_debugcon = echo;
    }

    /// Act on `news`; `ap_started` is whether an application processor has
    /// been started. Returns why the run loop must stop, if it must.
    pub(crate) fn handle(&mut self, news: BoardNews, ap_started: bool, until: Option<Milestone>) -> Option<Stop> {
        if news.speaker_hz != self.speaker_hz {
            self.speaker_hz = news.speaker_hz;
            if std::env::var_os("TEMPLEOS_DEBUG").is_some() {
                eprintln!("[speaker {:9.3}s] {:?}", self.start.elapsed().as_secs_f64(), news.speaker_hz);
            }
            if let Some(f) = &mut self.speaker {
                f(news.speaker_hz);
            }
        }
        if !news.debugcon.is_empty() {
            self.debugcon(&news.debugcon);
        }
        if news.kernel_timer && self.reached.contains(&Milestone::LongMode) {
            self.reach(Milestone::KernelTimers);
        }
        if ap_started {
            self.reach(Milestone::ApStarted);
        }
        if news.reset {
            return Some(Stop::ResetRequested);
        }
        until.filter(|m| self.reached.contains(m)).map(Stop::Milestone)
    }

    pub(crate) fn reach(&mut self, m: Milestone) {
        if !self.reached.contains(&m) {
            self.reached.push(m);
            let t = self.start.elapsed().as_secs_f64();
            println!("[vmm {t:8.3}s] milestone: {}", m.describe());
        }
    }

    fn debugcon(&mut self, bytes: &[u8]) {
        if let Some(w) = &mut self.debugcon_sink {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
        for &b in bytes {
            if b != b'\n' {
                self.debugcon_line.push(b);
                continue;
            }
            let line = String::from_utf8_lossy(&self.debugcon_line).into_owned();
            self.debugcon_line.clear();
            if self.echo_debugcon {
                println!("[debugcon] {line}");
            }
            if line.starts_with("SeaBIOS (version") {
                self.reach(Milestone::BiosBanner);
            }
        }
    }
}
