//! Cut a saved clip without re-encoding: the samples between two times are
//! copied into a new MP4. Video has to start on a keyframe, and the encoder
//! makes one per second (encoder.rs), so a cut can begin up to a second before
//! the asked start. Reads only the sample tables into memory; sample data is
//! streamed, so long recordings can be cut too.

use crate::prelude::*;

use std::fs::File;
use std::os::windows::fs::FileExt;

use crate::clock::SECOND;
use crate::mux::Mp4Writer;
use crate::ring::{Packet, VideoFormat};

type Result<T> = core::result::Result<T, String>;

/// Big-endian reader over a box body. Every read is bounds-checked, so a
/// broken file gives an error instead of a crash.
struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = rest;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_be_bytes(b.try_into().unwrap()))
    }
}

/// Child boxes of `data`: (type, body).
fn boxes(mut data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    core::iter::from_fn(move || {
        let mut r = R(data);
        let size = r.u32()? as usize;
        let kind: [u8; 4] = r.take(4)?.try_into().unwrap();
        let (head, size) = match size {
            1 => (16, r.u64()? as usize),
            0 => (8, data.len()),
            s => (8, s),
        };
        if size < head || size > data.len() {
            return None;
        }
        let body = &data[head..size];
        data = &data[size..];
        Some((kind, body))
    })
}

fn child<'a>(data: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
    path.iter().try_fold(data, |d, kind| boxes(d).find(|(k, _)| k == *kind).map(|(_, b)| b))
}

/// A full box's entries after version/flags and the entry count.
fn entries(body: &[u8]) -> Option<(u8, u32, R<'_>)> {
    let mut r = R(body);
    let version = r.u8()?;
    r.take(3)?;
    let count = r.u32()?;
    Some((version, count, r))
}

struct Avc {
    width: u32,
    height: u32,
    /// Annex B SPS + PPS.
    params: Vec<u8>,
    nal_len: usize,
}

struct Track {
    video: Option<Avc>,
    name: String,
    /// Sample times on the 100ns timeline, from the start of the file.
    times: Vec<i64>,
    sync: Vec<bool>,
    sizes: Vec<u32>,
    offsets: Vec<u64>,
}

fn parse_avc(stsd: &[u8]) -> Option<Avc> {
    let (_, _, r) = entries(stsd)?;
    let (kind, entry) = boxes(r.0).next()?;
    if &kind != b"avc1" {
        return None;
    }
    let mut e = R(entry);
    e.take(24)?;
    let (width, height) = (e.u16()? as u32, e.u16()? as u32);
    let avcc = child(entry.get(78..)?, &[b"avcC"])?;
    let mut a = R(avcc);
    a.take(4)?;
    let nal_len = (a.u8()? & 3) as usize + 1;
    let mut params = Vec::new();
    for mask in [0x1f, 0xff] {
        for _ in 0..(a.u8()? & mask) {
            let n = a.u16()? as usize;
            params.extend_from_slice(&[0, 0, 0, 1]);
            params.extend_from_slice(a.take(n)?);
        }
    }
    Some(Avc { width, height, params, nal_len })
}

fn parse_trak(trak: &[u8], movie_scale: u32) -> Option<Track> {
    let mdia = child(trak, &[b"mdia"])?;
    let mut mdhd = R(child(mdia, &[b"mdhd"])?);
    let skip = if mdhd.u8()? == 1 { 3 + 16 } else { 3 + 8 };
    mdhd.take(skip)?;
    let scale = mdhd.u32()?.max(1);

    let mut hdlr = R(child(mdia, &[b"hdlr"])?);
    hdlr.take(8)?;
    let handler = hdlr.take(4)?;
    hdlr.take(12)?;
    let name = String::from_utf8_lossy(hdlr.0.split(|&b| b == 0).next().unwrap_or_default()).into_owned();
    if handler != b"vide" && handler != b"soun" {
        return None;
    }

    // An empty first edit delays the track (audio that started after video).
    let mut start = 0i64;
    if let Some((0, n, mut r)) = child(trak, &[b"edts", b"elst"]).and_then(entries) {
        if n > 0 {
            let duration = r.u32()?;
            if r.u32()? == u32::MAX {
                start = duration as i64 * SECOND / movie_scale as i64;
            }
        }
    }

    let stbl = child(mdia, &[b"minf", b"stbl"])?;
    let table = |kind: &[u8; 4]| child(stbl, &[kind]).and_then(entries);

    let mut times = Vec::new();
    let (_, n, mut r) = table(b"stts")?;
    let mut dts = 0u64;
    for _ in 0..n {
        let (count, delta) = (r.u32()?, r.u32()?);
        for _ in 0..count {
            times.push(dts as i64);
            dts += delta as u64;
        }
    }
    if let Some((_, n, mut r)) = table(b"ctts") {
        let mut i = 0;
        for _ in 0..n {
            let (count, offset) = (r.u32()?, r.u32()? as i32 as i64);
            for _ in 0..count {
                *times.get_mut(i)? += offset;
                i += 1;
            }
        }
    }
    let to_time = |t: i64| start + (t as i128 * SECOND as i128 / scale as i128) as i64;
    let times: Vec<i64> = times.into_iter().map(to_time).collect();

    let mut sync = vec![true; times.len()];
    if let Some((_, n, mut r)) = table(b"stss") {
        sync.fill(false);
        for _ in 0..n {
            *sync.get_mut((r.u32()? as usize).checked_sub(1)?)? = true;
        }
    }

    let mut r = R(child(stbl, &[b"stsz"])?);
    r.take(4)?;
    let (fixed, count) = (r.u32()?, r.u32()? as usize);
    let sizes: Vec<u32> = if fixed > 0 { vec![fixed; count] } else { (0..count).map(|_| r.u32()).collect::<Option<_>>()? };

    let chunks: Vec<u64> = if let Some((_, n, mut r)) = table(b"co64") {
        (0..n).map(|_| r.u64()).collect::<Option<_>>()?
    } else {
        let (_, n, mut r) = table(b"stco")?;
        (0..n).map(|_| r.u32().map(u64::from)).collect::<Option<_>>()?
    };
    let (_, n, mut r) = table(b"stsc")?;
    // (first chunk, samples per chunk); the description index is skipped.
    let mut runs = Vec::new();
    for _ in 0..n {
        let (first, per_chunk, _) = (r.u32()?, r.u32()?, r.u32()?);
        runs.push((first, per_chunk));
    }
    let mut offsets = Vec::with_capacity(sizes.len());
    for (c, &chunk) in chunks.iter().enumerate() {
        let per_chunk = runs.iter().rev().find(|(first, _)| *first as usize <= c + 1).map_or(1, |r| r.1);
        let mut at = chunk;
        for _ in 0..per_chunk {
            let Some(&size) = sizes.get(offsets.len()) else { break };
            offsets.push(at);
            at += size as u64;
        }
    }
    if times.len() != sizes.len() || offsets.len() != sizes.len() {
        return None;
    }

    let video = if handler == b"vide" { Some(parse_avc(child(stbl, &[b"stsd"])?)?) } else { None };
    Some(Track { video, name, times, sync, sizes, offsets })
}

fn read_at(f: &File, at: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0; len];
    let mut done = 0;
    while done < len {
        match f.seek_read(&mut buf[done..], at + done as u64) {
            Ok(0) => return Err("the file ends early".into()),
            Ok(n) => done += n,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(buf)
}

fn read_moov(f: &File) -> Result<Vec<u8>> {
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    let mut pos = 0;
    while pos + 8 <= len {
        let head = read_at(f, pos, 16.min((len - pos) as usize))?;
        let mut r = R(&head);
        let (size, kind) = (r.u32().unwrap() as u64, r.take(4).unwrap());
        let (head, size) = match size {
            1 => (16, r.u64().ok_or("bad box")?),
            0 => (8, len - pos),
            s => (8, s),
        };
        if size < head {
            break;
        }
        if kind == b"moov" {
            return read_at(f, pos + head, (size - head) as usize);
        }
        pos += size;
    }
    Err("not an MP4 (no moov box)".into())
}

/// Copy `src` from `start` to `end` (100ns, from the start of the file) into
/// `dst`. Returns the new file's size.
pub fn trim(src: &str, dst: &str, start: i64, end: i64) -> Result<u64> {
    let f = File::open(src).map_err(|e| e.to_string())?;
    let moov = read_moov(&f)?;
    let mut mvhd = R(child(&moov, &[b"mvhd"]).ok_or("no mvhd")?);
    let skip = if mvhd.u8() == Some(1) { 3 + 16 } else { 3 + 8 };
    mvhd.take(skip);
    let movie_scale = mvhd.u32().unwrap_or(1000).max(1);
    let tracks: Vec<Track> = boxes(&moov).filter(|(k, _)| k == b"trak").filter_map(|(_, t)| parse_trak(t, movie_scale)).collect();
    let vi = tracks.iter().position(|t| t.video.is_some()).ok_or("no H.264 video track")?;
    let (video, avc) = (&tracks[vi], tracks[vi].video.as_ref().unwrap());

    // Decode order from the keyframe at or before `start`.
    let first = (0..video.times.len())
        .filter(|&i| video.sync[i] && video.times[i] <= start)
        .last()
        .or_else(|| video.sync.iter().position(|&s| s))
        .ok_or("no keyframe")?;
    let t0 = video.times[first];
    let frames: Vec<usize> = (first..video.times.len()).filter(|&i| video.times[i] < end).collect();
    if frames.is_empty() || end <= t0 {
        return Err("nothing between those times".into());
    }

    // (time, track, sample); video first on ties so the writer starts on it.
    let mut order: Vec<(i64, usize, usize)> = frames.iter().map(|&i| (video.times[i], vi, i)).collect();
    let audio: Vec<usize> = (0..tracks.len()).filter(|&t| t != vi).collect();
    for &t in &audio {
        let times = &tracks[t].times;
        order.extend((0..times.len()).filter(|&i| times[i] >= t0 && times[i] < end).map(|i| (times[i], t, i)));
    }
    order.sort_by_key(|&(time, t, _)| (time, t != vi));

    let span = video.times[*frames.last().unwrap()] - video.times[frames[0]];
    let fps = if span > 0 { ((frames.len() - 1) as i64 * SECOND + span / 2) / span } else { 30 };
    let names = audio.iter().map(|&t| tracks[t].name.clone()).collect();
    let part = format!("{dst}.part");
    let err = |e: windows::core::Error| e.message().to_string();
    let mut w = Mp4Writer::create(&part, names).map_err(err)?;
    w.set_format(VideoFormat { width: avc.width, height: avc.height, fps: fps as u32, seq_header: avc.params.clone() });
    let result = (|| {
        for &(time, t, i) in &order {
            let track = &tracks[t];
            let mut data = read_at(&f, track.offsets[i], track.sizes[i] as usize)?;
            if t == vi {
                data = to_annexb(&data, avc.nal_len);
                w.video(&Packet { pts: time, key: track.sync[i], data }).map_err(err)?;
            } else {
                let a = audio.iter().position(|&x| x == t).unwrap();
                w.audio(a, &Packet { pts: time, key: true, data }).map_err(err)?;
            }
        }
        w.finish().map_err(err)
    })();
    let size = result.and_then(|size| std::fs::rename(&part, dst).map(|_| size).map_err(|e| e.to_string()));
    if size.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    size
}

/// Length-prefixed NAL units to Annex B start codes.
fn to_annexb(mut data: &[u8], nal_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    while data.len() >= nal_len {
        let n = data[..nal_len].iter().fold(0usize, |n, &b| n << 8 | b as usize);
        let Some(nal) = data.get(nal_len..nal_len + n) else { break };
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        data = &data[nal_len + n..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::Clip;
    use alloc::sync::Arc;

    fn packet(pts: i64, key: bool, data: Vec<u8>) -> Arc<Packet> {
        Arc::new(Packet { pts, key, data })
    }

    #[test]
    fn cuts_from_the_keyframe_before_start_and_keeps_audio_in_range() {
        let dir = std::env::temp_dir().join("jarvis-trim-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (src, dst) = (dir.join("in.mp4"), dir.join("out.mp4"));
        let (src, dst) = (src.to_str().unwrap(), dst.to_str().unwrap());

        // 3 s at 10 fps, a keyframe every second; audio frames of 1024 samples.
        let sps = [0, 0, 0, 1, 0x67, 0x42, 0, 0x1f, 1];
        let pps = [0, 0, 0, 1, 0x68, 2];
        let video = (0..30)
            .map(|i| {
                let key = i % 10 == 0;
                let mut data = if key { [&sps[..], &pps[..]].concat() } else { Vec::new() };
                data.extend_from_slice(&[0, 0, 0, 1, if key { 0x65 } else { 0x41 }, i as u8]);
                packet(1000 + i * SECOND / 10, key, data)
            })
            .collect();
        let frame = 1024 * SECOND / 48_000;
        let audio = vec![(0..140).map(|i| packet(1000 + i * frame, true, vec![i as u8; 7])).collect()];
        let format = VideoFormat { width: 64, height: 32, fps: 10, seq_header: Vec::new() };
        let clip = Clip { format, video, audio, audio_names: vec!["Mix".into()] };
        crate::mux::write_mp4(src, &clip).unwrap();

        trim(src, dst, 15 * SECOND / 10, 25 * SECOND / 10).unwrap();
        assert!(!std::path::Path::new(&format!("{dst}.part")).exists());

        let f = File::open(dst).unwrap();
        let moov = read_moov(&f).unwrap();
        let tracks: Vec<Track> = boxes(&moov).filter(|(k, _)| k == b"trak").filter_map(|(_, t)| parse_trak(t, 1000)).collect();
        assert_eq!(tracks.len(), 2);
        let (v, a) = (&tracks[0], &tracks[1]);
        // Frames 10..25: from the keyframe at 1.0 s up to 2.5 s.
        assert_eq!(v.times.len(), 15);
        assert_eq!(v.sync.iter().filter(|&&s| s).count(), 2); // 1.0 s and 2.0 s
        assert!(v.sync[0] && v.sync[10]);
        assert_eq!(v.video.as_ref().unwrap().width, 64);
        let first = read_at(&f, v.offsets[1], v.sizes[1] as usize).unwrap();
        assert_eq!(first, [0, 0, 0, 2, 0x41, 11]);
        // Audio frames whose time is in [1.0 s, 2.5 s), same bytes as before.
        let kept: Vec<u8> =
            (0..140i64).filter(|&i| (SECOND..25 * SECOND / 10).contains(&(i * 1024 * SECOND / 48_000))).map(|i| i as u8).collect();
        assert_eq!(a.times.len(), kept.len());
        assert_eq!(a.name, "Mix");
        assert_eq!(read_at(&f, a.offsets[0], 7).unwrap(), vec![kept[0]; 7]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
