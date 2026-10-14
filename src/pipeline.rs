//! Owns the capture threads. Stopping joins them, so a restart with new
//! settings never has two encoders fighting over the GPU.

use crate::prelude::*;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::rt::Mutex;
use crate::rt::thread::{self, JoinHandle};

use crate::audio;
use crate::capture::{self, VideoConfig};
use crate::config::Config;
use crate::ring::Ring;

pub struct Pipeline {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle>,
    pub status: Arc<Mutex<String>>,
}

impl Pipeline {
    pub fn start(cfg: &Config, ring: Arc<Mutex<Ring>>) -> Self {
        let audio = cfg.audio_config();
        let audio_tracks = audio.track_names();
        {
            let mut ring = ring.lock();
            ring.set_keep(cfg.replay_seconds);
            ring.set_audio_tracks(audio_tracks.clone());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new("Starting...".to_string()));
        let mut threads = Vec::new();

        let video = VideoConfig {
            monitor: cfg.monitor as usize,
            fps: cfg.fps,
            bitrate: cfg.bitrate_mbps * 1_000_000,
            height: cfg.resolution,
            cursor: cfg.capture_cursor,
            keep_stretched: cfg.keep_stretched,
        };
        {
            let (ring, stop, status) = (ring.clone(), stop.clone(), status.clone());
            threads.push(thread::spawn("video", move || capture::run(video, ring, stop, status)));
        }

        if !audio_tracks.is_empty() {
            let (ring, stop) = (ring.clone(), stop.clone());
            threads.push(thread::spawn("audio", move || audio::run(audio, ring, stop)));
        }

        Self { stop, threads, status }
    }

    pub fn status(&self) -> String {
        self.status.lock().clone()
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let name = t.name();
            t.join();
            crate::log!("pipeline: {name} thread stopped");
        }
    }
}
