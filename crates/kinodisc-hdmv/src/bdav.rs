//! Blu-ray/AVCHD transport fix-up for the M2TS files FFmpeg writes.
//!
//! FFmpeg's `-mpegts_m2ts_mode 1` is not a BDAV muxer, and hardware players
//! notice (a PS4 refused every native AVCHD disc as "not compatible"):
//!
//! - the 4-byte arrival time stamp (ATS) before each packet is computed
//!   from a mux rate of 1 bit/s when the stream is VBR, so it jumps around
//!   instead of growing with the transport clock;
//! - there is a DVB Service Description Table (PID 0x11) and no Selection
//!   Information Table (PID 0x1F, listed in the PAT as program 0);
//! - the PMT carries no HDMV video descriptor and no AC-3 descriptor.
//!
//! [`scan_m2ts`] reads the stream once (PAT/PMT, the AC-3 headers, every
//! video key frame for the clip's EP map) and [`fix_m2ts_in_place`] rewrites
//! the ATS from the PCR, the PAT, the PMT and the SDT packets, packet for
//! packet, so the file keeps its size. Both stream the file in 1 MiB blocks.
//! The SIT and the descriptors follow what tsMuxeR (Apache-2.0) writes, the
//! muxer whose AVCHD discs that PS4 played.

use crate::bdmv::{BdmvError, M2TS_PACKET_SIZE, TS_PACKET_SIZE};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// The arrival time clock runs at 27 MHz and the ATS keeps its low 30 bits.
const ATS_MASK: u64 = (1 << 30) - 1;
/// 48 Mbit/s, the TS recording rate the clip info declares: two packets
/// never arrive closer than 192 bytes at that rate (in 27 MHz ticks).
pub const MIN_ATS_GAP: u64 = (M2TS_PACKET_SIZE as u64 * 8 * 27_000_000) / 48_000_000;
const SIT_PID: u16 = 0x001F;
const SDT_PID: u16 = 0x0011;
const BLOCK_PACKETS: usize = 5_461;
/// Selection Information Table as tsMuxeR writes it: a partial transport
/// stream descriptor (peak rate 48 Mbit/s) and service 1. CRC included.
const SIT_SECTION: [u8; 28] = [
    0x7f, 0xf0, 0x19, 0xff, 0xff, 0xc1, 0x00, 0x00, 0xf0, 0x0a, 0x63, 0x08, 0xc1, 0xd4, 0xc0, 0xff,
    0xff, 0xff, 0xff, 0xff, 0x00, 0x01, 0x80, 0x00, 0x03, 0x00, 0x38, 0x6d,
];

/// A video key frame: where it starts (source packet number), its 90 kHz
/// PTS and its size, for the clip-info EP map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpEntry {
    pub pts: u64,
    pub spn: u32,
    pub frame_bytes: u32,
}

/// What an AC-3 stream says about itself in its first sync frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ac3Facts {
    pub pid: u16,
    /// `fscod`: 0 = 48 kHz, 1 = 44.1 kHz, 2 = 32 kHz.
    pub sample_rate_code: u8,
    /// `frmsizecod >> 1`: 6 = 96 kbit/s, 10 = 192 kbit/s, 15 = 448 kbit/s.
    pub bitrate_code: u8,
    pub bsid: u8,
    pub bsmod: u8,
    /// Channel layout: 1 = mono, 2 = stereo, 7 = 3/2 (5.x).
    pub acmod: u8,
}

impl Ac3Facts {
    /// Parses an AC-3 sync frame header (`0x0B77`…).
    pub fn parse(pid: u16, frame: &[u8]) -> Option<Self> {
        if frame.len() < 7 || frame[0] != 0x0B || frame[1] != 0x77 {
            return None;
        }
        let sample_rate_code = frame[4] >> 6;
        let frmsizecod = frame[4] & 0x3F;
        if sample_rate_code == 3 || frmsizecod > 37 {
            return None;
        }
        Some(Self {
            pid,
            sample_rate_code,
            bitrate_code: frmsizecod >> 1,
            bsid: frame[5] >> 3,
            bsmod: frame[5] & 0x07,
            acmod: frame[6] >> 5,
        })
    }

    /// BDMV `audio_presentation_type`: 1 = mono, 3 = stereo, 6 = multichannel.
    pub fn presentation_type(&self) -> u8 {
        match self.acmod {
            1 => 1,
            0 | 2 => 3,
            _ => 6,
        }
    }

    /// BDMV `sampling_frequency`: 1 = 48 kHz (the only rate AC-3 uses on
    /// Blu-ray and AVCHD).
    pub fn sampling_index(&self) -> u8 {
        1
    }

    /// The attribute byte of STN and clip-info coding info.
    pub fn attribute(&self) -> u8 {
        (self.presentation_type() << 4) | self.sampling_index()
    }

    /// ATSC A/52 audio descriptor (tag 0x81), as tsMuxeR writes it.
    fn descriptor(&self) -> [u8; 6] {
        [
            0x81,
            4,
            (self.sample_rate_code << 5) | (self.bsid & 0x1F),
            self.bitrate_code << 2,
            (self.bsmod << 5) | ((self.acmod & 0x0F) << 1),
            0x00,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmtStream {
    pub stream_type: u8,
    pub pid: u16,
    pub es_info: Vec<u8>,
}

/// Everything [`fix_m2ts_in_place`] and the clip info need from one pass.
#[derive(Debug, Clone, Default)]
pub struct M2tsScan {
    pub packets: u32,
    pub transport_stream_id: u16,
    pub program_number: u16,
    pub pmt_pid: u16,
    pub pcr_pid: u16,
    pub program_info: Vec<u8>,
    pub streams: Vec<PmtStream>,
    pub ac3: Vec<Ac3Facts>,
    /// The first video stream (EP map), if any.
    pub video_pid: Option<u16>,
    pub ep: Vec<EpEntry>,
    pub pcr_samples: u32,
    pub first_video_pts: Option<u64>,
    pub last_video_pts: Option<u64>,
}

impl M2tsScan {
    pub fn ac3_for(&self, pid: u16) -> Option<&Ac3Facts> {
        self.ac3.iter().find(|facts| facts.pid == pid)
    }
}

/// What [`fix_m2ts_in_place`] changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FixReport {
    pub packets: u32,
    pub tables_rewritten: u32,
    pub sdt_to_sit: u32,
    /// The most an arrival time had to be pushed past the PCR line to keep
    /// packets 48 Mbit/s apart, in 27 MHz ticks.
    pub max_ats_delay: u64,
}

fn ts_pid(ts: &[u8]) -> u16 {
    (((ts[1] & 0x1F) as u16) << 8) | ts[2] as u16
}

fn payload_offset(ts: &[u8]) -> Option<usize> {
    let control = (ts[3] >> 4) & 0x03;
    if control & 0x01 == 0 {
        return None;
    }
    let offset = if control & 0x02 != 0 {
        5 + ts[4] as usize
    } else {
        4
    };
    (offset < TS_PACKET_SIZE).then_some(offset)
}

fn random_access(ts: &[u8]) -> bool {
    (ts[3] >> 4) & 0x02 != 0 && ts[4] > 0 && ts[5] & 0x40 != 0
}

/// 27 MHz PCR of a packet that carries one.
fn pcr_of(ts: &[u8]) -> Option<u64> {
    if (ts[3] >> 4) & 0x02 == 0 || ts[4] < 7 || ts[5] & 0x10 == 0 {
        return None;
    }
    let b = &ts[6..12];
    let base = ((b[0] as u64) << 25)
        | ((b[1] as u64) << 17)
        | ((b[2] as u64) << 9)
        | ((b[3] as u64) << 1)
        | ((b[4] as u64) >> 7);
    let extension = (((b[4] & 0x01) as u64) << 8) | b[5] as u64;
    Some(base * 300 + extension)
}

/// PTS of a PES header at the start of `payload`, if it has one.
fn pes_pts(payload: &[u8]) -> Option<u64> {
    if payload.len() < 14 || payload[..3] != [0, 0, 1] || payload[7] & 0x80 == 0 {
        return None;
    }
    let p = &payload[9..14];
    Some(
        (((p[0] >> 1) & 0x07) as u64) << 30
            | (p[1] as u64) << 22
            | ((p[2] >> 1) as u64) << 15
            | (p[3] as u64) << 7
            | (p[4] >> 1) as u64,
    )
}

/// Elementary data after the PES header at the start of `payload`.
fn pes_body(payload: &[u8]) -> Option<&[u8]> {
    if payload.len() < 9 || payload[..3] != [0, 0, 1] {
        return None;
    }
    payload.get(9 + payload[8] as usize..)
}

fn section_at(ts: &[u8]) -> Option<&[u8]> {
    if ts[1] & 0x40 == 0 {
        return None;
    }
    let start = payload_offset(ts)?;
    let pointer = ts[start] as usize;
    let section = ts.get(start + 1 + pointer..)?;
    if section.len() < 3 {
        return None;
    }
    let length = (((section[1] & 0x0F) as usize) << 8) | section[2] as usize;
    section.get(..3 + length)
}

/// Reads `path` once: tables, AC-3 headers and the video key frames.
pub fn scan_m2ts(path: &Path) -> Result<M2tsScan, BdmvError> {
    let length = std::fs::metadata(path)?.len();
    if length == 0 || length % M2TS_PACKET_SIZE as u64 != 0 {
        return Err(BdmvError::InvalidM2ts);
    }
    let packets = u32::try_from(length / M2TS_PACKET_SIZE as u64)
        .map_err(|_| BdmvError::PacketCountOverflow)?;
    let mut scan = M2tsScan {
        packets,
        ..M2tsScan::default()
    };
    let mut have_pat = false;
    let mut have_pmt = false;
    let mut pending: Option<EpEntry> = None;
    // FFmpeg flags key frames with random_access_indicator. Until a flag
    // shows up, an access unit that starts with an SPS or an IDR slice
    // counts as a key frame (those entries come first, in order).
    let mut saw_random_access = false;
    let mut ac3_pids: Vec<u16> = Vec::new();
    let mut file = File::open(path)?;
    let mut block = vec![0u8; BLOCK_PACKETS * M2TS_PACKET_SIZE];
    let mut spn = 0u32;
    loop {
        let read = read_full(&mut file, &mut block)?;
        if read == 0 {
            break;
        }
        for packet in block[..read].as_chunks::<M2TS_PACKET_SIZE>().0 {
            let ts = &packet[4..];
            if ts[0] != 0x47 {
                return Err(BdmvError::InvalidM2ts);
            }
            let pid = ts_pid(ts);
            if pid == scan.pcr_pid && have_pmt && pcr_of(ts).is_some() {
                scan.pcr_samples += 1;
            }
            if pid == 0 && !have_pat {
                if let Some(section) = section_at(ts) {
                    if section[0] == 0x00 && section.len() >= 12 {
                        scan.transport_stream_id = u16::from_be_bytes([section[3], section[4]]);
                        let entries = &section[8..section.len() - 4];
                        for entry in entries.as_chunks::<4>().0 {
                            let program = u16::from_be_bytes([entry[0], entry[1]]);
                            if program != 0 {
                                scan.program_number = program;
                                scan.pmt_pid = (((entry[2] & 0x1F) as u16) << 8) | entry[3] as u16;
                                have_pat = true;
                                break;
                            }
                        }
                    }
                }
            } else if have_pat && !have_pmt && pid == scan.pmt_pid {
                if let Some(section) = section_at(ts) {
                    if section[0] == 0x02 && section.len() >= 16 {
                        scan.pcr_pid = (((section[8] & 0x1F) as u16) << 8) | section[9] as u16;
                        let info = (((section[10] & 0x0F) as usize) << 8) | section[11] as usize;
                        let end = section.len() - 4;
                        scan.program_info = section.get(12..12 + info).unwrap_or(&[]).to_vec();
                        let mut offset = 12 + info;
                        while offset + 5 <= end {
                            let stream_type = section[offset];
                            let stream_pid = (((section[offset + 1] & 0x1F) as u16) << 8)
                                | section[offset + 2] as u16;
                            let es = (((section[offset + 3] & 0x0F) as usize) << 8)
                                | section[offset + 4] as usize;
                            let es_info = section
                                .get(offset + 5..(offset + 5 + es).min(end))
                                .unwrap_or(&[])
                                .to_vec();
                            if matches!(stream_type, 0x1B | 0x02 | 0x24 | 0xEA)
                                && scan.video_pid.is_none()
                            {
                                scan.video_pid = Some(stream_pid);
                            }
                            if stream_type == 0x81 {
                                ac3_pids.push(stream_pid);
                            }
                            scan.streams.push(PmtStream {
                                stream_type,
                                pid: stream_pid,
                                es_info,
                            });
                            offset += 5 + es;
                        }
                        have_pmt = true;
                    }
                }
            } else if Some(pid) == scan.video_pid {
                if let Some(start) = payload_offset(ts) {
                    let payload = &ts[start..];
                    if ts[1] & 0x40 != 0 {
                        if let Some(entry) = pending.take() {
                            scan.ep.push(entry);
                        }
                        if let Some(pts) = pes_pts(payload) {
                            scan.first_video_pts =
                                Some(scan.first_video_pts.map_or(pts, |first| first.min(pts)));
                            scan.last_video_pts =
                                Some(scan.last_video_pts.map_or(pts, |last| last.max(pts)));
                            let flagged = random_access(ts);
                            saw_random_access |= flagged;
                            let headed = pes_body(payload).is_some_and(starts_key_frame);
                            if flagged || (headed && !saw_random_access) {
                                pending = Some(EpEntry {
                                    pts,
                                    spn,
                                    frame_bytes: 0,
                                });
                            }
                        }
                    }
                    if let Some(entry) = pending.as_mut() {
                        entry.frame_bytes = entry.frame_bytes.saturating_add(payload.len() as u32);
                    }
                }
            } else if ts[1] & 0x40 != 0 && ac3_pids.contains(&pid) && scan.ac3_for(pid).is_none() {
                if let Some(body) = payload_offset(ts).and_then(|start| pes_body(&ts[start..])) {
                    if let Some(sync) = body.windows(2).position(|pair| pair == [0x0B, 0x77]) {
                        if let Some(facts) = Ac3Facts::parse(pid, &body[sync..]) {
                            scan.ac3.push(facts);
                        }
                    }
                }
            }
            spn = spn.saturating_add(1);
        }
    }
    if let Some(entry) = pending.take() {
        scan.ep.push(entry);
    }
    if !have_pmt {
        return Err(BdmvError::MissingPmt);
    }
    Ok(scan)
}

/// Whether an H.264 access unit begins a key frame: its first NAL units
/// (before any slice) include an SPS (7) or it is an IDR slice (5).
fn starts_key_frame(body: &[u8]) -> bool {
    let mut index = 0usize;
    while index + 3 < body.len().min(256) {
        if body[index] == 0 && body[index + 1] == 0 && body[index + 2] == 1 {
            match body[index + 3] & 0x1F {
                5 | 7 => return true,
                1 => return false,
                _ => {}
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    false
}

fn read_full(file: &mut File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled - filled % M2TS_PACKET_SIZE)
}

/// The PCR line, read ahead with its own file handle: (packet, 27 MHz).
struct PcrCursor {
    file: File,
    block: Vec<u8>,
    filled: usize,
    position: usize,
    spn: u32,
    pcr_pid: u16,
}

impl PcrCursor {
    fn open(path: &Path, pcr_pid: u16) -> Result<Self, BdmvError> {
        Ok(Self {
            file: File::open(path)?,
            block: vec![0u8; BLOCK_PACKETS * M2TS_PACKET_SIZE],
            filled: 0,
            position: 0,
            spn: 0,
            pcr_pid,
        })
    }

    fn next_sample(&mut self) -> Result<Option<(u32, u64)>, BdmvError> {
        loop {
            if self.position >= self.filled {
                self.filled = read_full(&mut self.file, &mut self.block)?;
                self.position = 0;
                if self.filled == 0 {
                    return Ok(None);
                }
            }
            let packet = &self.block[self.position..self.position + M2TS_PACKET_SIZE];
            self.position += M2TS_PACKET_SIZE;
            let spn = self.spn;
            self.spn = self.spn.saturating_add(1);
            let ts = &packet[4..];
            if ts_pid(ts) == self.pcr_pid {
                if let Some(pcr) = pcr_of(ts) {
                    return Ok(Some((spn, pcr)));
                }
            }
        }
    }
}

fn section_packet(
    pid: u16,
    continuity: u8,
    section: &[u8],
) -> Result<[u8; TS_PACKET_SIZE], BdmvError> {
    if section.len() + 5 > TS_PACKET_SIZE {
        return Err(BdmvError::FragmentedPmt);
    }
    let mut packet = [0xFFu8; TS_PACKET_SIZE];
    packet[0] = 0x47;
    packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
    packet[2] = (pid & 0xFF) as u8;
    packet[3] = 0x10 | (continuity & 0x0F);
    packet[4] = 0x00;
    packet[5..5 + section.len()].copy_from_slice(section);
    Ok(packet)
}

fn finish_section(mut section: Vec<u8>) -> Vec<u8> {
    let length = section.len() - 3 + 4;
    section[1] = 0xB0 | ((length >> 8) as u8 & 0x0F);
    section[2] = (length & 0xFF) as u8;
    let crc = crate::bdmv::mpeg_crc32(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

/// PAT with program 0 pointing at the SIT, then the program's PMT.
pub fn pat_section(scan: &M2tsScan) -> Vec<u8> {
    let mut section = vec![0x00, 0, 0];
    section.extend_from_slice(&scan.transport_stream_id.to_be_bytes());
    section.extend_from_slice(&[0xC1, 0x00, 0x00]);
    section.extend_from_slice(&[
        0x00,
        0x00,
        0xE0 | (SIT_PID >> 8) as u8,
        (SIT_PID & 0xFF) as u8,
    ]);
    section.extend_from_slice(&scan.program_number.to_be_bytes());
    section.extend_from_slice(&[
        0xE0 | (scan.pmt_pid >> 8) as u8,
        (scan.pmt_pid & 0xFF) as u8,
    ]);
    finish_section(section)
}

/// PMT with the HDMV video descriptor and the AC-3 descriptor; other
/// streams keep their descriptors.
pub fn pmt_section(scan: &M2tsScan, video_attr: u8, video_aspect: u8) -> Vec<u8> {
    let mut section = vec![0x02, 0, 0];
    section.extend_from_slice(&scan.program_number.to_be_bytes());
    section.extend_from_slice(&[0xC1, 0x00, 0x00]);
    section.extend_from_slice(&[
        0xE0 | (scan.pcr_pid >> 8) as u8,
        (scan.pcr_pid & 0xFF) as u8,
    ]);
    let info = scan.program_info.len() as u16;
    section.extend_from_slice(&[0xF0 | (info >> 8) as u8 & 0x0F, (info & 0xFF) as u8]);
    section.extend_from_slice(&scan.program_info);
    for stream in &scan.streams {
        let es_info: Vec<u8> = match stream.stream_type {
            0x1B => vec![
                0x05,
                0x08,
                b'H',
                b'D',
                b'M',
                b'V',
                0xFF,
                0x1B,
                video_attr,
                (video_aspect << 4) | 0x0F,
            ],
            0x81 => match scan.ac3_for(stream.pid) {
                Some(facts) => {
                    let mut info = vec![0x05, 0x04, b'A', b'C', b'-', b'3'];
                    info.extend_from_slice(&facts.descriptor());
                    info
                }
                None => stream.es_info.clone(),
            },
            _ => stream.es_info.clone(),
        };
        section.push(stream.stream_type);
        section.extend_from_slice(&[0xE0 | (stream.pid >> 8) as u8, (stream.pid & 0xFF) as u8]);
        let length = es_info.len() as u16;
        section.extend_from_slice(&[0xF0 | (length >> 8) as u8 & 0x0F, (length & 0xFF) as u8]);
        section.extend_from_slice(&es_info);
    }
    finish_section(section)
}

/// Arrival time for packet `spn` on the line through two PCR samples.
fn line_at(spn: u32, a: (u32, u64), b: (u32, u64)) -> u64 {
    if b.0 == a.0 {
        return a.1;
    }
    let slope_num = b.1 as i128 - a.1 as i128;
    let slope_den = b.0 as i128 - a.0 as i128;
    let value = a.1 as i128 + slope_num * (spn as i128 - a.0 as i128) / slope_den;
    value.max(0) as u64
}

/// Rewrites the ATS, PAT, PMT and SDT packets of `path` in place (see the
/// module notes). The file keeps its size and every packet stays where it
/// was, so `scan` (and the EP map in it) stays valid.
pub fn fix_m2ts_in_place(
    path: &Path,
    scan: &M2tsScan,
    video_attr: u8,
    video_aspect: u8,
) -> Result<FixReport, BdmvError> {
    let pat = pat_section(scan);
    let pmt = pmt_section(scan, video_attr, video_aspect);
    section_packet(scan.pmt_pid, 0, &pmt)?;
    let mut cursor = PcrCursor::open(path, scan.pcr_pid)?;
    let first = cursor.next_sample()?;
    let mut previous = first;
    let mut next = match first {
        Some(_) => cursor.next_sample()?,
        None => None,
    };
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut block = vec![0u8; BLOCK_PACKETS * M2TS_PACKET_SIZE];
    let mut report = FixReport::default();
    let mut spn = 0u32;
    let mut last_ats: Option<u64> = None;
    let mut sit_continuity = 0u8;
    let mut position = 0u64;
    loop {
        file.seek(SeekFrom::Start(position))?;
        let read = read_full(&mut file, &mut block)?;
        if read == 0 {
            break;
        }
        for packet in block[..read].as_chunks_mut::<M2TS_PACKET_SIZE>().0 {
            // Move the PCR window so that `previous` <= spn < `next`.
            while let Some(sample) = next {
                if sample.0 > spn {
                    break;
                }
                previous = Some(sample);
                next = cursor.next_sample()?;
            }
            let line = match (previous, next) {
                // Between two PCRs, or before the first one (its interval
                // extended back).
                (Some(a), Some(b)) => line_at(spn, a, b),
                // After the last PCR: keep the last interval's pace.
                (Some(a), None) => a.1 + (spn.saturating_sub(a.0) as u64) * MIN_ATS_GAP,
                (None, _) => spn as u64 * MIN_ATS_GAP,
            };
            let ats = match last_ats {
                Some(last) => line.max(last + MIN_ATS_GAP),
                None => line,
            };
            report.max_ats_delay = report.max_ats_delay.max(ats - line);
            last_ats = Some(ats);
            packet[..4].copy_from_slice(&((ats & ATS_MASK) as u32).to_be_bytes());

            let ts = &mut packet[4..];
            let pid = ts_pid(ts);
            let start = ts[1] & 0x40 != 0;
            let continuity = ts[3] & 0x0F;
            let replacement = if start && pid == 0 {
                Some(section_packet(0, continuity, &pat)?)
            } else if start && pid == scan.pmt_pid {
                Some(section_packet(scan.pmt_pid, continuity, &pmt)?)
            } else if pid == SDT_PID {
                report.sdt_to_sit += 1;
                let packet = section_packet(SIT_PID, sit_continuity, &SIT_SECTION)?;
                sit_continuity = (sit_continuity + 1) & 0x0F;
                Some(packet)
            } else {
                None
            };
            if let Some(replacement) = replacement {
                ts.copy_from_slice(&replacement);
                report.tables_rewritten += 1;
            }
            spn = spn.saturating_add(1);
        }
        file.seek(SeekFrom::Start(position))?;
        file.write_all(&block[..read])?;
        position += read as u64;
    }
    file.flush()?;
    report.packets = spn;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sit_section_crc_is_the_one_tsmuxer_writes() {
        let body = &SIT_SECTION[..SIT_SECTION.len() - 4];
        assert_eq!(
            crate::bdmv::mpeg_crc32(body).to_be_bytes(),
            SIT_SECTION[SIT_SECTION.len() - 4..]
        );
    }

    #[test]
    fn ac3_headers_give_the_layout_and_the_descriptor() {
        // 48 kHz, frmsizecod 12 (96 kbit/s), bsid 8, bsmod 0, acmod 2.
        let stereo = Ac3Facts::parse(0x1100, &[0x0B, 0x77, 0, 0, 0x0C, 0x40, 0x40]).unwrap();
        assert_eq!(stereo.attribute(), 0x31);
        assert_eq!(stereo.descriptor(), [0x81, 4, 0x08, 0x18, 0x04, 0x00]);
        // 448 kbit/s 3/2.
        let surround = Ac3Facts::parse(0x1100, &[0x0B, 0x77, 0, 0, 0x1E, 0x40, 0xE0]).unwrap();
        assert_eq!(surround.attribute(), 0x61);
        assert_eq!(surround.bitrate_code, 15);
        let mono = Ac3Facts::parse(0x1100, &[0x0B, 0x77, 0, 0, 0x0C, 0x40, 0x20]).unwrap();
        assert_eq!(mono.attribute(), 0x11);
        assert!(Ac3Facts::parse(0x1100, &[0x0B, 0x78, 0, 0, 0, 0, 0]).is_none());
    }

    fn scan_like_tsmuxer() -> M2tsScan {
        M2tsScan {
            transport_stream_id: 0,
            program_number: 1,
            pmt_pid: 0x0100,
            pcr_pid: 0x1001,
            program_info: vec![
                0x05, 0x04, b'H', b'D', b'M', b'V', 0x88, 0x04, 0x0f, 0xff, 0xfc, 0xfc,
            ],
            streams: vec![
                PmtStream {
                    stream_type: 0x1B,
                    pid: 0x1011,
                    es_info: Vec::new(),
                },
                PmtStream {
                    stream_type: 0x81,
                    pid: 0x1100,
                    es_info: vec![0x05, 0x04, b'A', b'C', b'-', b'3'],
                },
            ],
            ac3: vec![Ac3Facts::parse(0x1100, &[0x0B, 0x77, 0, 0, 0x0C, 0x40, 0x40]).unwrap()],
            ..M2tsScan::default()
        }
    }

    #[test]
    fn pat_and_pmt_match_the_tables_of_a_disc_the_ps4_plays() {
        // Sections copied from a tsMuxeR 2.7.0 AVCHD disc (480i, AC-3 96 kbit/s).
        let scan = scan_like_tsmuxer();
        assert_eq!(
            pat_section(&scan),
            [
                0x00, 0xb0, 0x11, 0x00, 0x00, 0xc1, 0x00, 0x00, 0x00, 0x00, 0xe0, 0x1f, 0x00, 0x01,
                0xe1, 0x00, 0x23, 0x5a, 0xab, 0x82
            ]
        );
        assert_eq!(
            pmt_section(&scan, 0x14, 3),
            [
                0x02, 0xb0, 0x39, 0x00, 0x01, 0xc1, 0x00, 0x00, 0xf0, 0x01, 0xf0, 0x0c, 0x05, 0x04,
                0x48, 0x44, 0x4d, 0x56, 0x88, 0x04, 0x0f, 0xff, 0xfc, 0xfc, 0x1b, 0xf0, 0x11, 0xf0,
                0x0a, 0x05, 0x08, 0x48, 0x44, 0x4d, 0x56, 0xff, 0x1b, 0x14, 0x3f, 0x81, 0xf1, 0x00,
                0xf0, 0x0c, 0x05, 0x04, 0x41, 0x43, 0x2d, 0x33, 0x81, 0x04, 0x08, 0x18, 0x04, 0x00,
                0x7f, 0xab, 0xc4, 0xd0
            ]
        );
    }

    #[test]
    fn arrival_times_follow_the_pcr_and_never_bunch_up() {
        assert_eq!(line_at(5, (0, 1000), (10, 2000)), 1500);
        assert_eq!(line_at(0, (2, 1000), (4, 2000)), 0);
        assert_eq!(MIN_ATS_GAP, 864);
    }
}
