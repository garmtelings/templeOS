//! The PC speaker on the host's default audio device (WASAPI, shared mode).
//!
//! The VM thread reports each tone change ([`Speaker::set`]) with the wall
//! time it happened. The audio thread plays a square wave and applies each
//! change a fixed [`DELAY`] after it happened, at the exact sample that
//! corresponds to that moment, so note lengths and rhythm come out as the
//! guest timed them however the VM and audio threads are scheduled.
//!
//! The guest can't observe any of this; if there is no audio device the
//! speaker is simply silent.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

const RATE: u32 = 48_000;
/// How long after the guest changes the tone it is heard. Must exceed the
/// device buffer (~20 ms) plus scheduling jitter so no change arrives late.
const DELAY: Duration = Duration::from_millis(60);
/// Square wave amplitude (full scale is 1.0).
const VOLUME: f32 = 0.15;

pub struct Speaker {
    /// Tone changes not yet played: (when, Hz; 0 = silent).
    events: Mutex<VecDeque<(Instant, f64)>>,
    stop: AtomicBool,
}

impl Speaker {
    pub fn new() -> Arc<Self> {
        Arc::new(Speaker { events: Mutex::new(VecDeque::new()), stop: AtomicBool::new(false) })
    }

    /// The speaker now plays `hz` (None = silent). Called from the VM thread.
    pub fn set(&self, hz: Option<f64>) {
        self.events.lock().unwrap().push_back((Instant::now(), hz.unwrap_or(0.0)));
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Start the audio thread. Failures (no device, no WASAPI) are reported
/// once on stderr and leave the speaker silent.
pub fn start(speaker: Arc<Speaker>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("pc-speaker".into())
        .spawn(move || {
            if let Err(e) = play(&speaker) {
                eprintln!("[audio] PC speaker disabled: {e}");
            }
        })
        .expect("spawn audio thread")
}

/// Square wave generator with sample-accurate tone changes.
struct Tone {
    hz: f64,
    phase: f64,
}

impl Tone {
    fn sample(&mut self) -> f32 {
        // Silence, and tones above what the output can carry (the speaker's
        // "ultrasonic = off" trick), give 0.
        if self.hz <= 0.0 || self.hz >= f64::from(RATE) / 2.0 {
            self.phase = 0.0;
            return 0.0;
        }
        let v = if self.phase < 0.5 { VOLUME } else { -VOLUME };
        self.phase = (self.phase + self.hz / f64::from(RATE)).fract();
        v
    }
}

fn play(speaker: &Speaker) -> windows::core::Result<()> {
    // SAFETY: COM and WASAPI calls on this thread; every pointer passed is
    // valid for the call, and the buffer from GetBuffer is written only up to
    // the frame count requested.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        // 48 kHz mono 32-bit float; the audio engine converts to the mix format.
        let format = WAVEFORMATEX {
            wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
            nChannels: 1,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 4,
            nBlockAlign: 4,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            200_000, // 20 ms, in 100 ns units
            0,
            &format,
            None,
        )?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let buffer_frames = client.GetBufferSize()?;
        let render: IAudioRenderClient = client.GetService()?;
        client.Start()?;

        let mut epoch = Instant::now();
        let mut written: u64 = 0;
        let mut tone = Tone { hz: 0.0, phase: 0.0 };
        while !speaker.stop.load(Ordering::Acquire) {
            if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                continue;
            }
            let padding = client.GetCurrentPadding()?;
            let n = buffer_frames.saturating_sub(padding);
            if n == 0 {
                continue;
            }
            // The device clock and Instant drift apart slowly; re-anchor the
            // sample timeline when they differ by more than 30 ms (a single
            // small jump in timing instead of an ever-growing delay).
            let queued = Duration::from_nanos(u64::from(padding) * 1_000_000_000 / u64::from(RATE));
            let now = Instant::now() + queued;
            let at = epoch + Duration::from_nanos(written * 1_000_000_000 / u64::from(RATE));
            let skew = if at > now { at - now } else { now - at };
            if skew > Duration::from_millis(30) {
                epoch = now - Duration::from_nanos(written * 1_000_000_000 / u64::from(RATE));
            }
            let data = render.GetBuffer(n)? as *mut f32;
            let out = std::slice::from_raw_parts_mut(data, n as usize);
            let mut events = speaker.events.lock().unwrap();
            for (i, s) in out.iter_mut().enumerate() {
                // Sample `written + i` plays DELAY after this moment.
                let t = epoch + Duration::from_nanos((written + i as u64) * 1_000_000_000 / u64::from(RATE));
                while let Some(&(at, hz)) = events.front() {
                    if at + DELAY > t {
                        break;
                    }
                    tone.hz = hz;
                    events.pop_front();
                }
                *s = tone.sample();
            }
            drop(events);
            render.ReleaseBuffer(n, 0)?;
            written += u64::from(n);
        }
        let _ = client.Stop();
        let _ = CloseHandle(event);
    }
    Ok(())
}
