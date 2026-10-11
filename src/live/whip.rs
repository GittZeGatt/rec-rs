//! WHIP (WebRTC-HTTP ingestion) output.

use anyhow::{Result, bail};

use super::{Formats, Output, OutputStatus, Packet};

pub struct WhipOutput;

impl WhipOutput {
    pub fn start(_url: &str, _key: &str) -> Result<WhipOutput> {
        bail!("WHIP output not implemented yet")
    }
}

impl Output for WhipOutput {
    fn packet(&mut self, _p: &Packet, _f: &Formats) {}
    fn status(&self) -> OutputStatus {
        OutputStatus::default()
    }
    fn stop(&mut self) {}
}
