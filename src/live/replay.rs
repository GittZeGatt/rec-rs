//! Replay buffer: the last N seconds of encoded video and audio, kept in
//! memory from a keyframe on, saved to MP4 on demand without stopping.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use objc2::AnyThread;
use objc2_av_foundation::*;
use objc2_core_foundation::CFRetained;
use objc2_core_media::*;
use objc2_foundation::NSURL;

use super::audio::AudioPacket;
use super::video::VideoPacket;
use crate::util::*;

/// What the ring needs to know about a packet.
pub trait Timed {
    fn pts(&self) -> f64;
    fn key(&self) -> bool {
        true
    }
    fn size(&self) -> usize;
}

impl Timed for VideoPacket {
    fn pts(&self) -> f64 {
        self.pts
    }
    fn key(&self) -> bool {
        self.key
    }
    fn size(&self) -> usize {
        self.data.len()
    }
}

impl Timed for AudioPacket {
    fn pts(&self) -> f64 {
        self.pts
    }
    fn size(&self) -> usize {
        self.data.len()
    }
}

/// Ring of the last `secs` seconds, always starting at a video keyframe.
pub struct Ring<V, A> {
    pub secs: f64,
    video: VecDeque<V>,
    audio: VecDeque<A>,
    bytes: usize,
}

impl<V: Timed + Clone, A: Timed + Clone> Ring<V, A> {
    pub fn new(secs: f64) -> Self {
        Ring { secs, video: VecDeque::new(), audio: VecDeque::new(), bytes: 0 }
    }

    pub fn push_video(&mut self, v: V) {
        // Wait for a keyframe to start.
        if self.video.is_empty() && !v.key() {
            return;
        }
        self.bytes += v.size();
        self.video.push_back(v);
        self.trim();
    }

    pub fn push_audio(&mut self, a: A) {
        // Keep a little audio before the first keyframe: the saved clip
        // starts it early and trims the encoder's priming (see write_mp4).
        if self.video.front().is_none_or(|v| a.pts() < v.pts() - AUDIO_LEAD) {
            return;
        }
        self.bytes += a.size();
        self.audio.push_back(a);
    }

    /// Drop whole GOPs that are older than the window (keeping the
    /// keyframe that starts it, so the saved clip always decodes).
    fn trim(&mut self) {
        let Some(latest) = self.video.back().map(|v| v.pts()) else { return };
        let horizon = latest - self.secs;
        // Newest keyframe at or before the horizon.
        let cut = self.video.iter().enumerate().filter(|(_, v)| v.key() && v.pts() <= horizon).map(|(i, _)| i).last();
        if let Some(cut) = cut.filter(|&c| c > 0) {
            for v in self.video.drain(..cut) {
                self.bytes -= v.size();
            }
            let start = self.video[0].pts() - AUDIO_LEAD;
            while self.audio.front().is_some_and(|a| a.pts() < start) {
                let a = self.audio.pop_front().unwrap();
                self.bytes -= a.size();
            }
        }
    }

    /// Bytes held (encoded payloads).
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Seconds of video held.
    pub fn held(&self) -> f64 {
        match (self.video.front(), self.video.back()) {
            (Some(a), Some(b)) => b.pts() - a.pts(),
            _ => 0.0,
        }
    }

    /// The last `secs` (or everything held), starting at a keyframe.
    pub fn snapshot(&self) -> (Vec<V>, Vec<A>) {
        let latest = self.video.back().map_or(0.0, |v| v.pts());
        let horizon = latest - self.secs;
        let start = self
            .video
            .iter()
            .enumerate()
            .filter(|(_, v)| v.key() && v.pts() <= horizon.max(self.video[0].pts()))
            .map(|(i, _)| i)
            .last()
            .unwrap_or(0);
        let v: Vec<V> = self.video.iter().skip(start).cloned().collect();
        let t0 = v.first().map_or(0.0, |v| v.pts());
        let a = self.audio.iter().filter(|a| a.pts() >= t0 - AUDIO_LEAD).cloned().collect();
        (v, a)
    }

    pub fn clear(&mut self) {
        self.video.clear();
        self.audio.clear();
        self.bytes = 0;
    }
}

pub type Replay = Ring<VideoPacket, AudioPacket>;

/// Audio kept before the clip start (more than the AAC priming delay).
const AUDIO_LEAD: f64 = 0.2;
/// AAC encoder priming (2112 frames): the decoder's output only matches the
/// input after this much, so a clip's audio must start this early.
const PRIMING: f64 = 2112.0 / 48000.0;

/// Write a snapshot to an MP4 (video passed through, audio re-encoded;
/// blocking).
pub fn write_mp4(path: &Path, video: &[VideoPacket], audio: &[AudioPacket], aac_cookie: &[u8]) -> Result<PathBuf> {
    // AVFoundation reports misuse with Objective-C exceptions.
    match objc2::exception::catch(std::panic::AssertUnwindSafe(|| write_mp4_inner(path, video, audio, aac_cookie))) {
        Ok(r) => r,
        Err(e) => bail!("writing the replay: {}", e.map(|e| format!("{e:?}")).unwrap_or_default()),
    }
}

fn write_mp4_inner(path: &Path, video: &[VideoPacket], audio: &[AudioPacket], aac_cookie: &[u8]) -> Result<PathBuf> {
    let Some(first) = video.first() else { bail!("the replay buffer is empty") };
    let t0 = first.pts;
    let abs = std::path::absolute(path)?;
    unsafe {
        let url = NSURL::fileURLWithPath(&ns(abs.to_str().context("path")?));
        let writer = AVAssetWriter::initWithURL_fileType_error(AVAssetWriter::alloc(), &url, AVFileTypeMPEG4.unwrap())
            .map_err(|e| anyhow!("AVAssetWriter: {}", e.localizedDescription()))?;
        let vin = AVAssetWriterInput::initWithMediaType_outputSettings_sourceFormatHint(
            AVAssetWriterInput::alloc(),
            AVMediaTypeVideo.unwrap(),
            None,
            Some(&first.format.0),
        );
        vin.setMediaTimeScale(60000);
        // Real-time inputs don't make one track wait for the other to fill
        // the interleaving window; the data is all in memory already.
        vin.setExpectsMediaDataInRealTime(true);
        writer.addInput(&vin);
        // Audio is decoded and re-encoded by AVAssetWriter (as recordings
        // are): its AAC track carries the priming edit list every player
        // reads the same way. Passing the buffered AAC through leaves the
        // mid-stream start to each player's priming guess.
        let ain = (!audio.is_empty()).then(|| {
            let a = AVAssetWriterInput::initWithMediaType_outputSettings(
                AVAssetWriterInput::alloc(),
                AVMediaTypeAudio.unwrap(),
                Some(&crate::writer::audio_settings(2, 192_000)),
            );
            a.setExpectsMediaDataInRealTime(true);
            writer.addInput(&a);
            a
        });
        if !writer.startWriting() {
            bail!("startWriting: {:?}", writer.error().map(|e| e.localizedDescription().to_string()));
        }
        writer.startSessionAtSourceTime(CMTime::new(0, 60000));
        let wait = |input: &AVAssetWriterInput, what: &str, t: f64| -> Result<()> {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !input.isReadyForMoreMediaData() {
                if std::time::Instant::now() > deadline {
                    bail!("{what} input stalled at {t:.3}s (status {:?}, {:?})", writer.status(), writer.error().map(|e| e.localizedDescription().to_string()));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(())
        };
        let shift = (t0 * 60000.0).round() as i64;
        let last_v = video.last().map_or(0.0, |v| v.pts - t0);
        // AVAssetWriter interleaves tracks: appending one track far ahead of
        // the other stalls it, so append both in timestamp order.
        // Start the audio at least one priming delay before the clip and
        // mark the lead-in as trimmed: AVAssetWriter writes an edit list,
        // which QuickTime and ffmpeg both honor (an untrimmed mid-stream
        // AAC start is placed differently by each player).
        let first = audio.iter().rposition(|a| a.pts <= t0 - PRIMING).unwrap_or(0);
        let audio: Vec<&AudioPacket> = audio[first..].iter().filter(|a| a.pts - t0 <= last_v + 0.05).collect();
        // Decode from the lead-in (decoder warm-up) and keep what falls in
        // the clip, as PCM buffers placed by each packet's timestamp.
        let fmt = crate::mixer::stereo_format();
        let mut units: Vec<(f64, CFRetained<CMSampleBuffer>)> = vec![];
        if !audio.is_empty() {
            let mut dec = super::audio::AacDecoder::new(aac_cookie)?;
            for a in &audio {
                let pcm = dec.decode(a);
                let skip = ((t0 - a.pts) * 48000.0).round().max(0.0) as usize;
                if a.pts < t0 - PRIMING || skip >= pcm.len() {
                    continue; // warm-up
                }
                let start = a.pts - t0 + skip as f64 / 48000.0;
                let data: Vec<f32> = pcm[skip..].iter().flatten().copied().collect();
                let pts = CMTime::new((start * 48000.0).round() as i64, 48000);
                if let Some(b) = crate::mixer::pcm_buffer(&fmt, data, pts) {
                    units.push((start, b));
                }
            }
        }
        let (mut vi, mut ui) = (0, 0);
        while vi < video.len() || ui < units.len() {
            let take_video = ui >= units.len() || (vi < video.len() && video[vi].pts - t0 <= units[ui].0);
            if take_video {
                let v = &video[vi];
                vi += 1;
                if let Some(s) = crate::writer::shifted(&v.sample.0, shift) {
                    wait(&vin, "video", v.pts - t0)?;
                    if !vin.appendSampleBuffer(&s) {
                        bail!("replay video append failed: {:?}", writer.error().map(|e| e.localizedDescription().to_string()));
                    }
                }
            } else {
                let (start, b) = &units[ui];
                ui += 1;
                if let Some(ain) = &ain {
                    wait(ain, "audio", *start)?;
                    if !ain.appendSampleBuffer(b) {
                        bail!("replay audio append failed: {:?}", writer.error().map(|e| e.localizedDescription().to_string()));
                    }
                }
            }
        }
        if let Some(ain) = &ain {
            ain.markAsFinished();
        }
        vin.markAsFinished();
        writer.endSessionAtSourceTime(CMTime::new(((last_v + 1.0 / 60.0) * 60000.0).round() as i64, 60000));
        let (tx, rx) = mpsc::channel();
        writer.finishWritingWithCompletionHandler(&RcBlock::new(move || {
            let _ = tx.send(());
        }));
        rx.recv_timeout(Duration::from_secs(60)).context("timed out writing the replay")?;
        if writer.status() != AVAssetWriterStatus::Completed {
            bail!("writing the replay failed: {:?}", writer.error().map(|e| e.localizedDescription().to_string()));
        }
    }
    Ok(abs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct P(f64, bool, usize);
    impl Timed for P {
        fn pts(&self) -> f64 {
            self.0
        }
        fn key(&self) -> bool {
            self.1
        }
        fn size(&self) -> usize {
            self.2
        }
    }

    /// 30 fps video with a keyframe every second, audio every 21 ms.
    fn fill(r: &mut Ring<P, P>, secs: f64) {
        let mut a = 0.0;
        for i in 0..(secs * 30.0) as usize {
            let t = i as f64 / 30.0;
            r.push_video(P(t, i % 30 == 0, 1000));
            while a <= t {
                r.push_audio(P(a, true, 100));
                a += 1024.0 / 48000.0;
            }
        }
    }

    #[test]
    fn keeps_the_window_from_a_keyframe() {
        let mut r = Ring::new(10.0);
        fill(&mut r, 60.0);
        let (v, a) = r.snapshot();
        assert!(v[0].1, "starts at a keyframe");
        let span = v.last().unwrap().0 - v[0].0;
        assert!((10.0..11.0).contains(&span), "10-11 s of video, got {span}");
        // Audio starts up to AUDIO_LEAD early (decoder warm-up for the clip).
        assert!(a[0].0 < v[0].0 && v[0].0 - a[0].0 <= AUDIO_LEAD + 0.03, "audio lead-in: {} vs {}", a[0].0, v[0].0);
        // Memory stays bounded (about 11 s held, not 60).
        assert!(r.held() < 11.1);
        assert!(r.bytes() < 12 * (30 * 1000 + 47 * 100));
    }

    #[test]
    fn waits_for_a_keyframe_and_handles_short_buffers() {
        let mut r: Ring<P, P> = Ring::new(30.0);
        r.push_video(P(0.0, false, 10));
        r.push_audio(P(0.0, true, 10));
        assert_eq!(r.bytes(), 0);
        fill(&mut r, 3.0);
        let (v, _) = r.snapshot();
        assert_eq!(v.len(), 90, "less than the window: everything");
        r.clear();
        assert_eq!((r.bytes(), r.held()), (0, 0.0));
    }
}
