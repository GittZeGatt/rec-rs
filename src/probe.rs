//! Verify a recording with AVFoundation: playability, tracks, full decode.

#![allow(deprecated)]

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use objc2::rc::Retained;
use objc2_av_foundation::*;
use objc2_core_media::{CMFormatDescription, CMSampleBuffer};
use objc2_foundation::NSURL;

use crate::util::*;

fn fourcc_str(v: u32) -> String {
    v.to_be_bytes().iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' }).collect()
}

pub fn probe(file: &Path) -> Result<()> {
    let abs = std::path::absolute(file)?;
    if !abs.exists() {
        bail!("{} does not exist", abs.display());
    }
    unsafe {
        let url = NSURL::fileURLWithPath(&ns(abs.to_str().unwrap()));
        let asset = AVURLAsset::URLAssetWithURL_options(&url, None);
        let playable = asset.isPlayable();
        println!("file      : {}", abs.display());
        println!("playable  : {playable}");
        println!("duration  : {:.3}s", secs(asset.duration()));
        let tracks = asset.tracks();
        let mut ok = playable;
        for (i, t) in tracks.iter().enumerate() {
            let media = t.mediaType().to_string();
            let fds = t.formatDescriptions();
            let codec = fds
                .firstObject()
                .map(|fd| {
                    let fd: &CMFormatDescription = &*(Retained::as_ptr(&fd) as *const CMFormatDescription);
                    fourcc_str(fd.media_sub_type())
                })
                .unwrap_or_default();
            let tr = t.timeRange();
            let mut line = format!(
                "track {i}  : {media} {codec} enabled={} start={:.3}s dur={:.3}s {:.1} kb/s",
                t.isEnabled(),
                secs(tr.start),
                secs(tr.duration),
                t.estimatedDataRate() / 1000.0
            );
            if media == "vide" {
                let sz = t.naturalSize();
                line += &format!(" {}x{} @ {:.3} fps", sz.width, sz.height, t.nominalFrameRate());
            }
            println!("{line}");
            // Decode every sample: the strongest check that players/editors can read it.
            let reader = AVAssetReader::assetReaderWithAsset_error(&asset)
                .map_err(|e| anyhow!("reader: {}", e.localizedDescription()))?;
            let settings = if media == "vide" {
                Some(dict(vec![(
                    cf_ns(objc2_core_video::kCVPixelBufferPixelFormatTypeKey),
                    num_i(PIX_420V as i64),
                )]))
            } else {
                Some(dict(vec![(&*ns("AVFormatIDKey"), num_i(fourcc(b"lpcm") as i64))]))
            };
            let out = AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(&t, settings.as_deref());
            out.setAlwaysCopiesSampleData(false);
            reader.addOutput(&out);
            if !reader.startReading() {
                println!("          decode: FAILED to start");
                ok = false;
                continue;
            }
            let (mut n, mut samples) = (0u64, 0i64);
            let (mut first, mut last) = (f64::NAN, f64::NAN);
            while let Some(sb) = out.copyNextSampleBuffer() {
                let sb: Retained<CMSampleBuffer> = sb;
                n += 1;
                samples += sb.num_samples() as i64;
                let p = secs(sb.presentation_time_stamp());
                if first.is_nan() {
                    first = p;
                }
                last = p;
            }
            let status = reader.status();
            let good = status == AVAssetReaderStatus::Completed;
            ok &= good;
            println!(
                "          decode: {} ({} buffers, {} samples, pts {:.3}..{:.3}s)",
                if good { "ok" } else { "FAILED" },
                n,
                samples,
                first,
                last
            );
        }
        if !ok {
            bail!("probe found problems");
        }
    }
    Ok(())
}
