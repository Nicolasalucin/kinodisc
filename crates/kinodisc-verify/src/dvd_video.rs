//! The MPEG-2 video of the titles of a DVD, read from its headers: picture
//! rate, GOP length in fields and field parity.
//!
//! A DVD plays 59.94 (NTSC) or 50 (PAL) fields per second. A picture lasts
//! two fields, or three when it carries `repeat_first_field` (film stored as
//! 24 pictures). The reader here shares no code with the writer of those
//! flags (`kinodisc-dvd::pulldown`): it is the check on it.

use crate::report::CheckList;
use crate::{normalize, warn_with};
use libmkiso::UdfReader;

const PACK: usize = 2048;
const READ_PACKS: usize = 512;
/// Without `--deep`, this much of every title set is read.
const QUICK_BYTES: u64 = 32 * 1024 * 1024;
const NTSC_GOP_FIELDS: u32 = 36;
const PAL_GOP_FIELDS: u32 = 30;
const RATE_PAL: u8 = 3;
const RATE_NTSC: u8 = 4;

#[derive(Debug, Clone, Copy)]
struct Picture {
    temporal_reference: u16,
    top_first: bool,
    repeat: bool,
    /// A progressive frame picture: the only kind whose repeat flag adds a field.
    frame: bool,
    has_extension: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct VideoSummary {
    pub frame_rate_code: Option<u8>,
    pub progressive_sequence: bool,
    pub gops: u64,
    pub pictures: u64,
    pub repeated: u64,
    pub max_gop_fields: u32,
    pub parity_breaks: u64,
}

/// Follows an MPEG-2 elementary stream handed over in pieces.
#[derive(Debug, Default)]
pub(crate) struct VideoScanner {
    pending: Vec<u8>,
    gop: Vec<Picture>,
    in_gop: bool,
    last_time_code: Option<u32>,
    next_top: Option<bool>,
    progressive_sequence: bool,
    pub summary: VideoSummary,
}

impl VideoScanner {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        let mut index = 0;
        let mut keep_from = None;
        while index + 3 < self.pending.len() {
            let third = self.pending[index + 2];
            if third > 1 {
                index += 3;
                continue;
            }
            if third == 0 {
                index += 1;
                continue;
            }
            if self.pending[index] != 0 || self.pending[index + 1] != 0 {
                index += 3;
                continue;
            }
            // The longest header read here is 9 bytes from its start code.
            if index + 9 > self.pending.len() {
                keep_from = Some(index);
                break;
            }
            self.header(index);
            index += 4;
        }
        // Keep a header cut by the end of this piece, or the last bytes
        // that may begin a start code.
        let from = keep_from.unwrap_or(self.pending.len().saturating_sub(3));
        self.pending.drain(..from);
    }

    pub(crate) fn finish(&mut self) {
        self.close_gop();
    }

    /// The read stopped before the end of the video: the last GOP is not
    /// whole, so it is left out instead of being judged by the pictures
    /// that happened to be read.
    pub(crate) fn finish_cut(&mut self) {
        self.gop.clear();
    }

    fn header(&mut self, at: usize) {
        let data = &self.pending;
        match data[at + 3] {
            0xB3 => {
                // A sequence header ends the GOP before it. Its extension
                // comes before the next GOP header and may belong to another
                // encode (the next title): the pictures read so far are
                // judged with the sequence they were coded in.
                let code = data[at + 7] & 0x0F;
                self.close_gop();
                if self.summary.frame_rate_code.is_none() {
                    self.summary.frame_rate_code = Some(code);
                }
            }
            0xB8 => {
                let time_code =
                    u32::from_be_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]])
                        >> 7;
                self.close_gop();
                // A time code that does not go forward is another encode (the
                // next title): its first field owes nothing to the last one.
                if self.last_time_code.is_some_and(|last| time_code <= last) {
                    self.next_top = None;
                }
                self.last_time_code = Some(time_code);
                self.in_gop = true;
            }
            0x00 => {
                let temporal_reference =
                    (u16::from(data[at + 4]) << 2) | u16::from(data[at + 5] >> 6);
                self.gop.push(Picture {
                    temporal_reference,
                    top_first: true,
                    repeat: false,
                    frame: true,
                    has_extension: false,
                });
            }
            0xB5 => match data[at + 4] >> 4 {
                1 => {
                    let progressive = data[at + 5] & 0x08 != 0;
                    if self.summary.pictures == 0 && self.gop.is_empty() {
                        self.summary.progressive_sequence = progressive;
                    }
                    self.progressive_sequence = progressive;
                }
                8 => {
                    if let Some(picture) = self.gop.last_mut() {
                        if !picture.has_extension {
                            picture.has_extension = true;
                            picture.frame = data[at + 6] & 0x03 == 3;
                            picture.top_first = data[at + 7] & 0x80 != 0;
                            picture.repeat = data[at + 7] & 0x02 != 0;
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn close_gop(&mut self) {
        if self.gop.is_empty() {
            return;
        }
        let pictures = std::mem::take(&mut self.gop);
        // Display order when the temporal references name every place once
        // (a closed GOP); the coded order otherwise.
        let mut order: Vec<usize> = (0..pictures.len()).collect();
        let mut seen = vec![false; pictures.len()];
        let closed = pictures.iter().all(|picture| {
            let place = usize::from(picture.temporal_reference);
            place < seen.len() && !std::mem::replace(&mut seen[place], true)
        });
        if closed {
            order.sort_by_key(|&coded| pictures[coded].temporal_reference);
        }
        let mut fields = 0u32;
        for coded in order {
            let picture = pictures[coded];
            if self.progressive_sequence {
                // Whole frames only: no fields to follow.
                fields += 2;
                continue;
            }
            if !picture.frame {
                // A field picture: one field, its own parity.
                fields += 1;
                self.next_top = None;
                continue;
            }
            if self
                .next_top
                .is_some_and(|expected| expected != picture.top_first)
            {
                self.summary.parity_breaks += 1;
            }
            self.next_top = Some(if picture.repeat {
                !picture.top_first
            } else {
                picture.top_first
            });
            self.summary.repeated += u64::from(picture.repeat);
            fields += if picture.repeat { 3 } else { 2 };
        }
        if self.in_gop {
            self.summary.gops += 1;
            self.summary.max_gop_fields = self.summary.max_gop_fields.max(fields);
        }
        self.summary.pictures += pictures.len() as u64;
    }
}

/// Hands the video payload of DVD packs (2048 bytes each) to `scanner`.
pub(crate) fn demux_packs(packs: &[u8], scanner: &mut VideoScanner) {
    for pack in packs.chunks(PACK).filter(|pack| pack.len() == PACK) {
        if pack[..4] != [0, 0, 1, 0xBA] {
            continue;
        }
        let mut at = 14 + usize::from(pack[13] & 0x07);
        while at + 6 <= PACK && pack[at..at + 3] == [0, 0, 1] {
            let id = pack[at + 3];
            let length = usize::from(u16::from_be_bytes([pack[at + 4], pack[at + 5]]));
            let end = (at + 6 + length).min(PACK);
            if id == 0xE0 && at + 9 <= end {
                let payload = at + 9 + usize::from(pack[at + 8]);
                if payload < end {
                    scanner.push(&pack[payload..end]);
                }
            }
            at = end;
        }
    }
}

/// The title VOBs of each title set, in order (`VTS_nn_1.VOB`, `_2`...).
fn title_sets(reader: &UdfReader<'_>) -> Vec<(String, Vec<usize>)> {
    let mut vobs: Vec<(String, usize)> = reader
        .entries()
        .iter()
        .enumerate()
        .map(|(index, entry)| (normalize(&entry.name), index))
        .filter(|(name, _)| {
            name.starts_with("VIDEO_TS/VTS_") && name.ends_with(".VOB") && !name.ends_with("_0.VOB")
        })
        .collect();
    vobs.sort();
    let mut sets: Vec<(String, Vec<usize>)> = Vec::new();
    for (name, index) in vobs {
        let set = name
            .chars()
            .take("VIDEO_TS/VTS_nn".len())
            .collect::<String>();
        match sets.last_mut() {
            Some((last, files)) if *last == set => files.push(index),
            _ => sets.push((set, vec![index])),
        }
    }
    sets
}

fn scan_title_set(reader: &UdfReader<'_>, files: &[usize], limit: Option<u64>) -> VideoSummary {
    let mut scanner = VideoScanner::default();
    let mut buffer = vec![0u8; PACK * READ_PACKS];
    let mut total = 0u64;
    let mut cut = false;
    'files: for &file in files {
        let mut offset = 0u64;
        loop {
            let Ok(read) = reader.read_at(file, offset, &mut buffer) else {
                break 'files;
            };
            if read == 0 {
                break;
            }
            demux_packs(&buffer[..read - read % PACK], &mut scanner);
            offset += read as u64;
            total += read as u64;
            if limit.is_some_and(|limit| total >= limit) {
                cut = true;
                break 'files;
            }
            if read < buffer.len() {
                break;
            }
        }
    }
    if cut {
        scanner.finish_cut();
    } else {
        scanner.finish();
    }
    scanner.summary
}

fn rate_text(code: u8) -> &'static str {
    match code {
        RATE_PAL => "25",
        RATE_NTSC => "29,97",
        _ => "otra cadencia",
    }
}

pub(crate) fn check(reader: &UdfReader<'_>, deep: bool, checks: &mut CheckList) {
    let limit = (!deep).then_some(QUICK_BYTES);
    let scope = if deep {
        "todo el vídeo"
    } else {
        "los primeros 32 MB de cada título; --deep lo lee entero"
    };
    let mut rates: Vec<u8> = Vec::new();
    let mut longest = 0u32;
    let mut pictures = 0u64;
    let mut repeated = 0u64;
    let mut breaks = 0u64;
    let mut long_gop = false;
    for (set, files) in title_sets(reader) {
        let summary = scan_title_set(reader, &files, limit);
        let Some(code) = summary.frame_rate_code else {
            continue;
        };
        let file = format!("{set}_1.VOB");
        if !matches!(code, RATE_PAL | RATE_NTSC) {
            crate::fail_with(
                checks,
                "DVD-VIDEO-RATE",
                format!(
                    "El vídeo declara el código de cadencia {code}; un DVD admite 25 o 29,97 imágenes por segundo."
                ),
                &file,
                None,
            );
        } else if !rates.contains(&code) {
            rates.push(code);
        }
        let gop_limit = if code == RATE_PAL {
            PAL_GOP_FIELDS
        } else {
            NTSC_GOP_FIELDS
        };
        if summary.max_gop_fields > gop_limit {
            long_gop = true;
            warn_with(
                checks,
                "DVD-VIDEO-GOP",
                format!(
                    "Un GOP dura {} campos; la norma del DVD admite {gop_limit}.",
                    summary.max_gop_fields
                ),
                &file,
                None,
            );
        }
        if summary.parity_breaks > 0 {
            warn_with(
                checks,
                "DVD-VIDEO-CADENCE",
                format!(
                    "La paridad de campos se corta {} veces: una imagen empieza por el mismo campo con que terminó la anterior.",
                    summary.parity_breaks
                ),
                &file,
                None,
            );
        }
        longest = longest.max(summary.max_gop_fields);
        pictures += summary.pictures;
        repeated += summary.repeated;
        breaks += summary.parity_breaks;
    }
    if pictures == 0 {
        return;
    }
    if !rates.is_empty() {
        let text: Vec<&str> = rates.iter().map(|code| rate_text(*code)).collect();
        checks.pass(
            "DVD-VIDEO-RATE",
            format!(
                "Vídeo de los títulos a {} imágenes por segundo.",
                text.join(" y ")
            ),
        );
    }
    if !long_gop {
        checks.pass(
            "DVD-VIDEO-GOP",
            format!("GOP más largo de los títulos: {longest} campos ({scope})."),
        );
    }
    if repeated > 0 && breaks == 0 {
        checks.pass(
            "DVD-VIDEO-CADENCE",
            format!(
                "Película con marcas de repetición: {pictures} imágenes guardadas, {repeated} con un campo repetido; la paridad de campos no se corta ({scope})."
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequence(rate: u8, progressive: bool) -> Vec<u8> {
        let mut bytes = vec![
            0,
            0,
            1,
            0xB3,
            0x2D,
            0x01,
            0xE0,
            0x30 | rate,
            0xFF,
            0xFF,
            0xE0,
            0x20,
        ];
        bytes.extend([
            0,
            0,
            1,
            0xB5,
            0x14,
            if progressive { 0x8A } else { 0x82 },
            0,
            1,
            0,
            0,
        ]);
        bytes
    }

    fn group(time_code: u32) -> Vec<u8> {
        let mut bytes = vec![0, 0, 1, 0xB8];
        bytes.extend(((time_code << 7) | 0x40).to_be_bytes());
        bytes
    }

    fn picture(temporal_reference: u16, top_first: bool, repeat: bool) -> Vec<u8> {
        let mut bytes = vec![
            0,
            0,
            1,
            0,
            (temporal_reference >> 2) as u8,
            (((temporal_reference & 3) as u8) << 6) | 0x0F,
            0xFF,
            0xF8,
        ];
        let flags = 0x41 | if top_first { 0x80 } else { 0 } | if repeat { 0x02 } else { 0 };
        bytes.extend([0, 0, 1, 0xB5, 0x8F, 0xFF, 0xF3, flags, 0x80]);
        bytes.extend([0, 0, 1, 0x01, 0x12, 0x34, 0x56, 0x78, 0x9A]);
        bytes
    }

    const CADENCE: [(bool, bool); 4] = [(true, true), (false, false), (false, true), (true, false)];

    /// A film GOP of `count` pictures starting at `first` of the film, coded
    /// with its first two pictures swapped (as an encoder reorders them).
    fn film_gop(first: usize, count: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut places: Vec<usize> = (0..count).collect();
        if count > 2 {
            places.swap(1, 2);
        }
        for place in places {
            let (top_first, repeat) = CADENCE[(first + place) % 4];
            bytes.extend(picture(place as u16, top_first, repeat));
        }
        bytes
    }

    fn scan(stream: &[u8], piece: usize) -> VideoSummary {
        let mut scanner = VideoScanner::default();
        for part in stream.chunks(piece) {
            scanner.push(part);
        }
        scanner.finish();
        scanner.summary
    }

    #[test]
    fn a_flagged_film_reads_as_ntsc_with_unbroken_parity() {
        let mut stream = sequence(RATE_NTSC, false);
        stream.extend(group(0));
        stream.extend(film_gop(0, 14));
        stream.extend(sequence(RATE_NTSC, false));
        stream.extend(group(17));
        stream.extend(film_gop(14, 14));
        let expected = VideoSummary {
            frame_rate_code: Some(RATE_NTSC),
            progressive_sequence: false,
            gops: 2,
            pictures: 28,
            repeated: 14,
            max_gop_fields: 35,
            parity_breaks: 0,
        };
        for piece in [1, 2, 3, 7, 64, 4096] {
            assert_eq!(scan(&stream, piece), expected, "piece {piece}");
        }
    }

    #[test]
    fn a_cut_in_the_cadence_is_a_parity_break() {
        let mut stream = sequence(RATE_NTSC, false);
        stream.extend(group(0));
        stream.extend(film_gop(0, 14));
        stream.extend(group(17));
        // The second GOP starts the cadence again instead of going on.
        stream.extend(film_gop(0, 14));
        let summary = scan(&stream, 4096);
        assert_eq!(summary.parity_breaks, 1);
    }

    #[test]
    fn another_title_starts_its_own_parity() {
        let mut stream = sequence(RATE_NTSC, false);
        stream.extend(group(0));
        stream.extend(film_gop(0, 14));
        // The time code goes back to zero: a new encode.
        stream.extend(sequence(RATE_NTSC, false));
        stream.extend(group(0));
        stream.extend(film_gop(0, 14));
        assert_eq!(scan(&stream, 4096).parity_breaks, 0);
    }

    #[test]
    fn a_film_before_plain_video_keeps_its_last_gop() {
        let mut stream = sequence(RATE_NTSC, false);
        stream.extend(group(0));
        stream.extend(film_gop(0, 14));
        // The next title: progressive frames, announced by its sequence
        // extension before its first GOP header.
        stream.extend(sequence(RATE_NTSC, true));
        stream.extend(group(0));
        for place in 0..16 {
            stream.extend(picture(place, false, false));
        }
        for piece in [1, 7, 4096] {
            let summary = scan(&stream, piece);
            assert_eq!(
                (summary.pictures, summary.repeated, summary.gops),
                (30, 7, 2),
                "piece {piece}"
            );
            assert_eq!(summary.max_gop_fields, 35, "piece {piece}");
            assert_eq!(summary.parity_breaks, 0, "piece {piece}");
        }
    }

    #[test]
    fn a_read_cut_inside_a_gop_leaves_that_gop_out() {
        let mut stream = sequence(RATE_NTSC, false);
        stream.extend(group(0));
        stream.extend(film_gop(0, 14));
        stream.extend(sequence(RATE_NTSC, false));
        stream.extend(group(17));
        // The first pictures of the next GOP as an encoder with B-frames
        // writes them (I, P, B): the place between them was not read, so
        // they would be taken in coded order and look like a broken cadence.
        for place in [0usize, 3, 1] {
            let (top_first, repeat) = CADENCE[(14 + place) % 4];
            stream.extend(picture(place as u16, top_first, repeat));
        }

        let mut judged = VideoScanner::default();
        judged.push(&stream);
        judged.finish();
        assert!(
            judged.summary.parity_breaks > 0,
            "the cut GOP misleads when judged"
        );

        let mut scanner = VideoScanner::default();
        scanner.push(&stream);
        scanner.finish_cut();
        assert_eq!(scanner.summary.parity_breaks, 0);
        assert_eq!((scanner.summary.gops, scanner.summary.pictures), (1, 14));
        assert_eq!(scanner.summary.repeated, 7);
    }

    #[test]
    fn plain_video_has_two_fields_per_picture() {
        let mut stream = sequence(RATE_NTSC, false);
        stream.extend(group(0));
        for place in 0..18 {
            stream.extend(picture(place, true, false));
        }
        let summary = scan(&stream, 4096);
        assert_eq!((summary.pictures, summary.repeated), (18, 0));
        assert_eq!(summary.max_gop_fields, 36);
        assert_eq!(summary.parity_breaks, 0);

        // A progressive sequence: frames, no fields to follow.
        let mut stream = sequence(RATE_NTSC, true);
        stream.extend(group(0));
        for place in 0..16 {
            stream.extend(picture(place, false, false));
        }
        let summary = scan(&stream, 4096);
        assert!(summary.progressive_sequence);
        assert_eq!((summary.max_gop_fields, summary.parity_breaks), (32, 0));
    }

    #[test]
    fn video_packets_are_taken_out_of_the_packs() {
        let mut stream = sequence(RATE_PAL, false);
        stream.extend(group(0));
        for place in 0..15 {
            stream.extend(picture(place, true, false));
        }
        // Packs of 2048 bytes: pack header, one video packet, padding.
        let mut packs = Vec::new();
        for payload in stream.chunks(600) {
            let mut pack = vec![0, 0, 1, 0xBA];
            pack.extend([0x44, 0, 4, 0, 4, 1, 1, 0x89, 0xC3, 0xF8]);
            let length = (3 + payload.len()) as u16;
            pack.extend([0, 0, 1, 0xE0]);
            pack.extend(length.to_be_bytes());
            pack.extend([0x81, 0x00, 0x00]);
            pack.extend(payload);
            let padding = (PACK - pack.len() - 6) as u16;
            pack.extend([0, 0, 1, 0xBE]);
            pack.extend(padding.to_be_bytes());
            pack.resize(PACK, 0xFF);
            packs.extend(pack);
        }
        let mut scanner = VideoScanner::default();
        demux_packs(&packs, &mut scanner);
        scanner.finish();
        assert_eq!(scanner.summary.frame_rate_code, Some(RATE_PAL));
        assert_eq!(scanner.summary.pictures, 15);
        assert_eq!(scanner.summary.max_gop_fields, 30);
    }
}
