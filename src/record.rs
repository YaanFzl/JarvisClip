//! Start/stop recording. While a recording runs, the replay buffer queues every
//! new packet for it, and this thread streams them into an MP4 a few times a
//! second. Nothing piles up in memory, so recordings can run as long as the
//! disk allows.

use crate::prelude::*;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::rt::Mutex;
use crate::rt::thread::{self, JoinHandle};
use crate::rt::{fs, path};

use crate::clock::{self, SECOND};
use crate::log;
use crate::mux::Mp4Writer;
use crate::ring::Ring;

pub struct Saved {
    pub path: String,
    pub size: u64,
    pub seconds: u32,
    /// The display mode or capture settings changed, which ended the recording
    /// early.
    pub cut_short: bool,
}

pub type Outcome = core::result::Result<Saved, String>;

pub struct Recording {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle>,
    /// When it started, on the shared clock.
    pub started: i64,
}

impl Recording {
    /// Start recording into `path`. `done` is called from the recording
    /// thread once the file is finished (or failed).
    pub fn start(ring: Arc<Mutex<Ring>>, path: String, done: impl FnOnce(Outcome) + Send + 'static) -> Self {
        ring.lock().start_recording();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            thread::spawn("record", move || {
                let outcome = write(&ring, &path, &stop);
                ring.lock().stop_recording();
                match &outcome {
                    Ok(s) => log!("recorded {} ({} bytes, {} s)", s.path, s.size, s.seconds),
                    Err(e) => {
                        log!("recording failed: {e}");
                        fs::remove(&path);
                    }
                }
                done(outcome);
            })
        };
        log!("recording started");
        Self { stop, thread: Some(thread), started: clock::now() }
    }

    /// Ask the recording to finish. The file is done when `done` is called.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn seconds(&self) -> u32 {
        ((clock::now() - self.started) / SECOND) as u32
    }
}

impl Drop for Recording {
    /// Finishes the file before returning.
    fn drop(&mut self) {
        self.stop();
        if let Some(t) = self.thread.take() {
            t.join();
        }
    }
}

fn write(ring: &Mutex<Ring>, file: &str, stop: &AtomicBool) -> Outcome {
    let names = ring.lock().audio_names();
    let mut w = Mp4Writer::create(file, names).map_err(|e| e.to_string())?;
    let mut stop_at = None;
    let mut cut_short = false;
    loop {
        if stop_at.is_none() && stop.load(Ordering::Relaxed) {
            stop_at = Some(clock::now());
            // Audio is mixed a little behind real time; let it catch up.
            thread::sleep_ms(300);
        }
        let batch = ring.lock().take_recorded();
        // After an early end the ring may already hold the next format.
        if !batch.ended {
            if let Some(f) = batch.format {
                w.set_format(f);
            }
        }
        let wanted = |pts: i64| stop_at.is_none_or(|t| pts <= t);
        for p in batch.video.iter().filter(|p| wanted(p.pts)) {
            w.video(p).map_err(|e| e.to_string())?;
        }
        for (track, p) in batch.audio.iter().filter(|(_, p)| wanted(p.pts)) {
            w.audio(*track, p).map_err(|e| e.to_string())?;
        }
        if stop_at.is_some() {
            break;
        }
        if batch.ended {
            cut_short = true;
            break;
        }
        for _ in 0..5 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep_ms(50);
        }
    }
    let seconds = w.seconds();
    let size = w.finish().map_err(|e| e.to_string())?;
    Ok(Saved { path: file.to_string(), size, seconds, cut_short })
}

/// A file name in `folder` that isn't taken yet: "stem.mp4", "stem (2).mp4"...
pub fn unused_path(folder: &str, stem: &str) -> String {
    (1..)
        .map(|n| path::join(folder, &if n == 1 { format!("{stem}.mp4") } else { format!("{stem} ({n}).mp4") }))
        .find(|p| !fs::exists(p))
        .unwrap()
}
