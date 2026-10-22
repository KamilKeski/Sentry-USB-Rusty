//! Presentation times of a video track's samples, from the MP4 sample tables.
//!
//! Each sample is one video frame. Tesla writes a telemetry SEI NAL into a
//! frame's sample only while it has telemetry to report (parked footage
//! carries none), so a SEI's time is the time of the sample that contains
//! it, never its position in the list of SEIs. The sample's byte range
//! (stsc + stsz + stco/co64) locates it; its decode time (stts) plus
//! composition offset (ctts, for B-frames), minus the edit list's start
//! (elst), is when a player shows it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use anyhow::{bail, Context, Result};

/// Largest `moov` we read. Tesla's is tens of KB for a one-minute clip.
const MAX_MOOV_BYTES: u64 = 16 * 1024 * 1024;

/// One video sample: where it sits in the file and when it is shown.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Span {
    offset: u64,
    size: u64,
    time_sec: f64,
}

/// The video track's samples, ordered by file offset.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoSampleTimes {
    spans: Vec<Span>,
}

impl VideoSampleTimes {
    /// Presentation time, in seconds from the start of the video, of the
    /// sample whose bytes contain `offset`.
    pub fn time_at_offset(&self, offset: u64) -> Option<f64> {
        let idx = self.spans.partition_point(|s| s.offset <= offset).checked_sub(1)?;
        let span = self.spans[idx];
        (offset < span.offset + span.size).then_some(span.time_sec)
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

/// Reads the sample tables of the first video track in `path`.
pub fn video_sample_times(path: &str) -> Result<VideoSampleTimes> {
    let mut f = File::open(path).with_context(|| format!("failed to open MP4 file: {}", path))?;
    let moov = read_top_level_box(&mut f, b"moov")?.context("no moov box")?;
    parse_moov(&moov)
}

fn read_top_level_box(f: &mut File, want: &[u8; 4]) -> Result<Option<Vec<u8>>> {
    let file_size = f.metadata()?.len();
    let mut pos = 0u64;
    while pos + 8 <= file_size {
        f.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 16];
        f.read_exact(&mut header[..8])?;
        let mut size = u32::from_be_bytes(header[..4].try_into().unwrap()) as u64;
        let mut header_size = 8u64;
        if size == 1 {
            f.read_exact(&mut header[8..16])?;
            size = u64::from_be_bytes(header[8..16].try_into().unwrap());
            header_size = 16;
        } else if size == 0 {
            size = file_size - pos;
        }
        if size < header_size {
            bail!("corrupt box at {}", pos);
        }
        if &header[4..8] == want {
            let body = size - header_size;
            if body > MAX_MOOV_BYTES || pos + size > file_size {
                bail!("{} box too large or truncated", String::from_utf8_lossy(want));
            }
            let mut data = vec![0u8; body as usize];
            f.read_exact(&mut data)?;
            return Ok(Some(data));
        }
        pos += size;
    }
    Ok(None)
}

/// Child boxes of a container's payload: (type, payload).
fn children(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let mut size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();
        let mut header = 8usize;
        if size == 1 {
            if pos + 16 > data.len() {
                break;
            }
            size = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap());
            header = 16;
        } else if size == 0 {
            size = (data.len() - pos) as u64;
        }
        let end = match usize::try_from(size).ok().and_then(|s| pos.checked_add(s)) {
            Some(end) if size as usize >= header && end <= data.len() => end,
            _ => break,
        };
        out.push((kind, &data[pos + header..end]));
        pos = end;
    }
    out
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    children(data).into_iter().find(|(k, _)| k == kind).map(|(_, body)| body)
}

fn be_u32(data: &[u8], at: usize) -> Result<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
        .context("sample table truncated")
}

fn be_u64(data: &[u8], at: usize) -> Result<u64> {
    data.get(at..at + 8)
        .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
        .context("sample table truncated")
}

/// Entries of a full-box table: (version, entry count, entries start).
fn table(data: &[u8], entry_size: usize) -> Result<(u8, usize, usize)> {
    let version = *data.first().context("empty table")?;
    let count = be_u32(data, 4)? as usize;
    if count.checked_mul(entry_size).is_none_or(|bytes| 8 + bytes > data.len()) {
        bail!("table count exceeds its box");
    }
    Ok((version, count, 8))
}

/// Expands (repeat, value) runs (stts, ctts) to one value per sample.
fn expand_runs(data: &[u8], samples: usize, signed: bool) -> Result<Vec<i64>> {
    let (version, count, start) = table(data, 8)?;
    let mut out = Vec::with_capacity(samples);
    for i in 0..count {
        let at = start + i * 8;
        let repeat = be_u32(data, at)? as usize;
        let raw = be_u32(data, at + 4)?;
        let value = if signed && version == 1 { raw as i32 as i64 } else { raw as i64 };
        if repeat > samples - out.len() {
            bail!("timing table covers more samples than the track has");
        }
        out.extend(std::iter::repeat_n(value, repeat));
    }
    if out.len() != samples {
        bail!("timing table covers {} of {} samples", out.len(), samples);
    }
    Ok(out)
}

fn parse_moov(moov: &[u8]) -> Result<VideoSampleTimes> {
    for (kind, trak) in children(moov) {
        if &kind != b"trak" {
            continue;
        }
        let Some(mdia) = child(trak, b"mdia") else { continue };
        let is_video = child(mdia, b"hdlr").and_then(|h| h.get(8..12)) == Some(b"vide");
        if is_video {
            return parse_video_trak(trak, mdia);
        }
    }
    bail!("no video track")
}

fn parse_video_trak(trak: &[u8], mdia: &[u8]) -> Result<VideoSampleTimes> {
    let mdhd = child(mdia, b"mdhd").context("no mdhd")?;
    let timescale = be_u32(mdhd, if mdhd.first() == Some(&1) { 20 } else { 12 })?;
    if timescale == 0 {
        bail!("zero media timescale");
    }
    let stbl = child(mdia, b"minf")
        .and_then(|minf| child(minf, b"stbl"))
        .context("no stbl")?;

    // Sample sizes.
    let stsz = child(stbl, b"stsz").context("no stsz")?;
    let fixed = be_u32(stsz, 4)? as u64;
    let samples = be_u32(stsz, 8)? as usize;
    let size_of = |i: usize| -> Result<u64> {
        if fixed != 0 { Ok(fixed) } else { Ok(be_u32(stsz, 12 + i * 4)? as u64) }
    };

    // Chunk offsets.
    let (chunk_offsets, stride, co) = match (child(stbl, b"stco"), child(stbl, b"co64")) {
        (Some(stco), None) => (table(stco, 4)?.1, 4, stco),
        (None, Some(co64)) => (table(co64, 8)?.1, 8, co64),
        _ => bail!("expected exactly one chunk offset table"),
    };
    let chunk_offset = |c: usize| -> Result<u64> {
        if stride == 8 { be_u64(co, 8 + c * 8) } else { Ok(be_u32(co, 8 + c * 4)? as u64) }
    };

    // Samples per chunk.
    let stsc = child(stbl, b"stsc").context("no stsc")?;
    let (_, runs, start) = table(stsc, 12)?;
    let mut per_chunk = Vec::with_capacity(runs);
    for i in 0..runs {
        let first = be_u32(stsc, start + i * 12)? as usize;
        let count = be_u32(stsc, start + i * 12 + 4)? as usize;
        if first == 0 || per_chunk.last().is_some_and(|&(prev, _)| first <= prev) {
            bail!("invalid stsc");
        }
        per_chunk.push((first, count));
    }

    // Byte range of every sample, in decode order.
    let mut ranges = Vec::with_capacity(samples);
    let mut run = 0usize;
    for c in 0..chunk_offsets {
        while run + 1 < per_chunk.len() && c + 1 >= per_chunk[run + 1].0 {
            run += 1;
        }
        let Some(&(_, in_chunk)) = per_chunk.get(run) else { bail!("chunk without stsc entry") };
        let mut offset = chunk_offset(c)?;
        for _ in 0..in_chunk {
            if ranges.len() == samples {
                bail!("chunks hold more samples than stsz lists");
            }
            let size = size_of(ranges.len())?;
            ranges.push((offset, size));
            offset += size;
        }
    }
    if ranges.len() != samples {
        bail!("chunks hold {} of {} samples", ranges.len(), samples);
    }

    // Presentation times.
    let deltas = expand_runs(child(stbl, b"stts").context("no stts")?, samples, false)?;
    let composition = match child(stbl, b"ctts") {
        Some(ctts) => expand_runs(ctts, samples, true)?,
        None => vec![0; samples],
    };
    let start_time = edit_start(trak)?;
    let mut decode_time = 0i64;
    let mut spans = Vec::with_capacity(samples);
    for (i, &(offset, size)) in ranges.iter().enumerate() {
        let shown = decode_time + composition[i] - start_time;
        spans.push(Span {
            offset,
            size,
            time_sec: (shown.max(0) as f64) / timescale as f64,
        });
        decode_time = decode_time.checked_add(deltas[i]).context("sample time overflow")?;
    }
    spans.sort_by_key(|s| s.offset);
    Ok(VideoSampleTimes { spans })
}

/// Media time the presentation starts at (the first non-empty edit), so
/// times line up with what a player shows as 0. Zero without an edit list.
fn edit_start(trak: &[u8]) -> Result<i64> {
    let Some(elst) = child(trak, b"edts").and_then(|edts| child(edts, b"elst")) else {
        return Ok(0);
    };
    let version = *elst.first().context("empty elst")?;
    let entry = if version == 1 { 20 } else { 12 };
    let (_, count, start) = table(elst, entry)?;
    for i in 0..count {
        let at = start + i * entry;
        let media_time = if version == 1 {
            be_u64(elst, at + 8)? as i64
        } else {
            be_u32(elst, at + 4)? as i32 as i64
        };
        if media_time >= 0 {
            return Ok(media_time);
        }
    }
    Ok(0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn full_box(kind: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
        let mut payload = vec![version, 0, 0, 0];
        payload.extend_from_slice(body);
        mp4_box(kind, &payload)
    }

    fn table_box(kind: &[u8; 4], version: u8, entries: &[&[u32]]) -> Vec<u8> {
        let mut body = (entries.len() as u32).to_be_bytes().to_vec();
        for entry in entries {
            for value in *entry {
                body.extend_from_slice(&value.to_be_bytes());
            }
        }
        full_box(kind, version, &body)
    }

    /// A video `moov` for samples of `sizes` bytes laid out back to back
    /// from `first_offset` in one chunk, `delta` ticks apart at
    /// `timescale`, with optional ctts runs and edit-list start.
    pub(crate) fn video_moov(
        first_offset: u32,
        sizes: &[u32],
        timescale: u32,
        delta: u32,
        ctts: Option<&[&[u32]]>,
        edit_media_time: Option<i32>,
    ) -> Vec<u8> {
        let mut stsz = vec![0u32, sizes.len() as u32];
        stsz.extend_from_slice(sizes);
        let stsz = full_box(b"stsz", 0, &stsz.iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<_>>());
        let mut stbl = Vec::new();
        stbl.extend(table_box(b"stts", 0, &[&[sizes.len() as u32, delta]]));
        if let Some(runs) = ctts {
            stbl.extend(table_box(b"ctts", 0, runs));
        }
        stbl.extend(table_box(b"stsc", 0, &[&[1, sizes.len() as u32, 1]]));
        stbl.extend(stsz);
        stbl.extend(table_box(b"stco", 0, &[&[first_offset]]));
        let minf = mp4_box(b"minf", &mp4_box(b"stbl", &stbl));
        let mut mdhd = vec![0u8; 8];
        mdhd.extend_from_slice(&timescale.to_be_bytes());
        mdhd.extend_from_slice(&[0; 8]);
        let mut hdlr = vec![0u8; 4];
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0; 12]);
        let mut mdia = full_box(b"mdhd", 0, &mdhd);
        mdia.extend(full_box(b"hdlr", 0, &hdlr));
        mdia.extend(minf);
        let mut trak = Vec::new();
        if let Some(media_time) = edit_media_time {
            let mut elst = 1u32.to_be_bytes().to_vec();
            elst.extend_from_slice(&0u32.to_be_bytes());
            elst.extend_from_slice(&media_time.to_be_bytes());
            elst.extend_from_slice(&0x0001_0000u32.to_be_bytes());
            trak.extend(mp4_box(b"edts", &full_box(b"elst", 0, &elst)));
        }
        trak.extend(mp4_box(b"mdia", &mdia));
        mp4_box(b"moov", &mp4_box(b"trak", &trak))
    }

    #[test]
    fn maps_byte_offsets_to_presentation_times() {
        let moov = video_moov(100, &[50, 30, 40], 36_000, 1_000, None, None);
        let times = parse_moov(&moov[8..]).unwrap();
        assert_eq!(times.len(), 3);
        assert_eq!(times.time_at_offset(100), Some(0.0));
        assert_eq!(times.time_at_offset(149), Some(0.0));
        assert_eq!(times.time_at_offset(150), Some(1.0 / 36.0));
        assert_eq!(times.time_at_offset(185), Some(2.0 / 36.0));
        assert_eq!(times.time_at_offset(99), None);
        assert_eq!(times.time_at_offset(220), None);
    }

    #[test]
    fn b_frames_use_composition_offsets_and_the_edit_list() {
        // Decode order I P B with the B shown between them: I at 1, P at 3,
        // B at 2 (in ticks of 1/30 s), and the edit list starting at 1.
        let moov = video_moov(0, &[10, 10, 10], 30, 1, Some(&[&[1, 1], &[1, 2], &[1, 0]]), Some(1));
        let times = parse_moov(&moov[8..]).unwrap();
        assert_eq!(times.time_at_offset(0), Some(0.0));
        assert_eq!(times.time_at_offset(10), Some(2.0 / 30.0));
        assert_eq!(times.time_at_offset(20), Some(1.0 / 30.0));
    }

    #[test]
    fn inconsistent_tables_are_rejected() {
        let mut moov = video_moov(0, &[10, 10], 30, 1, None, None);
        // stts covers 2 samples; claim 3 in stsz by corrupting the count.
        let at = moov.windows(4).position(|w| w == b"stsz").unwrap() + 4 + 8;
        moov[at..at + 4].copy_from_slice(&3u32.to_be_bytes());
        assert!(parse_moov(&moov[8..]).is_err());
    }
}
