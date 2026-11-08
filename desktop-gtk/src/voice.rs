//! Voice messages: the microphone through PipeWire (GStreamer, raw PCM into
//! an appsink — no GStreamer encoder plugins needed), encoded here to Ogg
//! Opus with libopus, plus the 0..1 waveform `SendVoiceMessage` wants.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

const RATE: u32 = 48_000;
/// 20 ms Opus frames
const FRAME: usize = 960;
const PRE_SKIP: u16 = 312;

pub struct Recorder {
    pipeline: gst::Pipeline,
    samples: Arc<Mutex<Vec<i16>>>,
    started: Instant,
}

pub struct Recording {
    pub ogg: Vec<u8>,
    pub duration: Duration,
    pub waveform: Vec<f32>,
}

impl Recorder {
    pub fn start() -> anyhow::Result<Recorder> {
        gst::init()?;
        let pipeline = gst::parse::launch(&format!(
            "pipewiresrc ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate={RATE},channels=1 ! appsink name=sink sync=false"
        ))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
        let sink = pipeline
            .by_name("sink")
            .and_then(|s| s.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| anyhow::anyhow!("no appsink"))?;
        let samples: Arc<Mutex<Vec<i16>>> = Arc::default();
        let store = samples.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let pcm = map.as_slice().chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]));
                    store.lock().unwrap().extend(pcm);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.set_state(gst::State::Playing)?;
        Ok(Recorder { pipeline, samples, started: Instant::now() })
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn cancel(self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    pub fn finish(self) -> anyhow::Result<Recording> {
        let _ = self.pipeline.set_state(gst::State::Null);
        let samples = std::mem::take(&mut *self.samples.lock().unwrap());
        if samples.len() < RATE as usize / 2 {
            anyhow::bail!("bản ghi quá ngắn");
        }
        let duration = Duration::from_millis(samples.len() as u64 * 1000 / RATE as u64);
        Ok(Recording { waveform: waveform(&samples, 100), ogg: encode_ogg_opus(&samples)?, duration })
    }
}

/// `points` RMS levels over the recording, scaled so the loudest is 1.
fn waveform(samples: &[i16], points: usize) -> Vec<f32> {
    let chunk = samples.len().div_ceil(points).max(1);
    let rms: Vec<f32> = samples
        .chunks(chunk)
        .map(|c| ((c.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / c.len() as f64).sqrt() / i16::MAX as f64) as f32)
        .collect();
    let peak = rms.iter().cloned().fold(0.0f32, f32::max).max(1e-4);
    rms.into_iter().map(|v| (v / peak).clamp(0.0, 1.0)).collect()
}

/// Mono 48 kHz PCM -> an Ogg Opus file (RFC 7845).
fn encode_ogg_opus(samples: &[i16]) -> anyhow::Result<Vec<u8>> {
    use ogg::writing::{PacketWriteEndInfo, PacketWriter};
    let mut enc = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)?;
    enc.set_bitrate(opus::Bitrate::Bits(32_000))?;
    let serial = 0x43_68_6f_53; // "ChoS"
    let mut out = Vec::new();
    {
        let mut w = PacketWriter::new(&mut out);
        let mut head = b"OpusHead".to_vec();
        head.push(1); // version
        head.push(1); // channels
        head.extend_from_slice(&PRE_SKIP.to_le_bytes());
        head.extend_from_slice(&RATE.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // channel mapping family
        w.write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)?;
        let vendor = b"ChoSua";
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes());
        w.write_packet(tags, serial, PacketWriteEndInfo::EndPage, 0)?;

        let frames: Vec<&[i16]> = samples.chunks(FRAME).collect();
        let mut granule = PRE_SKIP as u64;
        let mut buf = [0u8; 4000];
        for (i, frame) in frames.iter().enumerate() {
            let mut pcm = frame.to_vec();
            pcm.resize(FRAME, 0);
            let n = enc.encode(&pcm, &mut buf)?;
            granule += frame.len() as u64;
            let last = i + 1 == frames.len();
            let end = if last {
                PacketWriteEndInfo::EndStream
            } else if (i + 1) % 50 == 0 {
                PacketWriteEndInfo::EndPage // a page a second
            } else {
                PacketWriteEndInfo::NormalPacket
            };
            w.write_packet(buf[..n].to_vec(), serial, end, granule)?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn encodes_a_second_of_tone() {
        let samples: Vec<i16> = (0..48_000).map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16).collect();
        let ogg = super::encode_ogg_opus(&samples).unwrap();
        assert!(ogg.starts_with(b"OggS"));
        assert!(ogg.windows(8).any(|w| w == b"OpusHead"));
        let wf = super::waveform(&samples, 100);
        assert_eq!(wf.len(), 100);
        assert!(wf.iter().all(|v| (0.0..=1.0).contains(v)));
        if let Some(path) = std::env::var_os("CHOSUA_OGG_OUT") {
            std::fs::write(path, &ogg).unwrap();
        }
    }
}
