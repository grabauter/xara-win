//! Windows WASAPI audio via cpal, bridged into the blocking Audio contract.
//!
//! cpal drives real-time data callbacks on its own threads; those callbacks must never block,
//! so they only touch the non-blocking Spsc rings below and drop or silence on overflow. The
//! modem loop's read and write do the deadline-bounded waiting, mirroring the pulse backend.
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicI16, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{InputCallbackInfo, OutputCallbackInfo, SampleFormat, StreamError, SupportedStreamConfigRange};

use super::ratematch::{Queue, TxBuffer};
use super::{Audio, DeviceList, IO_TIMEOUT, RATE};

/// Lock-free fixed-capacity single-producer single-consumer ring of i16 samples.
/// head and tail grow monotonically (never wrapped) so head - tail is always the exact
/// fill level and full needs no reserved slot; the index is taken modulo cap. Samples
/// ride in Relaxed atomics: the Acquire/Release pair on head and tail orders them.
struct Spsc {
    buf: Vec<AtomicI16>,
    head: AtomicUsize, // producer only
    tail: AtomicUsize, // consumer only
    cap: usize,        // power of two
}

impl Spsc {
    fn new(n: usize) -> Self {
        let cap = n.next_power_of_two();
        Self {
            buf: (0..cap).map(|_| AtomicI16::new(0)).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            cap,
        }
    }

    /// Samples currently buffered; the rate matcher's queue depth.
    fn fill(&self) -> usize {
        self.head.load(Ordering::Acquire) - self.tail.load(Ordering::Relaxed)
    }

    /// Non-blocking: push as many samples as fit, return how many were written.
    fn try_push(&self, x: &[i16]) -> usize {
        let mut done = 0;
        while done < x.len() {
            let h = self.head.load(Ordering::Relaxed);
            let room = self.cap - (h - self.tail.load(Ordering::Acquire));
            if room == 0 {
                break;
            }
            let n = (x.len() - done).min(room);
            for i in 0..n {
                self.buf[(h + i) & (self.cap - 1)].store(x[done + i], Ordering::Relaxed);
            }
            self.head.store(h + n, Ordering::Release);
            done += n;
        }
        done
    }

    /// Non-blocking: pop as many samples as are ready, return how many were read.
    fn try_pop(&self, x: &mut [i16]) -> usize {
        let mut done = 0;
        while done < x.len() {
            let t = self.tail.load(Ordering::Relaxed);
            let avail = self.head.load(Ordering::Acquire) - t;
            if avail == 0 {
                break;
            }
            let n = (x.len() - done).min(avail);
            for i in 0..n {
                x[done + i] = self.buf[(t + i) & (self.cap - 1)].load(Ordering::Relaxed);
            }
            self.tail.store(t + n, Ordering::Release);
            done += n;
        }
        done
    }
}

const RING_SAMPLES: usize = 4096; // ~85 ms at 48 kHz, comfortably above one BLOCK

pub struct WasapiIo {
    in_stream: Option<cpal::Stream>,
    out_stream: Option<cpal::Stream>,
    cap: Arc<Spsc>,            // capture: the cpal thread pushes, read() pops
    play: Arc<Spsc>,           // playback: write() pushes, the cpal thread pops
    underruns: Arc<AtomicU64>, // output callbacks that found the ring dry after it had data
}

fn q16(s: f32) -> i16 {
    (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

fn dev_name(d: &cpal::Device) -> String {
    d.name().unwrap_or_default()
}

fn devices(host: &cpal::Host, want_in: bool) -> io::Result<Vec<cpal::Device>> {
    if want_in {
        Ok(host.input_devices().map_err(|e| io::Error::other(e.to_string()))?.collect())
    } else {
        Ok(host.output_devices().map_err(|e| io::Error::other(e.to_string()))?.collect())
    }
}

/// Device by name (case-insensitive), falling back to the system default when empty.
fn find(host: &cpal::Host, name: &str, want_in: bool) -> io::Result<cpal::Device> {
    if let Some(d) = devices(host, want_in)?.into_iter().find(|d| dev_name(d).eq_ignore_ascii_case(name)) {
        return Ok(d);
    }
    if name.is_empty() || name.eq_ignore_ascii_case("default") {
        return if want_in { host.default_input_device() } else { host.default_output_device() }
            .ok_or_else(|| io::Error::other("no default audio device"));
    }
    Err(io::Error::other(format!("no {} device named {name:?}", if want_in { "capture" } else { "playback" })))
}

/// Prefer 48 kHz I16, then F32, then mono over stereo; channels stay 1 or 2.
fn select_config(
    cfgs: impl Iterator<Item = SupportedStreamConfigRange>,
    what: &str,
) -> io::Result<(cpal::StreamConfig, usize, SampleFormat)> {
    let rank = |f: SampleFormat| match f {
        SampleFormat::I16 => 0,
        SampleFormat::F32 => 1,
        _ => 2,
    };
    let best = cfgs
        .filter(|c| (1..=2).contains(&c.channels()) && c.min_sample_rate().0 <= RATE && RATE <= c.max_sample_rate().0)
        .min_by_key(|c| (rank(c.sample_format()), c.channels()));
    let c = best
        .filter(|c| rank(c.sample_format()) < 2)
        .ok_or_else(|| io::Error::other(format!("wasapi {what}: no {RATE} Hz mono or stereo I16/F32 stream format")))?;
    let sup = c
        .try_with_sample_rate(cpal::SampleRate(RATE))
        .ok_or_else(|| io::Error::other(format!("wasapi {what}: {RATE} Hz went out of range")))?;
    Ok((sup.config(), sup.channels() as usize, sup.sample_format()))
}

fn stream_err(what: &'static str) -> impl FnMut(StreamError) {
    move |e| say!("wasapi {what} stream error: {e}")
}

fn open_in(dev: &cpal::Device, cap: &Arc<Spsc>) -> io::Result<cpal::Stream> {
    let cfgs = dev.supported_input_configs().map_err(|e| io::Error::other(format!("wasapi capture formats: {e}")))?;
    let (cfg, ch, fmt) = select_config(cfgs, "capture")?;
    let stream = match fmt {
        SampleFormat::I16 => {
            let (cap, mut buf) = (cap.clone(), Vec::with_capacity(RING_SAMPLES));
            dev.build_input_stream(
                &cfg,
                move |data: &[i16], _: &InputCallbackInfo| {
                    buf.clear();
                    if ch == 1 {
                        buf.extend_from_slice(data);
                    } else {
                        for f in data.chunks_exact(2) {
                            buf.push(((i32::from(f[0]) + i32::from(f[1])) / 2) as i16);
                        }
                    }
                    let _ = cap.try_push(&buf); // overflow is dropped; never block the audio thread
                },
                stream_err("capture"),
                None,
            )
        }
        _ => {
            let (cap, mut buf) = (cap.clone(), Vec::with_capacity(RING_SAMPLES));
            dev.build_input_stream(
                &cfg,
                move |data: &[f32], _: &InputCallbackInfo| {
                    buf.clear();
                    if ch == 1 {
                        buf.extend(data.iter().map(|s| q16(*s)));
                    } else {
                        for f in data.chunks_exact(2) {
                            buf.push(q16((f[0] + f[1]) * 0.5));
                        }
                    }
                    let _ = cap.try_push(&buf);
                },
                stream_err("capture"),
                None,
            )
        }
    }
    .map_err(|e| io::Error::other(format!("wasapi capture stream: {e}")))?;
    stream.play().map_err(|e| io::Error::other(format!("wasapi capture start: {e}")))?;
    Ok(stream)
}

fn open_out(dev: &cpal::Device, play: &Arc<Spsc>, underruns: &Arc<AtomicU64>) -> io::Result<cpal::Stream> {
    let cfgs = dev.supported_output_configs().map_err(|e| io::Error::other(format!("wasapi playback formats: {e}")))?;
    let (cfg, ch, fmt) = select_config(cfgs, "playback")?;
    let stream = match fmt {
        SampleFormat::I16 => {
            let (play, underruns, mut buf) = (play.clone(), underruns.clone(), Vec::with_capacity(RING_SAMPLES));
            dev.build_output_stream(
                &cfg,
                move |data: &mut [i16], _: &OutputCallbackInfo| {
                    let frames = data.len() / ch;
                    buf.clear();
                    buf.resize(frames, 0);
                    let have = play.fill();
                    let n = play.try_pop(&mut buf); // anything missing stays silence
                    if have > 0 && n < frames {
                        underruns.fetch_add(1, Ordering::Relaxed); // the ring had data but ran dry
                    }
                    for (i, f) in data.chunks_exact_mut(ch).enumerate() {
                        f.fill(buf[i]); // mono to every channel
                    }
                },
                stream_err("playback"),
                None,
            )
        }
        _ => {
            let (play, underruns, mut buf) = (play.clone(), underruns.clone(), Vec::with_capacity(RING_SAMPLES));
            dev.build_output_stream(
                &cfg,
                move |data: &mut [f32], _: &OutputCallbackInfo| {
                    let frames = data.len() / ch;
                    buf.clear();
                    buf.resize(frames, 0);
                    let have = play.fill();
                    let n = play.try_pop(&mut buf);
                    if have > 0 && n < frames {
                        underruns.fetch_add(1, Ordering::Relaxed);
                    }
                    for (i, f) in data.chunks_exact_mut(ch).enumerate() {
                        f.fill(buf[i] as f32 / 32768.0);
                    }
                },
                stream_err("playback"),
                None,
            )
        }
    }
    .map_err(|e| io::Error::other(format!("wasapi playback stream: {e}")))?;
    stream.play().map_err(|e| io::Error::other(format!("wasapi playback start: {e}")))?;
    Ok(stream)
}

impl WasapiIo {
    pub fn open(capture: &str, playback: &str, radio: bool, tx: TxBuffer) -> io::Result<Self> {
        check(playback, radio)?;
        let host = cpal::default_host();
        // the play ring must be able to hold the whole rate-matcher TX buffer
        let (cap, play) = (Arc::new(Spsc::new(RING_SAMPLES)), Arc::new(Spsc::new(RING_SAMPLES.max(tx.capacity()))));
        let underruns = Arc::new(AtomicU64::new(0));
        let in_stream = open_in(&find(&host, capture, true)?, &cap)?;
        let out_stream = open_out(&find(&host, playback, false)?, &play, &underruns)?;
        Ok(Self { in_stream: Some(in_stream), out_stream: Some(out_stream), cap, play, underruns })
    }

    pub fn open_capture(capture: &str) -> io::Result<Self> {
        let host = cpal::default_host();
        let cap = Arc::new(Spsc::new(RING_SAMPLES));
        let in_stream = open_in(&find(&host, capture, true)?, &cap)?;
        Ok(Self {
            in_stream: Some(in_stream),
            out_stream: None,
            cap,
            play: Arc::new(Spsc::new(RING_SAMPLES)),
            underruns: Arc::new(AtomicU64::new(0)),
        })
    }
}

impl Audio for WasapiIo {
    fn read(&mut self, x: &mut [i16]) -> io::Result<()> {
        if self.in_stream.is_none() {
            return Err(io::Error::other("wasapi: stream opened without capture"));
        }
        let until = Instant::now() + IO_TIMEOUT;
        let mut off = 0;
        while off < x.len() {
            off += self.cap.try_pop(&mut x[off..]);
            if off < x.len() {
                if until.checked_duration_since(Instant::now()).is_none_or(|d| d.is_zero()) {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "wasapi capture: no audio"));
                }
                std::thread::yield_now();
            }
        }
        Ok(())
    }

    fn write(&mut self, x: &[i16]) -> io::Result<()> {
        if self.out_stream.is_none() {
            return Err(super::capture_only());
        }
        let until = Instant::now() + IO_TIMEOUT;
        let mut off = 0;
        while off < x.len() {
            off += self.play.try_push(&x[off..]);
            if off < x.len() {
                if until.checked_duration_since(Instant::now()).is_none_or(|d| d.is_zero()) {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "wasapi playback: sink not draining"));
                }
                std::thread::yield_now();
            }
        }
        Ok(())
    }

    // cpal gives no endpoint-delay query, so frames is what our ring holds; floor stays 0
    // and underruns are counted by the output callback itself.
    fn queued(&mut self) -> io::Result<Option<Queue>> {
        if self.out_stream.is_none() {
            return Err(super::capture_only());
        }
        Ok(Some(Queue { frames: self.play.fill(), floor: 0, underruns: self.underruns.load(Ordering::Relaxed) }))
    }

    fn backlog(&mut self) -> io::Result<Option<usize>> {
        Ok(Some(self.cap.fill()))
    }
}

pub fn list() -> io::Result<(DeviceList, DeviceList)> {
    let host = cpal::default_host();
    let names = |v: Vec<cpal::Device>| v.into_iter().map(|d| (dev_name(&d), String::new())).collect();
    Ok((names(devices(&host, true)?), names(devices(&host, false)?)))
}

/// The Windows analogue of pulse's sink guard: with --radio the playback device must be
/// named explicitly and must exist; without it xara plays to whatever the user chose.
pub fn check(playback: &str, radio: bool) -> io::Result<()> {
    if radio && (playback.is_empty() || playback.eq_ignore_ascii_case("default")) {
        return Err(io::Error::other("--radio needs the playback device named explicitly, not a default"));
    }
    if radio {
        find(&cpal::default_host(), playback, false).map(drop)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_preserves_order_and_never_blocks() {
        let r = Spsc::new(8);
        assert_eq!(r.try_pop(&mut [0i16; 8]), 0); // empty: no block, no data
        assert_eq!(r.try_push(&[1, 2, 3]), 3);
        assert_eq!(r.try_push(&[4, 5, 6, 7, 8, 9]), 5); // cap 8: the last sample does not fit
        let mut out = [0i16; 8];
        assert_eq!(r.try_pop(&mut out), 8);
        assert_eq!(&out, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(r.try_pop(&mut out), 0);
        assert_eq!(r.try_push(&[1, 2, 3]), 3); // still writable after draining
    }

    #[test]
    fn ring_wraps_without_losing_samples() {
        let r = Spsc::new(4); // cap 4, fill never exceeds 3, so indices wrap constantly
        let (mut next_in, mut next_out) = (0i16, 0i16);
        let mut out = [0i16; 3];
        while next_out < 50 {
            let want: Vec<i16> = (next_in..(next_in + 3).min(50)).collect();
            next_in += r.try_push(&want) as i16;
            let m = r.try_pop(&mut out);
            let expect: Vec<i16> = (next_out..next_out + m as i16).collect();
            assert_eq!(&out[..m], &expect[..], "wrap corrupted the stream");
            next_out += m as i16;
        }
    }

    #[test]
    fn ring_flows_between_threads() {
        let r = Arc::new(Spsc::new(64));
        let (prod, cons) = (r.clone(), r.clone());
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wdone = done.clone();
        let producer = std::thread::spawn(move || {
            let mut sent = 0;
            for i in 0..10_000u32 {
                sent += prod.try_push(&[i as i16]);
                std::thread::yield_now();
            }
            wdone.store(true, Ordering::Release); // after this the fill level can only shrink
            sent
        });
        let mut got: Vec<i16> = Vec::new();
        let mut v = [0i16; 7];
        loop {
            let n = cons.try_pop(&mut v);
            got.extend_from_slice(&v[..n]);
            if n == 0 {
                if done.load(Ordering::Acquire) && r.head.load(Ordering::Acquire) == r.tail.load(Ordering::Acquire) {
                    break;
                }
                std::thread::yield_now();
            }
        }
        let sent = producer.join().unwrap();
        assert_eq!(got.len(), sent, "consumer lost or invented samples");
        assert!(got.iter().enumerate().all(|(i, &x)| x == i as i16), "consumer reordered the stream");
    }

    #[test]
    fn config_selection_prefers_i16_mono_at_rate() {
        let mk = |ch: u16, lo: u32, hi: u32, f: SampleFormat| {
            SupportedStreamConfigRange::new(
                ch,
                cpal::SampleRate(lo),
                cpal::SampleRate(hi),
                cpal::SupportedBufferSize::Unknown,
                f,
            )
        };
        let pool = vec![
            mk(2, 8_000, 192_000, SampleFormat::F32),
            mk(2, 8_000, 192_000, SampleFormat::I16),
            mk(1, 8_000, 192_000, SampleFormat::F32),
            mk(1, 8_000, 192_000, SampleFormat::I16),
        ];
        let (cfg, ch, fmt) = select_config(pool.into_iter(), "test").unwrap();
        assert_eq!((ch, fmt, cfg.sample_rate.0), (1, SampleFormat::I16, RATE));
        // stereo F32 only: accepted as stereo
        let stereo = vec![mk(2, 44_100, 48_000, SampleFormat::F32)];
        assert_eq!(select_config(stereo.into_iter(), "test").unwrap().1, 2);
        // nothing at our rate: refused rather than silently mis-sampled
        let bad = vec![mk(1, 8_000, 44_100, SampleFormat::I16)];
        assert!(select_config(bad.into_iter(), "test").is_err_and(|e| e.to_string().contains("no 48000 Hz")));
        // only 6-channel formats: refused
        let wide = vec![mk(6, 8_000, 192_000, SampleFormat::I16)];
        assert!(select_config(wide.into_iter(), "test").is_err());
    }
}
