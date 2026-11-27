//! Repeat flags ("soft 3:2 pulldown") for 24-picture film on an NTSC DVD.
//!
//! An NTSC DVD plays 59.94 fields per second. A film has 23.976 pictures per
//! second. Instead of encoding every fifth picture twice, the stream stores
//! the 23.976 pictures once and tells the player how many fields each one
//! lasts: 3, 2, 3, 2... (ISO/IEC 13818-2: `repeat_first_field`,
//! `top_field_first` and `progressive_frame` in the picture coding extension,
//! with `progressive_sequence` 0). FFmpeg's MPEG-2 encoder does not write
//! these flags, so Kinodisc encodes at 23.976 and rewrites the headers here.
//!
//! Every change is a fixed bit in a header: the stream keeps its size and its
//! pictures are not touched. Written from the standard; no third-party code.
//!
//! The flags also move the moment each picture leaves the decoder's buffer
//! (annex C.11 of the standard), so the encoder line keeps part of the buffer
//! free (`rate_control_buffer_bits`) and the sequence headers written here
//! declare the buffer of a DVD, 112 units of 16384 bits.
//!
//! The input must be what the encoder line produces: frame pictures,
//! progressive, closed GOPs, `frame_rate_code` 1 (24000/1001).

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use thiserror::Error;

/// A GOP on an NTSC DVD lasts at most 36 fields (18 frames at 29.97).
pub const NTSC_MAX_GOP_FIELDS: u32 = 36;
/// The longest GOP, in film pictures, that stays inside that limit whatever
/// its place in the 3:2 cadence (14 pictures last 35 fields; 15 last 37 or 38).
pub const FILM_GOP_PICTURES: u16 = 14;

const SEQUENCE_HEADER: u8 = 0xB3;
const EXTENSION: u8 = 0xB5;
const GROUP: u8 = 0xB8;
const PICTURE: u8 = 0x00;
const SEQUENCE_EXTENSION_ID: u8 = 1;
const PICTURE_CODING_EXTENSION_ID: u8 = 8;
const RATE_FILM: u8 = 1;
const RATE_NTSC: u8 = 4;
const FRAME_PICTURE: u8 = 3;
/// `vbv_buffer_size_value` of a DVD: 112 units of 16384 bits (1,835,008).
const DVD_VBV_UNITS: u16 = 112;
const READ_CHUNK: usize = 1 << 20;

/// `(top_field_first, repeat_first_field)` of the four pictures of the
/// cadence, in display order: 3, 2, 3, 2 fields, parity unbroken.
const CADENCE: [(bool, bool); 4] = [(true, true), (false, false), (false, true), (true, false)];

#[derive(Debug, Error)]
pub enum PulldownError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("el vídeo no está a 23,976 imágenes por segundo (código de cadencia {0})")]
    NotFilmRate(u8),
    #[error("el vídeo no es progresivo: no se pueden poner marcas de repetición")]
    NotProgressive,
    #[error("flujo MPEG-2 inesperado: {0}")]
    Malformed(String),
    #[error("un GOP dura {0} campos; el DVD NTSC admite 36")]
    GopTooLong(u32),
}

/// What a stream holds, read from its headers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PulldownSummary {
    /// `frame_rate_code` of the first sequence header (4 is 29.97).
    pub frame_rate_code: u8,
    /// `progressive_sequence` of the first sequence extension.
    pub progressive_sequence: bool,
    pub gops: u64,
    pub pictures: u64,
    /// Pictures with `repeat_first_field`.
    pub repeated: u64,
    /// Fields the player shows: 2 per picture, 3 with a repeat.
    pub fields: u64,
    pub max_gop_fields: u32,
    /// Pictures whose first field has the parity of the field before it.
    pub parity_breaks: u64,
    /// Pictures that are not progressive frame pictures.
    pub not_progressive: u64,
}

#[derive(Debug, Clone, Copy)]
struct PictureAt {
    temporal_reference: u16,
    /// Offset of the picture coding extension's start code in the unit.
    coding_extension: Option<usize>,
}

/// One GOP (or the headers before the first one) and where its headers are.
#[derive(Debug, Default)]
struct Unit {
    group: Option<usize>,
    sequence_headers: Vec<usize>,
    sequence_extensions: Vec<usize>,
    pictures: Vec<PictureAt>,
}

fn scan_unit(data: &[u8]) -> Result<Unit, PulldownError> {
    let mut unit = Unit::default();
    let short = |what: &str| PulldownError::Malformed(format!("{what} cortado"));
    let mut index = 0;
    while index + 3 < data.len() {
        // A start code prefix (00 00 01) at `index` needs 01 two bytes on.
        let third = data[index + 2];
        if third > 1 {
            index += 3;
            continue;
        }
        if third == 0 {
            index += 1;
            continue;
        }
        if data[index] != 0 || data[index + 1] != 0 {
            index += 3;
            continue;
        }
        match data[index + 3] {
            SEQUENCE_HEADER => {
                if index + 12 > data.len() {
                    return Err(short("cabecera de secuencia"));
                }
                unit.sequence_headers.push(index);
            }
            GROUP => {
                if index + 8 > data.len() {
                    return Err(short("cabecera de GOP"));
                }
                if unit.group.is_some() || !unit.pictures.is_empty() {
                    return Err(PulldownError::Malformed(
                        "dos cabeceras de GOP en una unidad".to_string(),
                    ));
                }
                unit.group = Some(index);
            }
            PICTURE => {
                if index + 6 > data.len() {
                    return Err(short("cabecera de imagen"));
                }
                unit.pictures.push(PictureAt {
                    temporal_reference: (u16::from(data[index + 4]) << 2)
                        | u16::from(data[index + 5] >> 6),
                    coding_extension: None,
                });
            }
            EXTENSION => {
                if index + 5 > data.len() {
                    return Err(short("extensión"));
                }
                match data[index + 4] >> 4 {
                    SEQUENCE_EXTENSION_ID => {
                        if index + 6 > data.len() {
                            return Err(short("extensión de secuencia"));
                        }
                        unit.sequence_extensions.push(index);
                    }
                    PICTURE_CODING_EXTENSION_ID => {
                        if index + 9 > data.len() {
                            return Err(short("extensión de imagen"));
                        }
                        let Some(picture) = unit.pictures.last_mut() else {
                            return Err(PulldownError::Malformed(
                                "extensión de imagen sin imagen".to_string(),
                            ));
                        };
                        picture.coding_extension = Some(index);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        index += 4;
    }
    Ok(unit)
}

/// The place of every picture of a closed GOP in display order.
fn display_order(unit: &Unit) -> Result<Vec<usize>, PulldownError> {
    let count = unit.pictures.len();
    let mut order = vec![usize::MAX; count];
    for (coded, picture) in unit.pictures.iter().enumerate() {
        let place = usize::from(picture.temporal_reference);
        if place >= count || order[place] != usize::MAX {
            return Err(PulldownError::Malformed(format!(
                "referencia temporal {place} en un GOP de {count} imágenes (¿GOP abierto?)"
            )));
        }
        order[place] = coded;
    }
    Ok(order)
}

#[derive(Debug, Default)]
struct Progress {
    summary: PulldownSummary,
    seen_sequence_header: bool,
    seen_sequence_extension: bool,
    /// Parity the next field must have (`true` is top); unknown at the start.
    next_top: Option<bool>,
}

fn group_time_code(fields_before: u64) -> u32 {
    // 30 pictures to the second, as the 29.97 sequence header now says.
    let total = fields_before / 2;
    let pictures = (total % 30) as u32;
    let seconds = ((total / 30) % 60) as u32;
    let minutes = ((total / 1_800) % 60) as u32;
    let hours = ((total / 108_000) % 24) as u32;
    (hours << 26) | (minutes << 20) | (1 << 19) | (seconds << 13) | (pictures << 7)
}

/// Reads one unit's headers into `progress`; with `write` it also sets the
/// flags, the 29.97 rate and the GOP time code.
fn handle_unit(data: &mut [u8], progress: &mut Progress, write: bool) -> Result<(), PulldownError> {
    let unit = scan_unit(data)?;
    for &at in &unit.sequence_headers {
        let code = data[at + 7] & 0x0F;
        if write {
            if code != RATE_FILM {
                return Err(PulldownError::NotFilmRate(code));
            }
            data[at + 7] = (data[at + 7] & 0xF0) | RATE_NTSC;
            // The film line encodes against a smaller buffer (the repeat
            // flags move the moment a picture leaves it); the stream
            // declares the buffer every DVD declares. A larger declared
            // buffer never breaks a stream that fits a smaller one.
            let declared = (u16::from(data[at + 10] & 0x1F) << 5) | u16::from(data[at + 11] >> 3);
            if declared < DVD_VBV_UNITS {
                data[at + 10] = (data[at + 10] & 0xE0) | (DVD_VBV_UNITS >> 5) as u8;
                data[at + 11] = (data[at + 11] & 0x07) | (((DVD_VBV_UNITS & 0x1F) as u8) << 3);
            }
        }
        if !progress.seen_sequence_header {
            progress.seen_sequence_header = true;
            progress.summary.frame_rate_code = if write { RATE_NTSC } else { code };
        }
    }
    for &at in &unit.sequence_extensions {
        let progressive = data[at + 5] & 0x08 != 0;
        if write {
            data[at + 5] &= !0x08;
        }
        if !progress.seen_sequence_extension {
            progress.seen_sequence_extension = true;
            progress.summary.progressive_sequence = progressive && !write;
        }
    }
    if unit.pictures.is_empty() {
        return Ok(());
    }
    let Some(group) = unit.group else {
        return Err(PulldownError::Malformed(
            "imágenes sin cabecera de GOP".to_string(),
        ));
    };
    if write {
        let kept = u32::from_be_bytes([
            data[group + 4],
            data[group + 5],
            data[group + 6],
            data[group + 7],
        ]) & 0x7F;
        let code = group_time_code(progress.summary.fields) | kept;
        data[group + 4..group + 8].copy_from_slice(&code.to_be_bytes());
    }
    let first_display = progress.summary.pictures;
    let mut gop_fields = 0u32;
    for (place, coded) in display_order(&unit)?.into_iter().enumerate() {
        let Some(at) = unit.pictures[coded].coding_extension else {
            return Err(PulldownError::Malformed(
                "imagen sin extensión de codificación (¿MPEG-1?)".to_string(),
            ));
        };
        let frame_picture = data[at + 6] & 0x03 == FRAME_PICTURE;
        let progressive_frame = data[at + 8] & 0x80 != 0;
        if write {
            if !frame_picture || !progressive_frame {
                return Err(PulldownError::NotProgressive);
            }
            let (top_first, repeat) = CADENCE[((first_display + place as u64) % 4) as usize];
            let mut flags = data[at + 7] & !0x82;
            if top_first {
                flags |= 0x80;
            }
            if repeat {
                flags |= 0x02;
            }
            data[at + 7] = flags;
        }
        let top_first = data[at + 7] & 0x80 != 0;
        let repeat = data[at + 7] & 0x02 != 0;
        if !frame_picture || !progressive_frame {
            progress.summary.not_progressive += 1;
        }
        if progress
            .next_top
            .is_some_and(|expected| expected != top_first)
        {
            progress.summary.parity_breaks += 1;
        }
        // Two fields end on the other parity; a third brings the first back.
        progress.next_top = Some(if repeat { !top_first } else { top_first });
        progress.summary.repeated += u64::from(repeat);
        gop_fields += if repeat { 3 } else { 2 };
    }
    if write && gop_fields > NTSC_MAX_GOP_FIELDS {
        return Err(PulldownError::GopTooLong(gop_fields));
    }
    progress.summary.gops += 1;
    progress.summary.pictures += unit.pictures.len() as u64;
    progress.summary.fields += u64::from(gop_fields);
    progress.summary.max_gop_fields = progress.summary.max_gop_fields.max(gop_fields);
    Ok(())
}

fn find_group(data: &[u8], from: usize) -> Option<usize> {
    let mut index = from;
    while index + 3 < data.len() {
        let third = data[index + 2];
        if third > 1 {
            index += 3;
        } else if third == 0 {
            index += 1;
        } else if data[index] == 0 && data[index + 1] == 0 {
            if data[index + 3] == GROUP {
                return Some(index);
            }
            index += 4;
        } else {
            index += 3;
        }
    }
    None
}

/// Walks the stream one GOP at a time, so a whole film never sits in memory.
fn walk<R: Read, W: Write>(
    reader: &mut R,
    mut writer: Option<&mut W>,
    chunk: usize,
) -> Result<PulldownSummary, PulldownError> {
    let write = writer.is_some();
    let mut progress = Progress::default();
    let mut pending: Vec<u8> = Vec::new();
    // A GOP header at offset 0 opens the unit; the next one closes it.
    let mut searched = 1usize;
    let mut buffer = vec![0u8; chunk.max(1)];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        pending.extend_from_slice(&buffer[..read]);
        while let Some(end) = find_group(&pending, searched) {
            handle_unit(&mut pending[..end], &mut progress, write)?;
            if let Some(writer) = writer.as_mut() {
                writer.write_all(&pending[..end])?;
            }
            pending.drain(..end);
            searched = 1;
        }
        // The last three bytes may be the start of a start code.
        searched = pending.len().saturating_sub(3).max(1);
    }
    if !pending.is_empty() {
        handle_unit(&mut pending, &mut progress, write)?;
        if let Some(writer) = writer.as_mut() {
            writer.write_all(&pending)?;
        }
    }
    if let Some(writer) = writer.as_mut() {
        writer.flush()?;
    }
    if !progress.seen_sequence_header {
        return Err(PulldownError::Malformed(
            "sin cabecera de secuencia".to_string(),
        ));
    }
    Ok(progress.summary)
}

/// Copies the 23.976 stream `input` to `output` as a 29.97 stream whose
/// pictures carry the 3:2 repeat flags.
pub fn apply_soft_pulldown(input: &Path, output: &Path) -> Result<PulldownSummary, PulldownError> {
    let mut reader = BufReader::with_capacity(READ_CHUNK, File::open(input)?);
    let mut writer = BufWriter::with_capacity(READ_CHUNK, File::create(output)?);
    let summary = walk(&mut reader, Some(&mut writer), READ_CHUNK);
    drop(writer);
    if summary.is_err() {
        let _ = std::fs::remove_file(output);
    }
    summary
}

/// Reads the rate, the repeat flags and the field parity of a stream.
pub fn inspect(path: &Path) -> Result<PulldownSummary, PulldownError> {
    let mut reader = BufReader::with_capacity(READ_CHUNK, File::open(path)?);
    walk(&mut reader, None::<&mut std::io::Sink>, READ_CHUNK)
}

#[cfg(test)]
mod tests {
    use super::*;

    const I: u8 = 1;
    const P: u8 = 2;
    const B: u8 = 3;

    fn sequence(rate: u8) -> Vec<u8> {
        let mut bytes = vec![0, 0, 1, SEQUENCE_HEADER, 0x2D, 0x01, 0xE0, 0x30 | rate];
        bytes.extend([0xFF, 0xFF, 0xE0, 0x20]);
        // Sequence extension: progressive_sequence 1, 4:2:0.
        bytes.extend([0, 0, 1, EXTENSION, 0x14, 0x8A, 0x00, 0x01, 0x00, 0x00]);
        bytes
    }

    fn group() -> Vec<u8> {
        // Time code 0, marker bit, closed GOP.
        vec![0, 0, 1, GROUP, 0x00, 0x08, 0x00, 0x40]
    }

    fn picture(temporal_reference: u16, kind: u8) -> Vec<u8> {
        let mut bytes = vec![
            0,
            0,
            1,
            PICTURE,
            (temporal_reference >> 2) as u8,
            (((temporal_reference & 3) as u8) << 6) | (kind << 3) | 0x07,
            0xFF,
            0xF8,
        ];
        // Coding extension as the encoder writes a progressive frame.
        bytes.extend([0, 0, 1, EXTENSION, 0x8F, 0xFF, 0xF3, 0x41, 0x80]);
        // A slice with bytes that are not a start code.
        bytes.extend([0, 0, 1, 0x01, 0x12, 0x34, 0x56, 0x02, 0x00, 0x03]);
        bytes
    }

    /// GOPs of `sizes` pictures, coded I P B B P B B..., closed.
    fn stream(rate: u8, sizes: &[u16]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for &size in sizes {
            bytes.extend(sequence(rate));
            bytes.extend(group());
            bytes.extend(picture(0, I));
            let mut anchor = 0;
            while anchor + 1 < size {
                let next = (anchor + 3).min(size - 1);
                bytes.extend(picture(next, P));
                for between in anchor + 1..next {
                    bytes.extend(picture(between, B));
                }
                anchor = next;
            }
        }
        bytes
    }

    fn rewrite(input: &[u8], chunk: usize) -> Result<(Vec<u8>, PulldownSummary), PulldownError> {
        let mut output = Vec::new();
        let summary = walk(&mut &input[..], Some(&mut output), chunk)?;
        Ok((output, summary))
    }

    fn read(input: &[u8]) -> PulldownSummary {
        walk(&mut &input[..], None::<&mut std::io::Sink>, 4096).unwrap()
    }

    /// `(top_field_first, repeat_first_field)` of every picture, display order.
    fn flags(stream: &[u8]) -> Vec<(bool, bool)> {
        let mut all = Vec::new();
        let mut rest = stream;
        let mut start = find_group(rest, 0);
        while let Some(at) = start {
            let end = find_group(rest, at + 4).unwrap_or(rest.len());
            let unit = scan_unit(&rest[at..end]).unwrap();
            for coded in display_order(&unit).unwrap() {
                let ext = at + unit.pictures[coded].coding_extension.unwrap();
                all.push((rest[ext + 7] & 0x80 != 0, rest[ext + 7] & 0x02 != 0));
            }
            rest = &rest[end..];
            start = find_group(rest, 0);
        }
        all
    }

    /// `vbv_buffer_size_value` of every sequence header.
    fn declared_buffers(stream: &[u8]) -> Vec<u16> {
        let mut all: Vec<u16> = Vec::new();
        let mut index = 0;
        while index + 12 <= stream.len() {
            if stream[index..index + 4] == [0, 0, 1, SEQUENCE_HEADER] {
                all.push(
                    (u16::from(stream[index + 10] & 0x1F) << 5)
                        | u16::from(stream[index + 11] >> 3),
                );
            }
            index += 1;
        }
        all
    }

    #[test]
    fn the_stream_declares_the_dvd_buffer() {
        // The test header declares 4 units, as an encode against a smaller
        // buffer would declare fewer than 112.
        let input = stream(RATE_FILM, &[14, 6]);
        assert_eq!(declared_buffers(&input), vec![4, 4]);
        let (output, _) = rewrite(&input, 4096).unwrap();
        assert_eq!(declared_buffers(&output), vec![112, 112]);
        // The bits around the field stay: marker bit, no matrices.
        let at = output
            .windows(4)
            .position(|w| w == [0, 0, 1, SEQUENCE_HEADER])
            .unwrap();
        assert_eq!(output[at + 8..at + 12], [0xFF, 0xFF, 0xE3, 0x80]);

        // A header that already declares more is left alone.
        let mut large = input.clone();
        large[10] = 0xE3;
        large[11] = 0xF8; // 127 units
        let (output, _) = rewrite(&large, 4096).unwrap();
        assert_eq!(declared_buffers(&output)[0], 127);
    }

    #[test]
    fn film_pictures_get_the_three_two_cadence_in_display_order() {
        let input = stream(RATE_FILM, &[14, 14, 6]);
        let before = read(&input);
        assert_eq!(before.frame_rate_code, RATE_FILM);
        assert!(before.progressive_sequence);
        assert_eq!((before.pictures, before.repeated), (34, 0));

        let (output, summary) = rewrite(&input, READ_CHUNK).unwrap();
        assert_eq!(output.len(), input.len(), "only fixed bits change");
        assert_eq!(summary.frame_rate_code, RATE_NTSC);
        assert_eq!((summary.gops, summary.pictures), (3, 34));
        // 34 pictures: 8 whole cadences (80 fields) and 3 + 2.
        assert_eq!(summary.fields, 85);
        assert_eq!(summary.repeated, 17);
        assert_eq!(summary.max_gop_fields, 35);
        assert_eq!(summary.parity_breaks, 0);

        let cadence = flags(&output);
        assert_eq!(cadence.len(), 34);
        for (place, flag) in cadence.iter().enumerate() {
            assert_eq!(*flag, CADENCE[place % 4], "picture {place}");
        }
        // What a reader of the result sees.
        let after = read(&output);
        assert_eq!(after.frame_rate_code, RATE_NTSC);
        assert!(!after.progressive_sequence);
        assert_eq!(
            (after.fields, after.repeated, after.parity_breaks),
            (85, 17, 0)
        );
        assert_eq!(after.not_progressive, 0);
    }

    #[test]
    fn only_header_bits_change() {
        let input = stream(RATE_FILM, &[14, 14]);
        let (output, _) = rewrite(&input, READ_CHUNK).unwrap();
        let changed: Vec<usize> = (0..input.len())
            .filter(|&at| input[at] != output[at])
            .collect();
        for at in &changed {
            let unit = &input[..];
            // Every changed byte sits right after a header start code (the
            // declared buffer is in bytes 10 and 11 of a sequence header).
            let near = (at.saturating_sub(11)..*at).any(|start| {
                unit[start..].starts_with(&[0, 0, 1])
                    && matches!(unit[start + 3], SEQUENCE_HEADER | EXTENSION | GROUP)
            });
            assert!(near, "byte {at} is not in a header");
        }
        assert!(!changed.is_empty());
    }

    #[test]
    fn the_result_does_not_depend_on_how_the_file_is_read() {
        let input = stream(RATE_FILM, &[14, 9, 14, 3]);
        let (whole, summary) = rewrite(&input, READ_CHUNK).unwrap();
        for chunk in [1, 2, 3, 5, 7, 64, 255] {
            let (pieces, same) = rewrite(&input, chunk).unwrap();
            assert_eq!(pieces, whole, "chunk {chunk}");
            assert_eq!(same, summary, "chunk {chunk}");
        }
    }

    #[test]
    fn the_time_code_counts_thirty_pictures_per_second() {
        // 24 pictures last 60 fields: one second on the disc.
        let input = stream(RATE_FILM, &[12, 12, 12]);
        let (output, _) = rewrite(&input, READ_CHUNK).unwrap();
        let mut codes = Vec::new();
        let mut from = 0;
        while let Some(at) = find_group(&output, from) {
            codes.push(u32::from_be_bytes([
                output[at + 4],
                output[at + 5],
                output[at + 6],
                output[at + 7],
            ]));
            from = at + 4;
        }
        let pictures = |code: u32| (code >> 7) & 0x3F;
        let seconds = |code: u32| (code >> 13) & 0x3F;
        assert_eq!(codes.len(), 3);
        assert_eq!((seconds(codes[0]), pictures(codes[0])), (0, 0));
        assert_eq!((seconds(codes[1]), pictures(codes[1])), (0, 15));
        assert_eq!((seconds(codes[2]), pictures(codes[2])), (1, 0));
        for code in codes {
            assert_eq!(code & 0x40, 0x40, "closed_gop is kept");
            assert_eq!(code & (1 << 19), 1 << 19, "marker bit");
        }
    }

    #[test]
    fn a_stream_that_is_not_film_is_refused() {
        let ntsc = stream(RATE_NTSC, &[14]);
        assert!(matches!(
            rewrite(&ntsc, READ_CHUNK),
            Err(PulldownError::NotFilmRate(RATE_NTSC))
        ));
        let exact_24 = stream(2, &[14]);
        assert!(matches!(
            rewrite(&exact_24, READ_CHUNK),
            Err(PulldownError::NotFilmRate(2))
        ));
    }

    #[test]
    fn interlaced_pictures_are_refused() {
        let mut input = stream(RATE_FILM, &[6]);
        let unit_at = find_group(&input, 0).unwrap();
        let unit = scan_unit(&input[unit_at..]).unwrap();
        let ext = unit_at + unit.pictures[2].coding_extension.unwrap();
        input[ext + 8] &= !0x80; // progressive_frame 0
        assert!(matches!(
            rewrite(&input, READ_CHUNK),
            Err(PulldownError::NotProgressive)
        ));
    }

    #[test]
    fn a_gop_longer_than_the_disc_allows_is_refused() {
        // 15 pictures last 37 or 38 fields.
        let input = stream(RATE_FILM, &[15]);
        assert!(matches!(
            rewrite(&input, READ_CHUNK),
            Err(PulldownError::GopTooLong(38))
        ));
        // 14 fit at every place of the cadence.
        let input = stream(RATE_FILM, &[1, 14, 1, 14, 1, 14, 14]);
        let (_, summary) = rewrite(&input, READ_CHUNK).unwrap();
        assert_eq!(summary.max_gop_fields, 35);
    }

    #[test]
    fn an_open_gop_is_refused() {
        let mut input = stream(RATE_FILM, &[6]);
        let unit_at = find_group(&input, 0).unwrap();
        // The second picture claims a place outside the GOP.
        let second = unit_at
            + 8
            + input[unit_at + 8..]
                .windows(4)
                .enumerate()
                .filter(|(_, bytes)| *bytes == [0, 0, 1, PICTURE])
                .nth(1)
                .unwrap()
                .0;
        input[second + 4] = 0x10;
        assert!(matches!(
            rewrite(&input, READ_CHUNK),
            Err(PulldownError::Malformed(_))
        ));
    }

    #[test]
    fn a_broken_parity_is_counted() {
        let input = stream(RATE_FILM, &[8]);
        let (mut output, _) = rewrite(&input, READ_CHUNK).unwrap();
        let unit_at = find_group(&output, 0).unwrap();
        let unit = scan_unit(&output[unit_at..]).unwrap();
        // Display picture 1 (bottom first, 2 fields) becomes top first.
        let coded = display_order(&unit).unwrap()[1];
        let ext = unit_at + unit.pictures[coded].coding_extension.unwrap();
        output[ext + 7] |= 0x80;
        assert!(read(&output).parity_breaks >= 1);
    }
}
