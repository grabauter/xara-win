#[cfg(feature = "alsa")]
pub mod alsa;
#[cfg(feature = "pulse")]
pub mod pulse;
#[cfg(feature = "pulse")]
mod pulse_ffi;
#[cfg(feature = "wasapi")]
pub mod wasapi;

#[cfg(not(any(feature = "pulse", feature = "alsa", feature = "wasapi")))]
compile_error!("xara needs an audio backend: enable the pulse, alsa, or wasapi feature");

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

pub const RATE: u32 = 48000;
pub const BLOCK: usize = 480;
pub(crate) const STALL: Duration = Duration::from_secs(1);

pub type DeviceList = Vec<(String, String)>;

fn capture_only() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "write to a capture-only audio device")
}

#[cfg(any(feature = "pulse", feature = "alsa"))]
fn remaining(until: std::time::Instant, what: &str) -> io::Result<Duration> {
    until
        .checked_duration_since(std::time::Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, format!("{what}: timed out")))
}

pub trait Audio {
    fn read(&mut self, x: &mut [i16]) -> io::Result<()>;
    fn write(&mut self, x: &[i16]) -> io::Result<()>;
}

pub fn sample(v: i16) -> f64 {
    f64::from(v) / 32768.0
}

pub fn quantize(v: f64) -> i16 {
    (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

pub struct TxAudio {
    delay: VecDeque<f64>,
}

impl TxAudio {
    pub fn new(lead_samples: usize) -> Self {
        Self { delay: std::iter::repeat_n(0.0, lead_samples).collect() }
    }

    pub fn process(&mut self, out: &mut [f64], mute: bool) {
        if mute {
            out.fill(0.0);
            self.delay.iter_mut().for_each(|v| *v = 0.0);
        }
        if !self.delay.is_empty() {
            self.delay.extend(out.iter().copied());
            for o in out.iter_mut() {
                *o = self.delay.pop_front().unwrap();
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Pulse,
    Alsa,
    Wasapi,
}

impl Default for Backend {
    fn default() -> Self {
        if cfg!(feature = "pulse") {
            Self::Pulse
        } else if cfg!(feature = "wasapi") {
            Self::Wasapi
        } else {
            Self::Alsa
        }
    }
}

impl std::str::FromStr for Backend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let b = match s {
            "pulse" => Self::Pulse,
            "alsa" => Self::Alsa,
            "wasapi" => Self::Wasapi,
            _ => return Err(format!("--audio {s:?}: expected pulse, alsa or wasapi")),
        };
        if b.built() { Ok(b) } else { Err(format!("--audio {s}: this xara was built without the {s} feature")) }
    }
}

impl Backend {
    pub fn built(self) -> bool {
        match self {
            Self::Pulse => cfg!(feature = "pulse"),
            Self::Alsa => cfg!(feature = "alsa"),
            Self::Wasapi => cfg!(feature = "wasapi"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Pulse => "pulse",
            Self::Alsa => "alsa",
            Self::Wasapi => "wasapi",
        }
    }
}

#[cfg(not(all(feature = "pulse", feature = "alsa", feature = "wasapi")))]
fn unbuilt(b: Backend) -> io::Error {
    io::Error::other(format!("this xara was built without the {} feature", b.name()))
}

pub fn open(b: Backend, capture: &str, playback: &str, radio: bool) -> io::Result<Box<dyn Audio>> {
    match b {
        #[cfg(feature = "pulse")]
        Backend::Pulse => Ok(Box::new(pulse::PulseIo::open(capture, playback, radio)?)),
        #[cfg(feature = "alsa")]
        Backend::Alsa => Ok(Box::new(alsa::AlsaIo::open(capture, playback, radio)?)),
        #[cfg(feature = "wasapi")]
        Backend::Wasapi => Ok(Box::new(wasapi::WasapiIo::open(capture, playback, radio)?)),
        #[cfg(not(all(feature = "pulse", feature = "alsa", feature = "wasapi")))]
        _ => Err(unbuilt(b)),
    }
}

pub fn check_playback(b: Backend, playback: &str, radio: bool) -> io::Result<()> {
    match b {
        #[cfg(feature = "pulse")]
        Backend::Pulse => pulse::check(playback, radio),
        #[cfg(feature = "alsa")]
        Backend::Alsa => alsa::resolve_playback(playback, radio).map(drop),
        #[cfg(feature = "wasapi")]
        Backend::Wasapi => wasapi::check(playback, radio),
        #[cfg(not(all(feature = "pulse", feature = "alsa", feature = "wasapi")))]
        _ => Err(unbuilt(b)),
    }
}

pub fn list(b: Backend) -> io::Result<(DeviceList, DeviceList)> {
    match b {
        #[cfg(feature = "pulse")]
        Backend::Pulse => pulse::list(),
        #[cfg(feature = "alsa")]
        Backend::Alsa => alsa::list(),
        #[cfg(feature = "wasapi")]
        Backend::Wasapi => wasapi::list(),
        #[cfg(not(all(feature = "pulse", feature = "alsa", feature = "wasapi")))]
        _ => Err(unbuilt(b)),
    }
}

pub fn open_capture(b: Backend, capture: &str) -> io::Result<Box<dyn Audio>> {
    match b {
        #[cfg(feature = "pulse")]
        Backend::Pulse => Ok(Box::new(pulse::PulseIo::open_capture(capture)?)),
        #[cfg(feature = "alsa")]
        Backend::Alsa => Ok(Box::new(alsa::AlsaIo::open_capture(capture)?)),
        #[cfg(feature = "wasapi")]
        Backend::Wasapi => Ok(Box::new(wasapi::WasapiIo::open_capture(capture)?)),
        #[cfg(not(all(feature = "pulse", feature = "alsa", feature = "wasapi")))]
        _ => Err(unbuilt(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_convert_at_the_device_boundary() {
        for s in [i16::MIN, -1, 0, 1, 12345, i16::MAX] {
            assert_eq!(quantize(sample(s)), s);
        }
        assert_eq!((quantize(1.0), quantize(-1.0), quantize(4.0), quantize(-4.0)), (32767, -32768, 32767, -32768));
        assert_eq!((quantize(0.5 / 32768.0), quantize(-0.4 / 32768.0), quantize(f64::NAN)), (1, 0, 0));
        assert_eq!(sample(-32768), -1.0);
    }
}
