//! In-memory replay buffer of *encoded* packets. Saving a clip never re-encodes,
//! it just copies packets from the last keyframe before the requested start.
//! While a recording is running, new packets are also queued up for it.

use crate::prelude::*;

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use crate::clock::SECOND;

pub struct Packet {
    /// Presentation time on the shared clock, 100ns units.
    pub pts: i64,
    pub key: bool,
    pub data: Vec<u8>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Annex B SPS/PPS from the encoder's output type, if it reported one.
    pub seq_header: Vec<u8>,
}

pub struct Clip {
    pub format: VideoFormat,
    pub video: Vec<Arc<Packet>>,
    /// One list per audio track, in track order.
    pub audio: Vec<Vec<Arc<Packet>>>,
    pub audio_names: Vec<String>,
}

/// Packets an ongoing recording hasn't written yet.
#[derive(Default)]
pub struct Recorded {
    pub format: Option<VideoFormat>,
    pub video: Vec<Arc<Packet>>,
    /// (track, packet)
    pub audio: Vec<(usize, Arc<Packet>)>,
    /// The video format changed or the buffer was reset, so nothing more can
    /// go into the same file.
    pub ended: bool,
}

#[derive(Default)]
pub struct Ring {
    video: VecDeque<Arc<Packet>>,
    audio: Vec<VecDeque<Arc<Packet>>>,
    audio_names: Vec<String>,
    format: Option<VideoFormat>,
    /// How much history to keep, 100ns units.
    keep: i64,
    bytes: usize,
    recording: Option<Recorded>,
}

impl Ring {
    pub fn new(seconds: u32) -> Self {
        // A couple of seconds of slack so we can always start on a keyframe.
        Self { keep: (seconds as i64 + 3) * SECOND, ..Default::default() }
    }

    pub fn set_keep(&mut self, seconds: u32) {
        self.keep = (seconds as i64 + 3) * SECOND;
        self.trim();
    }

    /// Called when a capture session (re)starts. Packets from an encoder with a
    /// different format can't share a file, so the buffer is dropped in that case.
    pub fn set_format(&mut self, format: VideoFormat) {
        if self.format.as_ref() != Some(&format) {
            self.clear();
        }
        self.format = Some(format);
    }

    /// Name each audio track. Called when the pipeline starts.
    pub fn set_audio_tracks(&mut self, names: Vec<String>) {
        if names != self.audio_names {
            self.clear();
            self.audio = names.iter().map(|_| VecDeque::new()).collect();
            self.audio_names = names;
        }
    }

    pub fn audio_names(&self) -> Vec<String> {
        self.audio_names.clone()
    }

    pub fn update_seq_header(&mut self, seq: Vec<u8>) {
        if let Some(f) = self.format.as_mut() {
            f.seq_header = seq;
        }
    }

    pub fn push_video(&mut self, p: Packet) {
        self.bytes += p.data.len();
        let p = Arc::new(p);
        if let Some(r) = self.recording.as_mut().filter(|r| !r.ended) {
            r.video.push(p.clone());
        }
        self.video.push_back(p);
        self.trim();
    }

    pub fn push_audio(&mut self, track: usize, p: Packet) {
        let Some(queue) = self.audio.get_mut(track) else { return };
        self.bytes += p.data.len();
        let p = Arc::new(p);
        if let Some(r) = self.recording.as_mut().filter(|r| !r.ended) {
            r.audio.push((track, p.clone()));
        }
        queue.push_back(p);
        self.trim();
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// How many seconds of video are buffered right now.
    pub fn seconds(&self) -> u32 {
        match (self.video.front(), self.video.back()) {
            (Some(first), Some(last)) => ((last.pts - first.pts) / SECOND) as u32,
            _ => 0,
        }
    }

    pub fn clear(&mut self) {
        self.video.clear();
        for a in &mut self.audio {
            a.clear();
        }
        self.bytes = 0;
        if let Some(r) = self.recording.as_mut() {
            r.ended = true;
        }
    }

    fn trim(&mut self) {
        let newest = self
            .video
            .back()
            .map(|p| p.pts)
            .into_iter()
            .chain(self.audio.iter().filter_map(|a| a.back().map(|p| p.pts)))
            .max()
            .unwrap_or(0);
        let cutoff = newest - self.keep;
        // The buffer must always start on a keyframe.
        while self.video.front().is_some_and(|p| !p.key) {
            self.pop_video(1);
        }
        // Drop whole GOPs while the next keyframe is still older than the cutoff.
        while let Some(next_key) = self.video.iter().skip(1).position(|p| p.key).map(|i| i + 1) {
            if self.video[next_key].pts > cutoff {
                break;
            }
            self.pop_video(next_key);
        }
        let audio_cutoff = self.video.front().map_or(cutoff, |p| p.pts);
        for queue in &mut self.audio {
            while queue.front().is_some_and(|p| p.pts < audio_cutoff) {
                let p = queue.pop_front().unwrap();
                self.bytes -= p.data.len();
            }
        }
    }

    fn pop_video(&mut self, n: usize) {
        for p in self.video.drain(..n) {
            self.bytes -= p.data.len();
        }
    }

    /// Grab the last `seconds` of footage, starting at the closest keyframe at
    /// or before the requested start.
    pub fn snapshot(&self, seconds: u32) -> Option<Clip> {
        let format = self.format.clone()?;
        let newest = self.video.back()?.pts;
        let start = newest - seconds as i64 * SECOND;
        let first = self
            .video
            .iter()
            .rposition(|p| p.key && p.pts <= start)
            .or_else(|| self.video.iter().position(|p| p.key))?;
        let video: Vec<_> = self.video.iter().skip(first).cloned().collect();
        let t0 = video[0].pts;
        let audio = self
            .audio
            .iter()
            .map(|a| a.iter().filter(|p| p.pts >= t0 && p.pts <= newest).cloned().collect())
            .collect();
        Some(Clip { format, video, audio, audio_names: self.audio_names.clone() })
    }

    /// Start queueing packets for a recording, beginning at the newest
    /// keyframe so the file can start right away.
    pub fn start_recording(&mut self) {
        let mut r = Recorded::default();
        if let Some(first) = self.video.iter().rposition(|p| p.key) {
            let t0 = self.video[first].pts;
            r.video = self.video.iter().skip(first).cloned().collect();
            for (track, queue) in self.audio.iter().enumerate() {
                r.audio.extend(queue.iter().filter(|p| p.pts >= t0).map(|p| (track, p.clone())));
            }
        }
        self.recording = Some(r);
    }

    /// Hand over the packets queued since the last call.
    pub fn take_recorded(&mut self) -> Recorded {
        let format = self.format.clone();
        match self.recording.as_mut() {
            Some(r) => Recorded {
                format,
                video: core::mem::take(&mut r.video),
                audio: core::mem::take(&mut r.audio),
                ended: r.ended,
            },
            None => Recorded { ended: true, ..Default::default() },
        }
    }

    pub fn stop_recording(&mut self) {
        self.recording = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Push `secs` seconds of 10 fps video with a keyframe every second.
    fn fill(ring: &mut Ring, from: i64, secs: i64) {
        for i in from * 10..(from + secs) * 10 {
            ring.push_video(Packet { pts: i * SECOND / 10, key: i % 10 == 0, data: vec![0; 100] });
            ring.push_audio(0, Packet { pts: i * SECOND / 10, key: true, data: vec![0; 10] });
        }
    }

    fn ring(seconds: u32) -> Ring {
        let mut ring = Ring::new(seconds);
        ring.set_audio_tracks(vec!["Audio".into()]);
        ring
    }

    #[test]
    fn counts_buffered_seconds() {
        let mut ring = ring(60);
        assert_eq!(ring.seconds(), 0);
        fill(&mut ring, 0, 12);
        assert_eq!(ring.seconds(), 11); // first to last frame
    }

    #[test]
    fn stops_growing_once_full() {
        let mut ring = ring(10);
        fill(&mut ring, 0, 40);
        // Keeps the replay length plus a little slack for keyframes, no more.
        let secs = ring.seconds();
        assert!((10..=14).contains(&secs), "buffered {secs}s");
    }

    #[test]
    fn recording_starts_at_a_keyframe_and_outlives_the_buffer() {
        let mut ring = ring(5);
        fill(&mut ring, 0, 3);
        // Halfway into a GOP: the recording starts back at its keyframe.
        ring.push_video(Packet { pts: 3 * SECOND, key: true, data: vec![0; 100] });
        ring.push_video(Packet { pts: 3 * SECOND + SECOND / 10, key: false, data: vec![0; 100] });
        ring.start_recording();
        let first = ring.take_recorded();
        assert_eq!(first.video.len(), 2);
        assert!(first.video[0].key && first.video[0].pts == 3 * SECOND);
        assert!(!first.ended);

        // Much longer than the replay length: nothing is lost to trimming.
        fill(&mut ring, 4, 30);
        let rest = ring.take_recorded();
        assert_eq!(rest.video.len(), 300);
        assert_eq!(rest.audio.len(), 300);
        assert!(ring.take_recorded().video.is_empty());

        ring.clear();
        assert!(ring.take_recorded().ended);
    }
}
