//! Native Blu-ray (BDMV) navigation structures.
//!
//! These builders emit the canonical on-disc layouts used by tsMuxeR for a
//! single-movie Blu-ray folder. They are deliberately independent from the
//! in-memory tsMuxeR patch helpers so the native FFmpeg fallback can produce a
//! complete BDMV tree without reading media payloads.

use crate::bdmv::{
    BdmvError, M2TS_PACKET_SIZE, PMT_STREAM_TYPE_IG, PMT_STREAM_TYPE_PGS, TS_PACKET_SIZE,
};
use crate::opcodes::{play_playlist_mark, HdmvCommand};

/// BDAV clips start at 600 seconds. Playlist and mark times are 45 kHz.
pub const BDAV_START_45K: u32 = 27_000_000;
pub const CLPI_HEADER_LEN: usize = 56;
pub const MPLS_HEADER_LEN: usize = 58;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeStream {
    pub pid: u16,
    pub stream_type: u8,
    pub language: Option<String>,
    /// BDMV video attribute (`video_format << 4 | frame_rate`); only used for
    /// video streams. Defaults to 1080p 23.976 (`0x61`).
    pub video_attr: u8,
    /// Clip-info aspect ratio code (2 = 4:3, 3 = 16:9), video streams only.
    pub video_aspect: u8,
    /// Audio attribute (`presentation_type << 4 | sampling_frequency`),
    /// audio streams only. Defaults to stereo 48 kHz (`0x31`); the native
    /// muxer reads the real layout from the AC-3 headers.
    pub audio_attr: u8,
}

/// Clip-info aspect ratio code for 16:9 (HD is always 16:9).
pub const ASPECT_16_9: u8 = 3;
/// Clip-info aspect ratio code for 4:3 (480i/576i only).
pub const ASPECT_4_3: u8 = 2;

/// Default BDMV video attribute: 1080p @ 23.976.
pub const DEFAULT_VIDEO_ATTR: u8 = 0x61;
/// Default BDMV audio attribute: stereo, 48 kHz.
pub const DEFAULT_AUDIO_ATTR: u8 = 0x31;

impl NativeStream {
    pub const fn video(pid: u16) -> Self {
        Self {
            pid,
            stream_type: 0x1B,
            language: None,
            video_attr: DEFAULT_VIDEO_ATTR,
            video_aspect: ASPECT_16_9,
            audio_attr: DEFAULT_AUDIO_ATTR,
        }
    }

    pub const fn audio(pid: u16, language: Option<String>) -> Self {
        Self {
            pid,
            stream_type: 0x81,
            language,
            video_attr: DEFAULT_VIDEO_ATTR,
            video_aspect: ASPECT_16_9,
            audio_attr: DEFAULT_AUDIO_ATTR,
        }
    }

    pub const fn pgs(pid: u16, language: Option<String>) -> Self {
        Self {
            pid,
            stream_type: PMT_STREAM_TYPE_PGS,
            language,
            video_attr: DEFAULT_VIDEO_ATTR,
            video_aspect: ASPECT_16_9,
            audio_attr: DEFAULT_AUDIO_ATTR,
        }
    }

    /// Overrides the BDMV video attribute for a video stream.
    pub const fn with_video_attr(mut self, video_attr: u8) -> Self {
        self.video_attr = video_attr;
        self
    }

    /// Overrides the clip-info aspect ratio code for a video stream.
    pub const fn with_video_aspect(mut self, video_aspect: u8) -> Self {
        self.video_aspect = video_aspect;
        self
    }

    /// Overrides the audio attribute (`presentation_type << 4 | rate`).
    pub const fn with_audio_attr(mut self, audio_attr: u8) -> Self {
        self.audio_attr = audio_attr;
        self
    }

    pub const fn ig(pid: u16, language: Option<String>) -> Self {
        Self {
            pid,
            stream_type: PMT_STREAM_TYPE_IG,
            language,
            video_attr: DEFAULT_VIDEO_ATTR,
            video_aspect: ASPECT_16_9,
            audio_attr: DEFAULT_AUDIO_ATTR,
        }
    }

    fn coding_type(&self) -> u8 {
        match self.stream_type {
            0x02 | 0x1B | 0x24 => 0x1B,
            0x03 | 0x04 | 0x0F | 0x81 | 0x82 | 0x86 => 0x81,
            PMT_STREAM_TYPE_IG => PMT_STREAM_TYPE_IG,
            _ => PMT_STREAM_TYPE_PGS,
        }
    }

    fn language_bytes(&self) -> [u8; 3] {
        let language = self.language.as_deref().unwrap_or("und");
        let bytes = language.as_bytes();
        [
            bytes.first().copied().unwrap_or(b'u'),
            bytes.get(1).copied().unwrap_or(b'n'),
            bytes.get(2).copied().unwrap_or(b'd'),
        ]
    }

    fn attribute_bytes(&self) -> [u8; 4] {
        let language = self.language_bytes();
        match self.coding_type() {
            0x1B => [self.video_attr, 0x00, 0x00, 0x00],
            0x81 => [self.audio_attr, language[0], language[1], language[2]],
            PMT_STREAM_TYPE_IG => [language[0], language[1], language[2], 0x00],
            _ => [language[0], language[1], language[2], 0x00],
        }
    }
}

#[derive(Debug, Clone)]
pub struct NativePlaylist {
    pub clip_id: u16,
    pub in_time: u32,
    pub out_time: u32,
    pub streams: Vec<NativeStream>,
    /// 45 kHz timestamps. Empty for menu playlists.
    pub marks: Vec<u32>,
    pub still_time: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct NativeClip {
    pub clip_id: u16,
    pub source_packets: u32,
    pub streams: Vec<NativeStream>,
    /// Transport facts from [`crate::bdav::scan_m2ts`]: PCR, presentation
    /// times and the key frames of the EP map. `None` writes an empty CPI.
    pub transport: Option<ClipTransport>,
}

/// What the clip info needs from the transport stream itself.
#[derive(Debug, Clone, Default)]
pub struct ClipTransport {
    pub pmt_pid: u16,
    pub pcr_pid: u16,
    /// 45 kHz presentation start and end (the playlist IN/OUT times).
    pub presentation_start: u32,
    pub presentation_end: u32,
    /// The stream the EP map indexes (the video) and its key frames.
    pub ep_pid: u16,
    pub ep: Vec<crate::bdav::EpEntry>,
}

impl ClipTransport {
    pub fn from_scan(scan: &crate::bdav::M2tsScan, in_time: u32, out_time: u32) -> Self {
        Self {
            pmt_pid: scan.pmt_pid,
            pcr_pid: scan.pcr_pid,
            presentation_start: in_time,
            presentation_end: out_time,
            ep_pid: scan.video_pid.unwrap_or(0x1011),
            ep: scan.ep.clone(),
        }
    }
}

/// Big-endian bit writer for the EP map's packed fields.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bits: u32,
}

impl BitWriter {
    fn put(&mut self, width: u32, value: u64) {
        for bit in (0..width).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (value >> bit) & 1 == 1 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }

    fn align16(&mut self) {
        while !self.bits.is_multiple_of(16) {
            self.put(8, 0);
        }
    }
}

fn put16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn clip_id_bytes(clip_id: u16) -> [u8; 5] {
    let mut bytes = [b'0'; 5];
    let text = format!("{clip_id:05}");
    bytes.copy_from_slice(&text.as_bytes()[..5]);
    bytes
}

/// Builds a 120-byte INDX0200 first-play/top-menu index. AppInfoBDMV is 34
/// bytes (it said 0 before 1.2.6); First Playback and Top Menu are
/// interactive HDMV objects and the title a movie object, as tsMuxeR writes.
pub fn build_index_bdmv(first_play: u16, top_menu: u16) -> Vec<u8> {
    let mut index = vec![0u8; 120];
    index[..8].copy_from_slice(b"INDX0200");
    put32(&mut index, 8, 78);
    put32(&mut index, 40, 34);
    put32(&mut index, 78, 38);
    let first = 82usize;
    index[first] = 0x40;
    index[first + 4] = 0x40;
    put16(&mut index, first + 6, first_play);
    let top = 94usize;
    index[top] = 0x40;
    index[top + 4] = 0x40;
    put16(&mut index, top + 6, top_menu);
    put16(&mut index, 106, 1);
    let title = 108usize;
    index[title] = 0x40;
    put16(&mut index, title + 6, 0);
    index
}

/// Builds a MovieObject.bdmv with a single 12-byte-command object.
pub fn build_movie_object(commands: &[HdmvCommand]) -> Result<Vec<u8>, BdmvError> {
    let command_count = u16::try_from(commands.len())
        .map_err(|_| BdmvError::InvalidMovieObject("too many commands"))?;
    let mut movie_object = vec![0u8; 50];
    movie_object[..8].copy_from_slice(b"MOBJ0200");
    let data_len = 6u32
        .checked_add(4)
        .and_then(|value| value.checked_add(u32::from(command_count) * 12))
        .ok_or(BdmvError::InvalidMovieObject("data length overflow"))?;
    put32(&mut movie_object, 40, data_len);
    put16(&mut movie_object, 48, 1);
    movie_object.extend_from_slice(&0x8000u16.to_be_bytes());
    movie_object.extend_from_slice(&command_count.to_be_bytes());
    for command in commands {
        movie_object.extend_from_slice(&command.to_bytes());
    }
    Ok(movie_object)
}

/// A one-object movie table whose object starts at playmark GPR0. Chapter
/// buttons set GPR0 and jump to title 1, so PlayPL_PM preserves seeking.
pub fn build_movie_object_with_gpr0_playmark(playlist: u16) -> Result<Vec<u8>, BdmvError> {
    build_movie_object(&[play_playlist_mark(u32::from(playlist), 0)])
}

fn build_stn(streams: &[NativeStream]) -> Result<Vec<u8>, BdmvError> {
    let mut video = 0u8;
    let mut audio = 0u8;
    let mut pg = 0u8;
    let mut ig = 0u8;
    for stream in streams {
        match stream.coding_type() {
            0x1B => video = video.saturating_add(1),
            0x81 => audio = audio.saturating_add(1),
            PMT_STREAM_TYPE_IG => ig = ig.saturating_add(1),
            _ => pg = pg.saturating_add(1),
        }
    }
    let payload_len = 2usize
        .checked_add(4)
        .and_then(|value| value.checked_add(8))
        .and_then(|value| value.checked_add(streams.len().checked_mul(16)?))
        .ok_or(BdmvError::InvalidMpls("STN length overflow"))?;
    let stn_len =
        u16::try_from(payload_len).map_err(|_| BdmvError::InvalidMpls("STN too large"))?;
    let mut stn = Vec::with_capacity(2 + payload_len);
    stn.extend_from_slice(&stn_len.to_be_bytes());
    stn.extend_from_slice(&[0, 0]);
    stn.extend_from_slice(&[video, audio, pg, ig]);
    stn.extend_from_slice(&[0; 8]);
    for stream in streams {
        stn.push(9); // stream entry length
        stn.push(0x01);
        stn.extend_from_slice(&stream.pid.to_be_bytes());
        stn.extend_from_slice(&[0; 6]);
        stn.push(5); // coding-info length
        let attributes = stream.attribute_bytes();
        stn.push(stream.coding_type());
        stn.extend_from_slice(&attributes);
    }
    Ok(stn)
}

/// Builds an MPLS0200 movie playlist with an optional chapter mark table.
pub fn build_playlist_mpls(spec: &NativePlaylist) -> Result<Vec<u8>, BdmvError> {
    if spec.out_time <= spec.in_time {
        return Err(BdmvError::InvalidMpls("OUT_time must follow IN_time"));
    }
    let stn = build_stn(&spec.streams)?;
    let playitem_len = 32usize
        .checked_add(stn.len())
        .ok_or(BdmvError::InvalidMpls("playitem overflow"))?;
    let playitem_len =
        u16::try_from(playitem_len).map_err(|_| BdmvError::InvalidMpls("playitem too large"))?;
    let list_len = 6usize
        .checked_add(2)
        .and_then(|value| value.checked_add(usize::from(playitem_len)))
        .ok_or(BdmvError::InvalidMpls("playlist overflow"))?;
    let list_len =
        u32::try_from(list_len).map_err(|_| BdmvError::InvalidMpls("playlist too large"))?;
    let mark_pos = MPLS_HEADER_LEN
        .checked_add(4)
        .and_then(|value| value.checked_add(usize::try_from(list_len).ok()?))
        .ok_or(BdmvError::InvalidMpls("mark position overflow"))?;

    let mut mpls = vec![0u8; MPLS_HEADER_LEN];
    mpls[..8].copy_from_slice(b"MPLS0200");
    put32(&mut mpls, 8, MPLS_HEADER_LEN as u32);
    // AppInfoPlayList(): 14 bytes, sequential playback, UO mask clear,
    // audio_mix_app_flag set (the length said 0 before 1.2.6).
    put32(&mut mpls, 40, 14);
    mpls[45] = 0x01;
    mpls[56] = 0x40;
    put32(
        &mut mpls,
        12,
        u32::try_from(mark_pos).map_err(|_| BdmvError::InvalidMpls("mark position"))?,
    );
    let mut playlist = Vec::with_capacity(4 + list_len as usize);
    playlist.extend_from_slice(&list_len.to_be_bytes());
    playlist.extend_from_slice(&[0, 0]);
    playlist.extend_from_slice(&1u16.to_be_bytes());
    playlist.extend_from_slice(&0u16.to_be_bytes());
    playlist.extend_from_slice(&playitem_len.to_be_bytes());
    playlist.extend_from_slice(&clip_id_bytes(spec.clip_id));
    playlist.extend_from_slice(b"M2TS");
    playlist.extend_from_slice(&[0, 1, 0]);
    playlist.extend_from_slice(&spec.in_time.to_be_bytes());
    playlist.extend_from_slice(&spec.out_time.to_be_bytes());
    playlist.extend_from_slice(&[0; 9]);
    playlist.push(spec.still_time.map_or(0, |_| 0x01));
    playlist.extend_from_slice(&spec.still_time.unwrap_or(0).to_be_bytes());
    playlist.extend_from_slice(&stn);
    mpls.extend_from_slice(&playlist);

    // A playlist without chapters (menus) still gets one mark at IN_time,
    // as tsMuxeR writes: before 1.2.6 the table was a bare zero length,
    // without its mark count.
    let start_mark = [spec.in_time];
    let marks: &[u32] = if spec.marks.is_empty() {
        &start_mark
    } else {
        &spec.marks
    };
    let count = u16::try_from(marks.len()).map_err(|_| BdmvError::InvalidMpls("too many marks"))?;
    // PlayListMark(): length, count, then 14 bytes per mark: reserved,
    // mark_type (1 = entry mark), ref_to_PlayItem_id, mark_time_stamp,
    // entry_ES_PID (0xFFFF = none) and duration.
    let length = 2u32
        .checked_add(u32::from(count) * 14)
        .ok_or(BdmvError::InvalidMpls("mark table overflow"))?;
    mpls.extend_from_slice(&length.to_be_bytes());
    mpls.extend_from_slice(&count.to_be_bytes());
    for mark in marks {
        mpls.extend_from_slice(&[0x00, 0x01, 0x00, 0x00]);
        mpls.extend_from_slice(&mark.to_be_bytes());
        mpls.extend_from_slice(&[0xff, 0xff, 0, 0, 0, 0]);
    }
    Ok(mpls)
}

/// The 48 Mbit/s TS recording rate, in bytes per second.
const TS_RECORDING_RATE: u32 = 6_000_000;

fn pad4(bytes: &mut Vec<u8>) {
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
}

fn with_length(body: Vec<u8>) -> Result<Vec<u8>, BdmvError> {
    let length =
        u32::try_from(body.len()).map_err(|_| BdmvError::InvalidClpi("block too large"))?;
    let mut block = length.to_be_bytes().to_vec();
    block.extend_from_slice(&body);
    Ok(block)
}

/// ClipInfo(): AV stream, main TS of a movie, 48 Mbit/s, the packet count
/// and the "HDMV" TS type info block.
fn clip_info_block(source_packets: u32) -> Result<Vec<u8>, BdmvError> {
    let mut body = vec![0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00];
    body.extend_from_slice(&TS_RECORDING_RATE.to_be_bytes());
    body.extend_from_slice(&source_packets.to_be_bytes());
    body.extend_from_slice(&[0u8; 128]);
    // TS_type_info_block(): length 30, validity flags, "HDMV", network
    // information (9) and stream format name (16), zero.
    body.extend_from_slice(&[0x00, 0x1E, 0x80, b'H', b'D', b'M', b'V']);
    body.extend_from_slice(&[0u8; 25]);
    with_length(body)
}

/// SequenceInfo(): one ATC and one STC sequence, with the PCR PID and the
/// presentation start/end of the clip.
fn sequence_info_block(transport: Option<&ClipTransport>) -> Result<Vec<u8>, BdmvError> {
    let mut body = vec![0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00];
    let (pcr_pid, start, end) = transport.map_or((0x1001, BDAV_START_45K, BDAV_START_45K), |t| {
        (t.pcr_pid, t.presentation_start, t.presentation_end)
    });
    body.extend_from_slice(&pcr_pid.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&start.to_be_bytes());
    body.extend_from_slice(&end.to_be_bytes());
    with_length(body)
}

/// ProgramInfo(): one program sequence and the coding info of each stream.
fn program_info_block(spec: &NativeClip) -> Result<Vec<u8>, BdmvError> {
    let count =
        u8::try_from(spec.streams.len()).map_err(|_| BdmvError::InvalidClpi("too many streams"))?;
    let pmt_pid = spec.transport.as_ref().map_or(0x0100, |t| t.pmt_pid);
    let mut body = vec![0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
    body.extend_from_slice(&pmt_pid.to_be_bytes());
    body.extend_from_slice(&[count, 0x00]);
    for stream in &spec.streams {
        body.extend_from_slice(&stream.pid.to_be_bytes());
        // StreamCodingInfo(): length 21, coding type, 4 attribute bytes,
        // ISRC (12 × '0') and 4 reserved bytes.
        body.push(0x15);
        body.push(stream.coding_type());
        let mut attributes = stream.attribute_bytes();
        if stream.coding_type() == 0x1B {
            // aspect_ratio (4 bits) after format/rate; tsMuxeR writes 3 for 16:9.
            attributes[1] = stream.video_aspect << 4;
        }
        body.extend_from_slice(&attributes);
        body.extend_from_slice(&[0x30; 12]);
        body.extend_from_slice(&[0; 4]);
    }
    with_length(body)
}

/// `I_end_position_offset` code for a key frame of `bytes` (HD sizes).
fn end_position_code(bytes: u32) -> u64 {
    match bytes {
        0 => 0,
        1..131_072 => 1,
        131_072..262_144 => 2,
        262_144..393_216 => 3,
        393_216..589_824 => 4,
        589_824..917_504 => 5,
        917_504..1_310_720 => 6,
        _ => 7,
    }
}

/// CPI(): EP map type, then one EP_map_for_one_stream_PID for the video,
/// with coarse entries (every 2^19 PTS ticks or 2^17 packets) pointing
/// into one fine entry per key frame, as tsMuxeR builds it.
fn cpi_block(transport: Option<&ClipTransport>) -> Result<Vec<u8>, BdmvError> {
    let Some(transport) = transport.filter(|t| !t.ep.is_empty()) else {
        return Ok(vec![0; 4]);
    };
    let fine_count = u32::try_from(transport.ep.len())
        .ok()
        .filter(|count| *count < 1 << 18)
        .ok_or(BdmvError::InvalidClpi("too many key frames"))?;
    let mut coarse: Vec<(u32, u64, u32)> = Vec::new();
    let mut last: Option<(u64, u32)> = None;
    for (fine_id, entry) in transport.ep.iter().enumerate() {
        let coarse_pts = entry.pts >> 19;
        let coarse_spn = entry.spn & 0xFFFE_0000;
        if last.is_none_or(|(pts, spn)| pts != coarse_pts || spn & 0xFFFE_0000 != coarse_spn) {
            coarse.push((fine_id as u32, coarse_pts, entry.spn));
        }
        last = Some((coarse_pts, entry.spn));
    }
    let coarse_count = u16::try_from(coarse.len())
        .map_err(|_| BdmvError::InvalidClpi("too many coarse entries"))?;

    let mut stream = BitWriter::default();
    stream.put(32, 0); // EP_fine_table_start_address, set below
    for (fine_id, pts, spn) in &coarse {
        stream.put(18, u64::from(*fine_id));
        stream.put(14, pts & 0x3FFF);
        stream.put(32, u64::from(*spn));
    }
    stream.align16();
    let fine_start = stream.bytes.len() as u32;
    stream.bytes[..4].copy_from_slice(&fine_start.to_be_bytes());
    for entry in &transport.ep {
        stream.put(1, 0); // is_angle_change_point
        stream.put(3, end_position_code(entry.frame_bytes));
        stream.put(11, (entry.pts >> 9) & 0x7FF);
        stream.put(17, u64::from(entry.spn) & 0x1FFFF);
    }

    let mut map = BitWriter::default();
    map.put(8, 0);
    map.put(8, 1); // number_of_stream_PID_entries
    map.put(16, u64::from(transport.ep_pid));
    map.put(10, 0);
    map.put(4, 1); // EP_stream_type: video
    map.put(16, u64::from(coarse_count));
    map.put(18, u64::from(fine_count));
    let address_at = map.bytes.len();
    map.put(32, 0); // EP_map_for_one_stream_PID_start_address
    map.align16();
    let start = map.bytes.len() as u32;
    map.bytes[address_at..address_at + 4].copy_from_slice(&start.to_be_bytes());
    map.bytes.extend_from_slice(&stream.bytes);

    let mut body = vec![0x00, 0x01]; // reserved, CPI_type 1 (EP map)
    body.extend_from_slice(&map.bytes);
    with_length(body)
}

/// Builds an HDMV0200 clip-info file in tsMuxeR's layout: ClipInfo,
/// SequenceInfo, ProgramInfo, CPI (EP map) and empty ClipMark and
/// extension data, each 4-byte aligned. Before 1.2.6 the SequenceInfo was
/// empty and the EP map a fixed one-entry block.
pub fn build_clpi(spec: &NativeClip) -> Result<Vec<u8>, BdmvError> {
    let mut clpi = Vec::new();
    clpi.extend_from_slice(b"HDMV0200");
    clpi.extend_from_slice(&[0; 32]); // five addresses + 12 reserved bytes
    let mut addresses = [0u32; 5];
    let blocks = [
        sequence_info_block(spec.transport.as_ref())?,
        program_info_block(spec)?,
        cpi_block(spec.transport.as_ref())?,
        vec![0; 4], // ClipMark()
        vec![0; 4], // extension data
    ];
    clpi.extend_from_slice(&clip_info_block(spec.source_packets)?);
    pad4(&mut clpi);
    for (slot, block) in blocks.iter().enumerate() {
        addresses[slot] =
            u32::try_from(clpi.len()).map_err(|_| BdmvError::InvalidClpi("clip info too large"))?;
        clpi.extend_from_slice(block);
        pad4(&mut clpi);
    }
    for (slot, address) in addresses.iter().enumerate() {
        put32(&mut clpi, 8 + slot * 4, *address);
    }
    Ok(clpi)
}

/// AVCHD index extension ("IDEX"), bytes 0x78..0x210 of the index.bdmv
/// tsMuxeR 2.7.0 writes for AVCHD (Apache-2.0, github.com/justdan96/tsMuxer).
const AVCHD_INDEX_EXTENSION: [u8; 408] = [
    0x00, 0x00, 0x01, 0x92, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x01, 0x10, 0x00, 0x01, 0x00,
    0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x01, 0x7e, 0x49, 0x44, 0x45, 0x58, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x01, 0x62, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x01, 0x36, 0x10, 0x13, 0x00, 0x01, 0x54, 0x52, 0x20, 0x30, 0x2e, 0x30, 0x2e, 0x31,
    0x2e, 0x38, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0xff, 0xff, 0x42, 0x20, 0x07, 0x08,
    0x08, 0x00, 0x54, 0x53, 0x00, 0x90, 0x0a, 0x54, 0x52, 0x20, 0x30, 0x2e, 0x30, 0x2e, 0x31, 0x2e,
    0x38, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00,
    0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x30, 0x30, 0x30, 0x30,
    0x30, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Turns a BDMV navigation file into its AVCHD form (version "0100"); an
/// index also gets the AVCHD extension data. Other bytes are unchanged.
pub fn avchd_navigation_file(bytes: &[u8]) -> Result<Vec<u8>, BdmvError> {
    if bytes.len() < 8 {
        return Err(BdmvError::InvalidIndex("navigation file too short"));
    }
    let mut out = bytes.to_vec();
    match &bytes[..4] {
        b"INDX" => {
            if bytes.len() < 120 {
                return Err(BdmvError::InvalidIndex("index.bdmv too short"));
            }
            out.truncate(120);
            put32(&mut out, 12, 0x78);
            out.extend_from_slice(&AVCHD_INDEX_EXTENSION);
        }
        b"MOBJ" | b"MPLS" | b"HDMV" => {}
        _ => return Err(BdmvError::InvalidIndex("not a BDMV navigation file")),
    }
    out[4..8].copy_from_slice(b"0100");
    Ok(out)
}

fn pid(packet: &[u8]) -> u16 {
    (((packet[1] & 0x1F) as u16) << 8) | packet[2] as u16
}

fn payload_start(packet: &[u8]) -> Option<usize> {
    if packet.len() < 5 || packet[0] != 0x47 || packet[3] & 0x10 == 0 {
        return None;
    }
    if packet[3] & 0x20 != 0 {
        let offset = 5 + packet[4] as usize;
        (offset < TS_PACKET_SIZE).then_some(offset)
    } else {
        Some(4)
    }
}

/// Parses the PAT/PMT stream descriptors from a bounded M2TS prefix. No media
/// payload is read; the caller only needs the first few kilobytes.
pub fn parse_pmt_from_m2ts_prefix(prefix: &[u8]) -> Result<Vec<NativeStream>, BdmvError> {
    let pmt_pid = find_pmt_pid(prefix)?;
    let mut streams = Vec::new();
    for packet in prefix.as_chunks::<M2TS_PACKET_SIZE>().0 {
        let ts = &packet[4..];
        if pid(ts) != pmt_pid || ts[1] & 0x40 == 0 {
            continue;
        }
        let Some(start) = payload_start(ts) else {
            continue;
        };
        let pointer = ts[start] as usize;
        let section_start = start + 1 + pointer;
        if section_start + 12 > TS_PACKET_SIZE {
            return Err(BdmvError::FragmentedPmt);
        }
        let section = &ts[section_start..];
        if section[0] != 0x02 {
            continue;
        }
        let section_length = (((section[1] & 0x0F) as usize) << 8) | section[2] as usize;
        let end = 3 + section_length;
        if end > section.len() || end < 16 {
            return Err(BdmvError::FragmentedPmt);
        }
        let program_info_length = (((section[10] & 0x0F) as usize) << 8) | section[11] as usize;
        let mut offset = 12 + program_info_length;
        while offset + 5 <= end - 4 {
            let stream_type = section[offset];
            let stream_pid =
                (((section[offset + 1] & 0x1F) as u16) << 8) | section[offset + 2] as u16;
            let info_length =
                (((section[offset + 3] & 0x0F) as usize) << 8) | section[offset + 4] as usize;
            let info = section
                .get(offset + 5..offset + 5 + info_length)
                .unwrap_or(&[]);
            let mut language = None;
            let mut cursor = 0usize;
            while cursor + 2 <= info.len() {
                let tag = info[cursor];
                let length = info[cursor + 1] as usize;
                let body = info.get(cursor + 2..cursor + 2 + length).unwrap_or(&[]);
                if tag == 0x0A && body.len() >= 3 {
                    language = Some(String::from_utf8_lossy(&body[..3]).to_string());
                }
                cursor += 2 + length;
            }
            streams.push(NativeStream {
                pid: stream_pid,
                stream_type,
                language,
                video_attr: DEFAULT_VIDEO_ATTR,
                video_aspect: ASPECT_16_9,
                audio_attr: DEFAULT_AUDIO_ATTR,
            });
            offset += 5 + info_length;
        }
        return Ok(streams);
    }
    Err(BdmvError::MissingPmt)
}

fn find_pmt_pid(prefix: &[u8]) -> Result<u16, BdmvError> {
    for packet in prefix.as_chunks::<M2TS_PACKET_SIZE>().0 {
        let ts = &packet[4..];
        if pid(ts) != 0 || ts[1] & 0x40 == 0 {
            continue;
        }
        let Some(start) = payload_start(ts) else {
            continue;
        };
        let pointer = ts[start] as usize;
        let Some(section) = ts.get(start + 1 + pointer..) else {
            continue;
        };
        if section.first().copied() != Some(0x00) || section.len() < 12 {
            continue;
        }
        let section_length = (((section[1] & 0x0F) as usize) << 8) | section[2] as usize;
        let end = 3 + section_length;
        if end > section.len() || end < 12 {
            continue;
        }
        let mut offset = 8usize;
        while offset + 4 <= end - 4 {
            let program = u16::from_be_bytes([section[offset], section[offset + 1]]);
            let program_pid =
                (((section[offset + 2] & 0x1F) as u16) << 8) | section[offset + 3] as u16;
            if program != 0 {
                return Ok(program_pid);
            }
            offset += 4;
        }
    }
    Err(BdmvError::MissingPmt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opcodes::jump_object;

    #[test]
    fn blu_ray_packet_counts_are_checked_against_u32_on_32_bit() {
        let fifty_gib = 50u64 * 1024 * 1024 * 1024;
        let packets = fifty_gib / 192;
        assert!(u32::try_from(packets).is_ok());
        assert!(u32::try_from(u64::from(u32::MAX) + 1).is_err());
        assert!(u32::try_from(fifty_gib).is_err());
    }

    #[test]
    fn index_round_trips_through_the_existing_patch_helper() {
        let index = build_index_bdmv(3, 3);
        assert_eq!(&index[..8], b"INDX0200");
        let patched = crate::bdmv::set_index_menu_object(&index, 7).expect("index patch");
        assert_eq!(u16::from_be_bytes([patched[88], patched[89]]), 7);
        assert_eq!(u16::from_be_bytes([patched[100], patched[101]]), 7);
    }

    #[test]
    fn movie_object_starts_at_gpr0_mark_and_accepts_menu_append() {
        let object = build_movie_object_with_gpr0_playmark(0).expect("movie object");
        assert_eq!(crate::bdmv::movie_object_count(&object).unwrap(), 1);
        let (appended, index) = crate::bdmv::append_menu_movie_object(&object, 1).unwrap();
        assert_eq!(index, 1);
        assert_eq!(crate::bdmv::movie_object_count(&appended).unwrap(), 2);
        let command = jump_object(1);
        assert_eq!(command.to_bytes()[0], 0x21);
    }

    #[test]
    fn playlist_registers_pgs_and_chapter_marks() {
        let spec = NativePlaylist {
            clip_id: 0,
            in_time: BDAV_START_45K,
            out_time: BDAV_START_45K + 90_000,
            streams: vec![
                NativeStream::video(0x1011),
                NativeStream::audio(0x1100, Some("eng".into())),
                NativeStream::pgs(0x1200, Some("spa".into())),
            ],
            marks: vec![
                BDAV_START_45K,
                BDAV_START_45K + 45_000,
                BDAV_START_45K + 90_000,
            ],
            still_time: None,
        };
        let mpls = build_playlist_mpls(&spec).expect("mpls");
        assert_eq!(&mpls[..8], b"MPLS0200");
        assert_eq!(&mpls[70..75], b"00000");
        assert_eq!(&mpls[75..79], b"M2TS");
        let playitem = 58 + 10;
        let stn = playitem + 2 + 32;
        let counts = stn + 4;
        assert_eq!(&mpls[counts..counts + 4], &[1, 1, 1, 0]);
        let pg_entry = stn + 48;
        assert_eq!(
            u16::from_be_bytes([mpls[pg_entry + 2], mpls[pg_entry + 3]]),
            0x1200
        );
        let mark_start = u32::from_be_bytes([mpls[12], mpls[13], mpls[14], mpls[15]]) as usize;
        assert_eq!(
            u32::from_be_bytes(mpls[mark_start..mark_start + 4].try_into().unwrap()),
            44
        );
        assert_eq!(mpls.len(), mark_start + 4 + 44);
        let timeline = crate::episodes::read_mpls_timeline(&mpls).expect("timeline");
        assert_eq!(
            timeline.marks,
            vec![
                BDAV_START_45K,
                BDAV_START_45K + 45_000,
                BDAV_START_45K + 90_000
            ]
        );
    }

    fn be32(bytes: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn clpi_has_canonical_size_stream_blocks_and_source_packet_count() {
        let clip = NativeClip {
            clip_id: 0,
            source_packets: 704,
            streams: vec![
                NativeStream::video(0x1011),
                NativeStream::audio(0x1100, Some("eng".into())),
                NativeStream::pgs(0x1200, Some("spa".into())),
            ],
            transport: None,
        };
        let clpi = build_clpi(&clip).expect("clpi");
        assert_eq!(&clpi[..8], b"HDMV0200");
        // ClipInfo is 176 bytes and declares the packet count.
        assert_eq!(be32(&clpi, 40), 176);
        assert_eq!(be32(&clpi, 56), 704);
        assert_eq!(&clpi[188..195], &[0x00, 0x1E, 0x80, b'H', b'D', b'M', b'V']);
        assert_eq!(be32(&clpi, 8), 0xDC, "SequenceInfo right after ClipInfo");
        assert_eq!(be32(&clpi, 0xDC), 22, "SequenceInfo is never empty");
        let program = be32(&clpi, 12) as usize;
        assert_eq!(program, 0xF8);
        assert_eq!(be32(&clpi, program), 10 + 3 * 24);
        let streams = program + 4 + 10;
        assert_eq!(&clpi[streams..streams + 2], &[0x10, 0x11]);
        assert_eq!(clpi[streams + 4], DEFAULT_VIDEO_ATTR);
        assert_eq!(clpi[streams + 5], 0x30, "video aspect 16:9");
        let audio = streams + 24;
        assert_eq!(
            clpi[audio + 4],
            0x31,
            "audio is stereo 48 kHz, not 'mono, rate 0'"
        );
        let pgs = streams + 48;
        assert_eq!(clpi[pgs + 3], 0x90, "PGS coding type");
        assert_eq!(&clpi[pgs + 4..pgs + 7], b"spa");
        // A 4:3 SD disc declares 4:3; the playlist STN has no aspect field.
        let clip = NativeClip {
            clip_id: 0,
            source_packets: 704,
            streams: vec![NativeStream::video(0x1011)
                .with_video_attr(0x14)
                .with_video_aspect(ASPECT_4_3)],
            transport: None,
        };
        let clpi = build_clpi(&clip).expect("clpi");
        let program = be32(&clpi, 12) as usize;
        assert_eq!(clpi[program + 4 + 10 + 4], 0x14);
        assert_eq!(clpi[program + 4 + 10 + 5], 0x20, "video aspect 4:3");
    }

    /// Reads the key frames back from a clip info's EP map (fine entries
    /// with their full SPN and the PTS bits the map keeps).
    fn ep_entries_of(clpi: &[u8]) -> (u16, Vec<crate::bdav::EpEntry>) {
        let cpi = be32(clpi, 16) as usize;
        let map = cpi + 6;
        let pid = u16::from_be_bytes([clpi[map + 2], clpi[map + 3]]);
        let packed = u64::from_be_bytes([
            0,
            0,
            clpi[map + 4],
            clpi[map + 5],
            clpi[map + 6],
            clpi[map + 7],
            clpi[map + 8],
            clpi[map + 9],
        ]);
        let coarse_count = ((packed >> 18) & 0xFFFF) as usize;
        let fine_count = (packed & 0x3FFFF) as usize;
        let one = map + be32(clpi, map + 10) as usize;
        let fine = one + be32(clpi, one) as usize;
        let coarse: Vec<(usize, u64, u32)> = (0..coarse_count)
            .map(|i| {
                let at = one + 4 + i * 8;
                let word = be32(clpi, at);
                (
                    (word >> 14) as usize,
                    u64::from(word & 0x3FFF),
                    be32(clpi, at + 4),
                )
            })
            .collect();
        let entries = (0..fine_count)
            .map(|i| {
                let word = be32(clpi, fine + i * 4);
                let (_, coarse_pts, coarse_spn) = *coarse
                    .iter()
                    .rev()
                    .find(|(first, _, _)| *first <= i)
                    .unwrap();
                let code = (word >> 28) & 0x7;
                crate::bdav::EpEntry {
                    pts: ((coarse_pts << 10) | u64::from((word >> 17) & 0x7FF)) << 9,
                    spn: (coarse_spn & !0x1FFFF) | (word & 0x1FFFF),
                    frame_bytes: [
                        0, 1_000, 200_000, 300_000, 400_000, 600_000, 1_000_000, 2_000_000,
                    ][code as usize],
                }
            })
            .collect();
        (pid, entries)
    }

    #[test]
    fn clip_info_is_byte_for_byte_the_one_a_ps4_plays() {
        // tsMuxeR 2.7.0 --avchd, 480i29.97 16:9 H.264 + AC-3 2.0 96 kbit/s,
        // 24 s, 30 key frames: the disc a PS4 accepted.
        let reference: &[u8] = include_bytes!("../tests/fixtures/tsmuxer-avchd-480i.clpi");
        let (ep_pid, ep) = ep_entries_of(reference);
        assert_eq!((ep_pid, ep.len()), (0x1011, 30));
        let clip = NativeClip {
            clip_id: 0,
            source_packets: be32(reference, 0x38),
            streams: vec![
                NativeStream::video(0x1011)
                    .with_video_attr(0x14)
                    .with_video_aspect(ASPECT_16_9),
                NativeStream::audio(0x1100, Some("\0\0\0".into())).with_audio_attr(0x31),
            ],
            transport: Some(ClipTransport {
                pmt_pid: 0x0100,
                pcr_pid: 0x1001,
                presentation_start: be32(reference, 0xDC + 18),
                presentation_end: be32(reference, 0xDC + 22),
                ep_pid,
                ep,
            }),
        };
        let clpi = avchd_navigation_file(&build_clpi(&clip).expect("clpi")).expect("avchd");
        assert_eq!(clpi.len(), reference.len());
        for (offset, (ours, theirs)) in clpi.iter().zip(reference).enumerate() {
            assert_eq!(ours, theirs, "byte {offset:#x} differs");
        }
    }

    #[test]
    fn avchd_index_and_navigation_files_match_tsmuxer() {
        let reference: &[u8] = include_bytes!("../tests/fixtures/tsmuxer-avchd-index.bdmv");
        // tsMuxeR's own movie objects: First Playback 2, Top Menu 1, title 0.
        let index = avchd_navigation_file(&build_index_bdmv(2, 1)).expect("index");
        assert_eq!(index.len(), 528);
        for (offset, (ours, theirs)) in index.iter().zip(reference).enumerate() {
            assert_eq!(ours, theirs, "byte {offset:#x} differs");
        }
        let mpls = build_playlist_mpls(&NativePlaylist {
            clip_id: 0,
            in_time: BDAV_START_45K,
            out_time: BDAV_START_45K + 45_000,
            streams: vec![NativeStream::video(0x1011)],
            marks: Vec::new(),
            still_time: None,
        })
        .expect("mpls");
        assert_eq!(be32(&mpls, 40), 14, "AppInfoPlayList is 14 bytes");
        assert_eq!((mpls[45], mpls[56]), (0x01, 0x40));
        // No chapters (a menu): one mark at IN_time, as tsMuxeR writes.
        let marks = be32(&mpls, 12) as usize;
        assert_eq!(&mpls[marks..marks + 6], &[0, 0, 0, 0x10, 0, 1]);
        assert_eq!(be32(&mpls, marks + 10), BDAV_START_45K);
        assert_eq!(mpls.len(), marks + 4 + 16);
        let mpls = avchd_navigation_file(&mpls).expect("avchd mpls");
        assert_eq!(&mpls[..8], b"MPLS0100");
        assert!(avchd_navigation_file(b"VIDEO_TS").is_err());
    }
}
