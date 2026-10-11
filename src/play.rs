//! `rec-rs play FILE`: show a file in an AVPlayer window (QuickTime's
//! engine, with exact audio-output latency compensation). A reference
//! player for end-to-end sync tests: record its window with system audio
//! and run scripts/synccheck.py on the result.

use std::path::Path;

use anyhow::{Context, Result};
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::*;
use objc2_av_foundation::{AVPlayer, AVPlayerLayer};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSDate, NSRunLoop, NSURL};

use crate::util::ns;

pub fn play(path: &Path, secs: f64, size: &str) -> Result<()> {
    let (w, h) = size.split_once('x').context("--size must be WxH")?;
    let (w, h): (f64, f64) = (w.parse()?, h.parse()?);
    let mtm = MainThreadMarker::new().context("must run on the main thread")?;
    let abs = std::path::absolute(path)?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.finishLaunching();
    let rect = CGRect::new(CGPoint::new(200.0, 200.0), CGSize::new(w, h));
    let win = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { win.setReleasedWhenClosed(false) };
    win.setTitle(&ns("rec-play"));
    let view = win.contentView().context("content view")?;
    view.setWantsLayer(true);
    let url = NSURL::fileURLWithPath(&ns(abs.to_str().context("path")?));
    let player = unsafe { AVPlayer::playerWithURL(&url, mtm) };
    let layer = unsafe { AVPlayerLayer::playerLayerWithPlayer(Some(&player)) };
    layer.setFrame(view.bounds());
    view.layer().context("layer")?.addSublayer(&layer);
    win.makeKeyAndOrderFront(None);
    app.activate();
    let run = |s: f64| NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(s));
    run(1.5); // let the window appear (and a recorder attach) first
    unsafe { player.play() };
    run(secs);
    Ok(())
}
