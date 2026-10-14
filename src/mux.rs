//! Minimal MP4 writer for H.264 + AAC-LC. Clips are written with the moov box
//! before mdat ("faststart") so they start playing immediately when uploaded
//! somewhere. Recordings are streamed to disk as they happen, so their moov box
//! goes at the end once the sample tables are known.
//!
//! With several audio tracks, the first is the full mix and the only one marked
//! enabled; the rest share its alternate group, so players pick one (the mix)
//! while video editors import them all.

use crate::prelude::*;

use alloc::sync::Arc;

use crate::rt::fs::{BufWriter, File};
use crate::rt::{self, error};

use crate::clock::SECOND;
use crate::ring::{Clip, Packet, VideoFormat};

pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u16 = 2;
pub const AUDIO_BITRATE: u32 = 192_000;
const AAC_FRAME: u32 = 1024;

const MOVIE_TIMESCALE: u32 = 1000;
const VIDEO_TIMESCALE: u32 = 90_000;
const VIDEO_TRACK: u32 = 1;

// ---------------------------------------------------------------------------
// Annex B helpers

/// Iterate NAL units in an Annex B byte stream (start codes stripped).
fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts.iter().skip(1).map(|&s| s - 3).chain([data.len()]).collect();
    starts.into_iter().zip(ends).map(move |(s, e)| {
        // A 4-byte start code leaves a zero at the end of the previous NAL.
        let mut e = e;
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        &data[s..e]
    })
}

fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// NALs that belong in the sample data (parameter sets live in avcC, and
/// access unit delimiters are pointless in MP4).
fn keep_nal(nal: &[u8]) -> bool {
    !nal.is_empty() && !matches!(nal_type(nal), 7 | 8 | 9)
}

fn avcc_size(annexb: &[u8]) -> u32 {
    nal_units(annexb).filter(|n| keep_nal(n)).map(|n| 4 + n.len() as u32).sum()
}

fn write_avcc(annexb: &[u8], w: &mut BufWriter) -> rt::Result<()> {
    for nal in nal_units(annexb).filter(|n| keep_nal(n)) {
        w.write_all(&(nal.len() as u32).to_be_bytes())?;
        w.write_all(nal)?;
    }
    Ok(())
}

/// SPS and PPS from the first of `sources` that has both.
fn find_param_sets<'a>(sources: impl IntoIterator<Item = &'a [u8]>) -> Option<(Vec<u8>, Vec<u8>)> {
    for data in sources {
        let mut sps = None;
        let mut pps = None;
        for nal in nal_units(data) {
            match nal_type(nal) {
                7 if sps.is_none() => sps = Some(nal.to_vec()),
                8 if pps.is_none() => pps = Some(nal.to_vec()),
                _ => {}
            }
        }
        if let (Some(s), Some(p)) = (sps, pps) {
            if s.len() >= 4 {
                return Some((s, p));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Box building

struct Mp4 {
    buf: Vec<u8>,
    stack: Vec<usize>,
}

impl Mp4 {
    fn new() -> Self {
        Self { buf: Vec::new(), stack: Vec::new() }
    }
    fn begin(&mut self, kind: &[u8; 4]) {
        self.stack.push(self.buf.len());
        self.u32(0);
        self.buf.extend_from_slice(kind);
    }
    fn begin_full(&mut self, kind: &[u8; 4], version: u8, flags: u32) {
        self.begin(kind);
        self.u32((version as u32) << 24 | flags);
    }
    fn end(&mut self) {
        let start = self.stack.pop().unwrap();
        let size = (self.buf.len() - start) as u32;
        self.buf[start..start + 4].copy_from_slice(&size.to_be_bytes());
    }
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    fn zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len() + n, 0);
    }
    fn matrix(&mut self) {
        for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
            self.u32(v);
        }
    }
}

fn ftyp() -> Vec<u8> {
    let mut m = Mp4::new();
    m.begin(b"ftyp");
    m.bytes(b"isom");
    m.u32(0x200);
    m.bytes(b"isomiso2avc1mp41");
    m.end();
    m.buf
}

struct Track {
    id: u32,
    timescale: u32,
    sizes: Vec<u32>,
    offsets: Vec<u64>,
    /// Run-length encoded (count, delta) sample durations.
    stts: Vec<(u32, u32)>,
    /// Run-length encoded (count, offset) composition offsets; empty when
    /// frames are never reordered.
    ctts: Vec<(u32, i32)>,
    /// 1-based sample numbers of sync samples (video only).
    sync: Vec<u32>,
    duration: u64,
    /// Delay before the first sample, in movie timescale.
    start_offset: u64,
}

impl Track {
    fn new(id: u32, timescale: u32) -> Self {
        Self {
            id,
            timescale,
            sizes: Vec::new(),
            offsets: Vec::new(),
            stts: Vec::new(),
            ctts: Vec::new(),
            sync: Vec::new(),
            duration: 0,
            start_offset: 0,
        }
    }

    /// Video sample table from presentation times (shared clock), in decode
    /// order. Decode times are the sorted presentation times, which also covers
    /// encoders that emit B-frames (their reordering goes into ctts). Durations
    /// come from timestamp deltas so dropped frames keep the clip in sync with
    /// audio.
    fn video(pts: &[i64], t0: i64, keys: &[bool], sizes: Vec<u32>, fps: u32) -> Self {
        let mut t = Self::new(VIDEO_TRACK, VIDEO_TIMESCALE);
        let pts: Vec<i64> = pts.iter().map(|&p| to_scale(p - t0, VIDEO_TIMESCALE) as i64).collect();
        let mut dts = pts.clone();
        dts.sort_unstable();
        let nominal = VIDEO_TIMESCALE / fps.max(1);
        for (i, &key) in keys.iter().enumerate() {
            if key {
                t.sync.push(i as u32 + 1);
            }
            let d = dts.get(i + 1).map_or(nominal, |next| (next - dts[i]) as u32);
            t.push_duration(d.max(1));
        }
        if pts != dts {
            for (p, d) in pts.iter().zip(&dts) {
                let offset = (p - d) as i32;
                match t.ctts.last_mut() {
                    Some((count, o)) if *o == offset => *count += 1,
                    _ => t.ctts.push((1, offset)),
                }
            }
        }
        t.sizes = sizes;
        t
    }

    /// Audio sample table: back-to-back AAC frames starting at `first_pts`.
    fn audio(id: u32, first_pts: i64, t0: i64, sizes: Vec<u32>) -> Self {
        let mut t = Self::new(id, AUDIO_RATE);
        t.start_offset = to_scale(first_pts - t0, MOVIE_TIMESCALE);
        for _ in 0..sizes.len() {
            t.push_duration(AAC_FRAME);
        }
        t.sizes = sizes;
        t
    }

    fn push_duration(&mut self, d: u32) {
        match self.stts.last_mut() {
            Some((count, delta)) if *delta == d => *count += 1,
            _ => self.stts.push((1, d)),
        }
        self.duration += d as u64;
    }

    fn movie_duration(&self) -> u32 {
        (self.duration * MOVIE_TIMESCALE as u64 / self.timescale as u64) as u32
    }

    fn write_stbl_tables(&self, m: &mut Mp4) {
        m.begin_full(b"stts", 0, 0);
        m.u32(self.stts.len() as u32);
        for &(count, delta) in &self.stts {
            m.u32(count);
            m.u32(delta);
        }
        m.end();

        if !self.ctts.is_empty() {
            m.begin_full(b"ctts", 1, 0); // version 1: signed offsets
            m.u32(self.ctts.len() as u32);
            for &(count, offset) in &self.ctts {
                m.u32(count);
                m.u32(offset as u32);
            }
            m.end();
        }

        if self.id == VIDEO_TRACK {
            m.begin_full(b"stss", 0, 0);
            m.u32(self.sync.len() as u32);
            for &s in &self.sync {
                m.u32(s);
            }
            m.end();
        }

        // One sample per chunk keeps the bookkeeping trivial.
        m.begin_full(b"stsc", 0, 0);
        m.u32(1);
        m.u32(1);
        m.u32(1);
        m.u32(1);
        m.end();

        m.begin_full(b"stsz", 0, 0);
        m.u32(0);
        m.u32(self.sizes.len() as u32);
        for &s in &self.sizes {
            m.u32(s);
        }
        m.end();

        m.begin_full(b"co64", 0, 0);
        m.u32(self.offsets.len() as u32);
        for &o in &self.offsets {
            m.u64(o);
        }
        m.end();
    }

    fn write_header(&self, m: &mut Mp4, width: u32, height: u32) {
        let audio = self.id != VIDEO_TRACK;
        // Only the first audio track is enabled; the others are alternatives.
        let enabled = self.id <= VIDEO_TRACK + 1;
        m.begin_full(b"tkhd", 0, if enabled { 3 } else { 2 }); // enabled | in movie
        m.u32(0);
        m.u32(0);
        m.u32(self.id);
        m.u32(0);
        m.u32(self.movie_duration() + self.start_offset as u32);
        m.zeros(8);
        m.u16(0); // layer
        m.u16(if audio { 1 } else { 0 }); // alternate group
        m.u16(if audio { 0x0100 } else { 0 }); // volume
        m.u16(0);
        m.matrix();
        m.u32(width << 16);
        m.u32(height << 16);
        m.end();

        if self.start_offset > 0 {
            m.begin(b"edts");
            m.begin_full(b"elst", 0, 0);
            m.u32(2);
            m.u32(self.start_offset as u32); // empty edit
            m.u32(u32::MAX); // media_time = -1
            m.u32(0x0001_0000);
            m.u32(self.movie_duration());
            m.u32(0);
            m.u32(0x0001_0000);
            m.end();
            m.end();
        }
    }

    fn write_mdhd(&self, m: &mut Mp4, handler: &[u8; 4], name: &str) {
        m.begin_full(b"mdhd", 0, 0);
        m.u32(0);
        m.u32(0);
        m.u32(self.timescale);
        m.u32(self.duration as u32);
        m.u16(0x55c4); // "und"
        m.u16(0);
        m.end();

        m.begin_full(b"hdlr", 0, 0);
        m.u32(0);
        m.bytes(handler);
        m.zeros(12);
        m.bytes(name.as_bytes());
        m.u8(0);
        m.end();
    }
}

fn write_dinf(m: &mut Mp4) {
    m.begin(b"dinf");
    m.begin_full(b"dref", 0, 0);
    m.u32(1);
    m.begin_full(b"url ", 0, 1);
    m.end();
    m.end();
    m.end();
}

fn write_moov(f: &VideoFormat, sps: &[u8], pps: &[u8], video: &Track, audio: &[(&Track, &str)]) -> Vec<u8> {
    let mut m = Mp4::new();
    m.begin(b"moov");

    let movie_duration = audio
        .iter()
        .map(|(a, _)| a.movie_duration() + a.start_offset as u32)
        .chain([video.movie_duration()])
        .max()
        .unwrap_or(0);
    m.begin_full(b"mvhd", 0, 0);
    m.u32(0);
    m.u32(0);
    m.u32(MOVIE_TIMESCALE);
    m.u32(movie_duration);
    m.u32(0x0001_0000); // rate
    m.u16(0x0100); // volume
    m.zeros(10);
    m.matrix();
    m.zeros(24);
    m.u32(VIDEO_TRACK + 1 + audio.len() as u32); // next track id
    m.end();

    // --- video track
    m.begin(b"trak");
    video.write_header(&mut m, f.width, f.height);
    m.begin(b"mdia");
    video.write_mdhd(&mut m, b"vide", "VideoHandler");
    m.begin(b"minf");
    m.begin_full(b"vmhd", 0, 1);
    m.zeros(8);
    m.end();
    write_dinf(&mut m);
    m.begin(b"stbl");
    m.begin_full(b"stsd", 0, 0);
    m.u32(1);
    m.begin(b"avc1");
    m.zeros(6);
    m.u16(1); // data reference index
    m.zeros(16);
    m.u16(f.width as u16);
    m.u16(f.height as u16);
    m.u32(0x0048_0000);
    m.u32(0x0048_0000);
    m.u32(0);
    m.u16(1); // frame count
    let mut compressor = [0u8; 32];
    compressor[0] = 10;
    compressor[1..11].copy_from_slice(b"bufferless");
    m.bytes(&compressor);
    m.u16(0x0018);
    m.u16(0xffff);
    m.begin(b"avcC");
    m.u8(1);
    m.u8(sps[1]);
    m.u8(sps[2]);
    m.u8(sps[3]);
    m.u8(0xff); // 4-byte NAL lengths
    m.u8(0xe1); // 1 SPS
    m.u16(sps.len() as u16);
    m.bytes(sps);
    m.u8(1);
    m.u16(pps.len() as u16);
    m.bytes(pps);
    m.end();
    // BT.709 limited range, matching what the video processor outputs.
    m.begin(b"colr");
    m.bytes(b"nclx");
    m.u16(1);
    m.u16(1);
    m.u16(1);
    m.u8(0);
    m.end();
    m.end(); // avc1
    m.end(); // stsd
    video.write_stbl_tables(&mut m);
    m.end(); // stbl
    m.end(); // minf
    m.end(); // mdia
    m.end(); // trak

    // --- audio tracks
    for (audio, name) in audio {
        m.begin(b"trak");
        audio.write_header(&mut m, 0, 0);
        m.begin(b"mdia");
        audio.write_mdhd(&mut m, b"soun", name);
        m.begin(b"minf");
        m.begin_full(b"smhd", 0, 0);
        m.u32(0);
        m.end();
        write_dinf(&mut m);
        m.begin(b"stbl");
        m.begin_full(b"stsd", 0, 0);
        m.u32(1);
        m.begin(b"mp4a");
        m.zeros(6);
        m.u16(1);
        m.zeros(8);
        m.u16(AUDIO_CHANNELS);
        m.u16(16);
        m.u32(0);
        m.u32(AUDIO_RATE << 16);
        m.begin_full(b"esds", 0, 0);
        // ES_Descriptor
        m.u8(0x03);
        m.u8(25);
        m.u16(audio.id as u16); // ES_ID
        m.u8(0);
        // DecoderConfigDescriptor
        m.u8(0x04);
        m.u8(17);
        m.u8(0x40); // MPEG-4 audio
        m.u8(0x15); // audio stream
        m.u8(0);
        m.u16(0); // buffer size
        m.u32(AUDIO_BITRATE);
        m.u32(AUDIO_BITRATE);
        // DecoderSpecificInfo: AudioSpecificConfig for AAC-LC
        m.u8(0x05);
        m.u8(2);
        let freq_index: u16 = match AUDIO_RATE {
            44_100 => 4,
            _ => 3, // 48 kHz
        };
        m.u16(2 << 11 | freq_index << 7 | (AUDIO_CHANNELS << 3));
        // SLConfigDescriptor
        m.u8(0x06);
        m.u8(1);
        m.u8(0x02);
        m.end(); // esds
        m.end(); // mp4a
        m.end(); // stsd
        audio.write_stbl_tables(&mut m);
        m.end();
        m.end();
        m.end();
        m.end();
    }

    m.end(); // moov
    m.buf
}

/// Convert a timestamp relative to the clip start into track timescale units.
fn to_scale(t: i64, scale: u32) -> u64 {
    (t.max(0) as i128 * scale as i128 / SECOND as i128) as u64
}

// ---------------------------------------------------------------------------
// Clips: everything is in memory, so the moov box goes first.

pub fn write_mp4(path: &str, clip: &Clip) -> rt::Result<u64> {
    let invalid = error;
    let param_sources =
        clip.video.iter().filter(|p| p.key).map(|p| p.data.as_slice()).chain([clip.format.seq_header.as_slice()]);
    let (sps, pps) = find_param_sets(param_sources).ok_or_else(|| invalid("no SPS/PPS in stream"))?;
    let t0 = clip.video.iter().map(|p| p.pts).min().ok_or_else(|| invalid("empty clip"))?;

    let pts: Vec<i64> = clip.video.iter().map(|p| p.pts).collect();
    let keys: Vec<bool> = clip.video.iter().map(|p| p.key).collect();
    let sizes = clip.video.iter().map(|p| avcc_size(&p.data)).collect();
    let mut video = Track::video(&pts, t0, &keys, sizes, clip.format.fps);

    // Tracks without packets are left out, but the rest keep their order.
    let mut audio: Vec<(Track, &str, &[Arc<Packet>])> = Vec::new();
    for (packets, name) in clip.audio.iter().zip(&clip.audio_names) {
        if let Some(first) = packets.first() {
            let id = VIDEO_TRACK + 1 + audio.len() as u32;
            let sizes = packets.iter().map(|p| p.data.len() as u32).collect();
            audio.push((Track::audio(id, first.pts, t0, sizes), name, packets));
        }
    }

    // Interleave samples by timestamp: (track index, sample index), where
    // track 0 is video and n is audio track n - 1.
    let mut order: Vec<(usize, usize, i64)> = Vec::new();
    order.extend(clip.video.iter().enumerate().map(|(i, p)| (0, i, p.pts)));
    for (t, (_, _, packets)) in audio.iter().enumerate() {
        order.extend(packets.iter().enumerate().map(|(i, p)| (t + 1, i, p.pts)));
    }
    order.sort_by_key(|&(_, _, pts)| pts);

    let assign_offsets = |base: u64, video: &mut Track, audio: &mut [(Track, &str, &[Arc<Packet>])]| {
        video.offsets.clear();
        for a in audio.iter_mut() {
            a.0.offsets.clear();
        }
        let mut off = base;
        for &(t, i, _) in &order {
            let track = if t == 0 { &mut *video } else { &mut audio[t - 1].0 };
            track.offsets.push(off);
            off += track.sizes[i] as u64;
        }
        off - base
    };

    let ftyp = ftyp();
    let moov = |video: &Track, audio: &[(Track, &str, &[Arc<Packet>])]| {
        let tracks: Vec<(&Track, &str)> = audio.iter().map(|(t, name, _)| (t, *name)).collect();
        write_moov(&clip.format, &sps, &pps, video, &tracks)
    };

    // co64 entries are fixed size, so the moov size doesn't depend on the
    // offsets: build once to measure, then again with real offsets.
    let mdat_payload = assign_offsets(0, &mut video, &mut audio);
    let moov_len = moov(&video, &audio).len() as u64;
    let mdat_header = 16u64;
    let base = ftyp.len() as u64 + moov_len + mdat_header;
    assign_offsets(base, &mut video, &mut audio);
    let moov = moov(&video, &audio);
    debug_assert_eq!(moov.len() as u64, moov_len);

    let mut w = BufWriter::new(File::create(path)?, 1 << 20);
    w.write_all(&ftyp)?;
    w.write_all(&moov)?;
    w.write_all(&1u32.to_be_bytes())?;
    w.write_all(b"mdat")?;
    w.write_all(&(mdat_header + mdat_payload).to_be_bytes())?;
    for &(t, i, _) in &order {
        if t == 0 {
            write_avcc(&clip.video[i].data, &mut w)?;
        } else {
            w.write_all(&audio[t - 1].2[i].data)?;
        }
    }
    w.flush()?;
    Ok(base + mdat_payload)
}

// ---------------------------------------------------------------------------
// Recordings: samples go to disk as they arrive, and the moov box is appended
// when the recording stops.

#[derive(Default)]
struct AudioSamples {
    first_pts: Option<i64>,
    sizes: Vec<u32>,
    offsets: Vec<u64>,
}

pub struct Mp4Writer {
    w: BufWriter,
    /// Where the next byte goes.
    pos: u64,
    /// Offset of the mdat box header.
    mdat: u64,
    /// Presentation time of the first video frame (a keyframe).
    t0: Option<i64>,
    params: Option<(Vec<u8>, Vec<u8>)>,
    format: Option<VideoFormat>,
    video_pts: Vec<i64>,
    video_keys: Vec<bool>,
    video_sizes: Vec<u32>,
    video_offsets: Vec<u64>,
    audio: Vec<AudioSamples>,
    audio_names: Vec<String>,
}

impl Mp4Writer {
    pub fn create(path: &str, audio_names: Vec<String>) -> rt::Result<Self> {
        let mut w = BufWriter::new(File::create(path)?, 1 << 20);
        let ftyp = ftyp();
        w.write_all(&ftyp)?;
        // 64-bit size, filled in by finish().
        w.write_all(&1u32.to_be_bytes())?;
        w.write_all(b"mdat")?;
        w.write_all(&0u64.to_be_bytes())?;
        Ok(Self {
            w,
            pos: ftyp.len() as u64 + 16,
            mdat: ftyp.len() as u64,
            t0: None,
            params: None,
            format: None,
            video_pts: Vec::new(),
            video_keys: Vec::new(),
            video_sizes: Vec::new(),
            video_offsets: Vec::new(),
            audio: audio_names.iter().map(|_| AudioSamples::default()).collect(),
            audio_names,
        })
    }

    pub fn set_format(&mut self, format: VideoFormat) {
        self.format = Some(format);
    }

    /// The recording starts at the first keyframe; anything before is dropped.
    pub fn video(&mut self, p: &Packet) -> rt::Result<()> {
        if self.t0.is_none() {
            if !p.key {
                return Ok(());
            }
            self.t0 = Some(p.pts);
        }
        if self.params.is_none() && p.key {
            self.params = find_param_sets([p.data.as_slice()]);
        }
        let size = avcc_size(&p.data);
        self.video_pts.push(p.pts);
        self.video_keys.push(p.key);
        self.video_sizes.push(size);
        self.video_offsets.push(self.pos);
        write_avcc(&p.data, &mut self.w)?;
        self.pos += size as u64;
        Ok(())
    }

    pub fn audio(&mut self, track: usize, p: &Packet) -> rt::Result<()> {
        let Some(t0) = self.t0 else { return Ok(()) };
        let Some(a) = self.audio.get_mut(track) else { return Ok(()) };
        if p.pts < t0 {
            return Ok(());
        }
        a.first_pts.get_or_insert(p.pts);
        a.sizes.push(p.data.len() as u32);
        a.offsets.push(self.pos);
        self.w.write_all(&p.data)?;
        self.pos += p.data.len() as u64;
        Ok(())
    }

    /// Seconds of video written so far.
    pub fn seconds(&self) -> u32 {
        match (self.video_pts.first(), self.video_pts.last()) {
            (Some(first), Some(last)) => ((last - first) / SECOND) as u32,
            _ => 0,
        }
    }

    /// Write the moov box and fix up the mdat size. Returns the file size.
    pub fn finish(mut self) -> rt::Result<u64> {
        let t0 = self.t0.ok_or_else(|| error("nothing was recorded"))?;
        let mut format = self.format.clone().ok_or_else(|| error("no video format"))?;
        let (sps, pps) = match self.params.take() {
            Some(p) => p,
            None => find_param_sets([format.seq_header.as_slice()]).ok_or_else(|| error("no SPS/PPS in stream"))?,
        };
        format.seq_header.clear();

        let mut video = Track::video(
            &self.video_pts,
            t0,
            &self.video_keys,
            core::mem::take(&mut self.video_sizes),
            format.fps,
        );
        video.offsets = core::mem::take(&mut self.video_offsets);
        let mut audio = Vec::new();
        for (a, name) in self.audio.iter_mut().zip(&self.audio_names) {
            if let Some(first) = a.first_pts {
                let id = VIDEO_TRACK + 1 + audio.len() as u32;
                let mut t = Track::audio(id, first, t0, core::mem::take(&mut a.sizes));
                t.offsets = core::mem::take(&mut a.offsets);
                audio.push((t, name.as_str()));
            }
        }
        let audio: Vec<(&Track, &str)> = audio.iter().map(|(t, name)| (t, *name)).collect();
        let moov = write_moov(&format, &sps, &pps, &video, &audio);
        let mdat_size = self.pos - self.mdat;
        self.w.write_all(&moov)?;
        self.w.seek(self.mdat + 8)?;
        self.w.write_all(&mdat_size.to_be_bytes())?;
        self.w.flush()?;
        Ok(self.pos + moov.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annexb() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
        let nals: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]);
        assert_eq!(avcc_size(&data), 4 + 3);
    }
}
