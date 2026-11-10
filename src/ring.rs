//! Replay buffer of *encoded* packets. Saving a clip never re-encodes, it just
//! copies packets from the last keyframe before the requested start. While a
//! recording is running, new packets are also queued up for it.
//!
//! With `spool_to`, packet data goes to temp files on disk and only a small
//! index stays in memory; otherwise everything is kept in RAM.

use crate::prelude::*;

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use crate::clock::SECOND;
use crate::log;
use crate::rt::{fs, path};

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

/// A clip's packets, still where the ring keeps them. `load` reads them in;
/// call it off the UI thread, since that can mean reading spool files.
pub struct Snapshot {
    format: VideoFormat,
    video: Vec<Arc<Slot>>,
    audio: Vec<Vec<Arc<Slot>>>,
    audio_names: Vec<String>,
}

impl Snapshot {
    pub fn load(self) -> Result<Clip, String> {
        let load = |slots: &[Arc<Slot>]| {
            slots.iter().map(|s| s.packet()).collect::<Option<Vec<_>>>().ok_or("couldn't read the replay buffer back from disk")
        };
        let video = load(&self.video)?;
        let audio = self.audio.iter().map(|a| load(a)).collect::<Result<_, _>>()?;
        Ok(Clip { format: self.format, video, audio, audio_names: self.audio_names })
    }
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

/// The ring's own queue for a recording, before the data is read back.
#[derive(Default)]
struct Queued {
    video: Vec<Arc<Slot>>,
    audio: Vec<(usize, Arc<Slot>)>,
    ended: bool,
}

/// A packet as the ring holds it.
pub struct Slot {
    pts: i64,
    key: bool,
    len: usize,
    data: Stored,
}

enum Stored {
    Memory(Arc<Packet>),
    /// Offset in a spool file. The file deletes itself once no slot uses it.
    Disk(Arc<Spool>, u64),
}

impl Slot {
    fn packet(&self) -> Option<Arc<Packet>> {
        match &self.data {
            Stored::Memory(p) => Some(p.clone()),
            Stored::Disk(spool, at) => {
                let data = spool.read(*at, self.len)?;
                Some(Arc::new(Packet { pts: self.pts, key: self.key, data }))
            }
        }
    }
}

/// A temp file of packet data. Opened delete-on-close, so Windows removes it
/// when the last slot in it is dropped, and also if the app exits or crashes.
pub struct Spool(std::fs::File);

impl Spool {
    fn create(path: &str) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
        const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .attributes(FILE_ATTRIBUTE_TEMPORARY)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
            .open(path)
            .map(Self)
    }

    fn write(&self, mut at: u64, mut data: &[u8]) -> std::io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !data.is_empty() {
            let n = self.0.seek_write(data, at)?;
            if n == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            data = &data[n..];
            at += n as u64;
        }
        Ok(())
    }

    fn read(&self, mut at: u64, len: usize) -> Option<Vec<u8>> {
        use std::os::windows::fs::FileExt;
        let mut buf = vec![0; len];
        let mut done = 0;
        while done < len {
            match self.0.seek_read(&mut buf[done..], at) {
                Ok(0) | Err(_) => return None,
                Ok(n) => {
                    done += n;
                    at += n as u64;
                }
            }
        }
        Some(buf)
    }
}

/// A new spool file every few seconds, so old footage is deleted in chunks
/// as it ages out of the buffer.
const SPOOL_FILE_SECONDS: i64 = 5;

struct Spooler {
    dir: String,
    /// The file being appended to, its length, and its first packet's pts.
    current: Option<(Arc<Spool>, u64, i64)>,
    next_id: u64,
}

impl Spooler {
    fn write(&mut self, pts: i64, data: &[u8]) -> std::io::Result<(Arc<Spool>, u64)> {
        if self.current.as_ref().is_none_or(|(_, _, first)| pts - first >= SPOOL_FILE_SECONDS * SECOND) {
            self.next_id += 1;
            let file = path::join(&self.dir, &format!("spool-{}.bin", self.next_id));
            self.current = Some((Arc::new(Spool::create(&file)?), 0, pts));
        }
        let (spool, len, _) = self.current.as_mut().unwrap();
        spool.write(*len, data)?;
        let at = *len;
        *len += data.len() as u64;
        Ok((spool.clone(), at))
    }
}

#[derive(Default)]
pub struct Ring {
    video: VecDeque<Arc<Slot>>,
    audio: Vec<VecDeque<Arc<Slot>>>,
    audio_names: Vec<String>,
    format: Option<VideoFormat>,
    /// How much history to keep, 100ns units.
    keep: i64,
    bytes: usize,
    recording: Option<Queued>,
    spooler: Option<Spooler>,
}

impl Ring {
    pub fn new(seconds: u32) -> Self {
        // A couple of seconds of slack so we can always start on a keyframe.
        Self { keep: (seconds as i64 + 3) * SECOND, ..Default::default() }
    }

    /// Where new packets go: temp files under `dir`, or RAM if `dir` is empty.
    /// Packets already buffered stay where they are until they age out.
    pub fn set_spool(&mut self, dir: &str) {
        if dir.is_empty() {
            self.spooler = None;
        } else if self.spooler.as_ref().is_none_or(|s| s.dir != dir) {
            self.spool_to(dir);
        }
    }

    /// Keep packet data in temp files under `dir` instead of in memory.
    fn spool_to(&mut self, dir: &str) {
        match fs::create_dir_all(dir) {
            Ok(()) => {
                log!("replay buffer on disk in {dir}");
                self.spooler = Some(Spooler { dir: dir.into(), current: None, next_id: 0 });
            }
            Err(e) => log!("can't use {dir} for the replay buffer ({e}), keeping it in memory"),
        }
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

    /// Wrap a packet for the ring, writing its data out when spooling.
    fn slot(&mut self, p: Packet) -> Arc<Slot> {
        let (pts, key, len) = (p.pts, p.key, p.data.len());
        let data = match self.spooler.as_mut().map(|s| s.write(pts, &p.data)) {
            Some(Ok((spool, at))) => Stored::Disk(spool, at),
            Some(Err(e)) => {
                log!("replay buffer disk write failed ({e}), keeping it in memory from now on");
                self.spooler = None;
                Stored::Memory(Arc::new(p))
            }
            None => Stored::Memory(Arc::new(p)),
        };
        self.bytes += len;
        Arc::new(Slot { pts, key, len, data })
    }

    pub fn push_video(&mut self, p: Packet) {
        let s = self.slot(p);
        if let Some(r) = self.recording.as_mut().filter(|r| !r.ended) {
            r.video.push(s.clone());
        }
        self.video.push_back(s);
        self.trim();
    }

    pub fn push_audio(&mut self, track: usize, p: Packet) {
        if track >= self.audio.len() {
            return;
        }
        let s = self.slot(p);
        if let Some(r) = self.recording.as_mut().filter(|r| !r.ended) {
            r.audio.push((track, s.clone()));
        }
        self.audio[track].push_back(s);
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
                self.bytes -= p.len;
            }
        }
    }

    fn pop_video(&mut self, n: usize) {
        for p in self.video.drain(..n) {
            self.bytes -= p.len;
        }
    }

    /// Grab the last `seconds` of footage, starting at the closest keyframe at
    /// or before the requested start.
    pub fn snapshot(&self, seconds: u32) -> Option<Snapshot> {
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
        Some(Snapshot { format, video, audio, audio_names: self.audio_names.clone() })
    }

    /// Start queueing packets for a recording, beginning at the newest
    /// keyframe so the file can start right away.
    pub fn start_recording(&mut self) {
        let mut r = Queued::default();
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
            Some(r) => {
                // A packet that can't be read back is skipped rather than
                // ending the whole recording.
                let video = core::mem::take(&mut r.video).iter().filter_map(|s| s.packet()).collect();
                let audio = core::mem::take(&mut r.audio).iter().filter_map(|(t, s)| Some((*t, s.packet()?))).collect();
                Recorded { format, video, audio, ended: r.ended }
            }
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

    #[test]
    fn spools_to_disk_reads_back_and_deletes_old_files() {
        let dir = std::env::temp_dir().join("jarvis-ring-test").to_string_lossy().into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        let mut ring = ring(10);
        ring.set_format(VideoFormat { width: 2, height: 2, fps: 10, seq_header: Vec::new() });
        ring.spool_to(&dir);
        // Every packet's bytes encode its index, so read-back mixups show.
        for i in 0..400i64 {
            let data = (i as u32).to_le_bytes().repeat(25);
            ring.push_video(Packet { pts: i * SECOND / 10, key: i % 10 == 0, data });
        }
        let clip = ring.snapshot(5).unwrap().load().unwrap();
        for p in &clip.video {
            let i = (p.pts * 10 / SECOND) as u32;
            assert_eq!(p.data, i.to_le_bytes().repeat(25));
        }
        assert!(clip.video.len() >= 50);
        // 40 s pushed, ~13 s kept: old 5 s files are gone, not piling up.
        let files = || std::fs::read_dir(&dir).unwrap().count();
        assert!(files() <= 5, "{} spool files left", files());
        drop(clip);
        ring.clear();
        ring.spooler = None; // closes the file still being written
        assert_eq!(files(), 0);
    }
}
