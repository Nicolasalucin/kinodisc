//! Native Rust + FFmpeg BDMV authoring fallback.
//!
//! This path is used when tsMuxeR is unavailable or the host is 32-bit. It
//! writes the movie M2TS directly with FFmpeg (copying H.264, AC-3 and PGS) and
//! builds the BDMV navigation files with the pure builders in `kinodisc-hdmv`.
//! Menus reuse the native IG packets but are muxed through the same FFmpeg path.
//!
//! FFmpeg's M2TS is then made a BDAV stream (`kinodisc_hdmv::bdav`: arrival
//! times from the PCR, SIT, HDMV/AC-3 descriptors) and its key frames feed
//! the clip info's EP map; [`finish_avchd_tree`] turns the navigation files
//! into AVCHD for DVD media. Hardware players refused the 1.2.5 output.

use crate::{push_file_input, run_stage, AvchdError};
use kinodisc_hdmv::bdav::{fix_m2ts_in_place, scan_m2ts};
use kinodisc_hdmv::native::{
    avchd_navigation_file, build_clpi, build_index_bdmv, build_movie_object_with_gpr0_playmark,
    build_playlist_mpls, parse_pmt_from_m2ts_prefix, ClipTransport, NativeClip, NativePlaylist,
    NativeStream, BDAV_START_45K,
};
use kinodisc_hdmv::{
    append_menu_movie_object, dummy_pgs_for_slots, inject_ig_into_menu_m2ts_file,
    pes_transport_packet_count, set_index_menu_object, IG_PID,
};
use std::fs;
use std::path::{Path, PathBuf};

/// How the AVCHD/BDMV folder is multiplexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxBackend {
    Auto,
    Native,
    TsMuxer,
}

impl MuxBackend {
    /// Pure selector used by tests and by `resolve`.
    pub fn select(self, tsmuxer_available: bool, pointer_width: usize) -> Self {
        match self {
            Self::Auto if pointer_width == 32 || !tsmuxer_available => Self::Native,
            Self::Auto => Self::TsMuxer,
            other => other,
        }
    }

    /// Resolves `Auto` using the environment and the host pointer width.
    pub fn resolve(self) -> Self {
        let requested = match kinodisc_toolchain::legacy::env_var("KINODISC_MUXER")
            .ok_or(())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "native" => Self::Native,
            "tsmuxer" | "tsmuxeR" => Self::TsMuxer,
            _ => self,
        };
        requested.select(tsmuxer_is_available(), std::mem::size_of::<usize>() * 8)
    }
}

/// Whether a usable tsMuxeR executable is available on this host.
pub fn tsmuxer_is_available() -> bool {
    ["tsMuxeR", "tsmuxer"].iter().any(|name| {
        let path = kinodisc_toolchain::resolve_binary(name);
        path.components().count() > 1 && path.is_file()
    })
}

#[derive(Debug, Clone)]
pub struct NativeMovieSubtitle<'a> {
    /// `None` means the PGS track is already inside `video` (combined MKV).
    pub path: Option<&'a Path>,
    pub language: &'a str,
}

#[derive(Debug, Clone)]
pub struct NativeMovieRequest<'a> {
    pub video: &'a Path,
    pub output_dir: &'a Path,
    pub duration_ms: u64,
    pub chapters_ms: &'a [u64],
    pub audio_languages: &'a [&'a str],
    pub subtitles: &'a [NativeMovieSubtitle<'a>],
    /// BDMV video attribute from `AvchdVideoConfig::bdmv_video_attr`.
    pub video_attr: u8,
    /// Clip-info aspect code (`kinodisc_hdmv::native::ASPECT_16_9` / `ASPECT_4_3`).
    pub video_aspect: u8,
}

#[derive(Debug, Clone, Default)]
pub struct NativeMovieReport {
    pub clip: u16,
    pub playlist: u16,
    pub object: u16,
    pub movie_objects: u16,
    pub source_packets: u32,
    pub streams: Vec<NativeStream>,
}

#[derive(Debug, Clone)]
pub struct NativeMenuRequest<'a> {
    pub bdmv_dir: &'a Path,
    pub work_dir: &'a Path,
    pub video_h264: &'a Path,
    pub audio_ac3: &'a Path,
    pub ig_pes_packets: &'a [Vec<u8>],
    pub width: u16,
    pub height: u16,
    pub duration_ms: u64,
    pub clip_id: u16,
    pub playlist_id: u16,
    pub expected_object: u16,
    /// BDMV video attribute from `AvchdVideoConfig::bdmv_video_attr`.
    pub video_attr: u8,
    /// Clip-info aspect code (`kinodisc_hdmv::native::ASPECT_16_9` / `ASPECT_4_3`).
    pub video_aspect: u8,
}

#[derive(Debug, Clone)]
pub struct NativeMenuReport {
    pub playlist: u16,
    pub clip: u16,
    pub object: u16,
    pub source_packets: u32,
    pub ig_packets: usize,
}

#[derive(Debug, Clone)]
pub struct NativeBdmvMuxer {
    pub ffmpeg: PathBuf,
    /// Priority of the muxing FFmpeg processes (low in the background mode).
    pub cpu: kinodisc_toolchain::CpuUsage,
}

impl Default for NativeBdmvMuxer {
    fn default() -> Self {
        Self::new(kinodisc_toolchain::resolve_binary("ffmpeg"))
    }
}

impl NativeBdmvMuxer {
    pub fn new(ffmpeg: impl Into<PathBuf>) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            cpu: kinodisc_toolchain::CpuUsage::default(),
        }
    }

    pub fn with_cpu(mut self, cpu: kinodisc_toolchain::CpuUsage) -> Self {
        self.cpu = cpu;
        self
    }

    /// Muxes `combined.mkv` to the main BDMV clip and writes the base nav tree.
    pub async fn mux_movie(
        &self,
        request: &NativeMovieRequest<'_>,
    ) -> Result<NativeMovieReport, AvchdError> {
        let bdmv = request.output_dir.join("BDMV");
        create_bdmv_dirs(&bdmv)?;
        let stream_dir = bdmv.join("STREAM");
        let stream_path = stream_dir.join("00000.m2ts");

        let mut args = vec![
            "-hide_banner".to_string(),
            "-y".to_string(),
            "-loglevel".to_string(),
            "error".to_string(),
        ];
        push_file_input(&mut args, request.video);
        for subtitle in request.subtitles {
            if let Some(path) = subtitle.path {
                args.extend(["-f".to_string(), "sup".to_string()]);
                push_file_input(&mut args, path);
            }
        }
        args.extend([
            "-map".to_string(),
            "0:v:0".to_string(),
            "-map".to_string(),
            "0:a?".to_string(),
        ]);
        if request
            .subtitles
            .iter()
            .any(|subtitle| subtitle.path.is_none())
        {
            args.extend(["-map".to_string(), "0:s?".to_string()]);
        }
        let mut external_input = 1usize;
        for subtitle in request.subtitles {
            if subtitle.path.is_some() {
                args.extend(["-map".to_string(), format!("{external_input}:s:0")]);
                external_input += 1;
            }
        }
        args.extend([
            "-c".to_string(),
            "copy".to_string(),
            "-mpegts_m2ts_mode".to_string(),
            "1".to_string(),
            "-mpegts_flags".to_string(),
            "+resend_headers".to_string(),
            "-output_ts_offset".to_string(),
            "600".to_string(),
            "-f".to_string(),
            "mpegts".to_string(),
            stream_path.display().to_string(),
        ]);
        run_stage(&self.ffmpeg, self.cpu, &args, "m2ts nativo", None).await?;

        let mut streams = resolve_streams(
            &stream_path,
            request.audio_languages,
            request.subtitles,
            request.video_attr,
            request.video_aspect,
        )?;
        let in_time = BDAV_START_45K;
        let out_time = in_time
            .checked_add(u32::try_from(request.duration_ms.saturating_mul(45)).unwrap_or(u32::MAX))
            .ok_or_else(|| AvchdError::InvalidConfig("duración BDMV fuera de rango".into()))?;
        let mut marks: Vec<u32> = request
            .chapters_ms
            .iter()
            .filter_map(|millis| {
                let offset = u32::try_from(millis.saturating_mul(45)).ok()?;
                let value = in_time.checked_add(offset)?;
                (value <= out_time).then_some(value)
            })
            .collect();
        marks.sort_unstable();
        marks.dedup();
        if marks.first().copied() != Some(in_time) {
            marks.insert(0, in_time);
        }
        let (source_packets, transport) = make_bdav_clip(
            &stream_path,
            &mut streams,
            request.video_attr,
            request.video_aspect,
            in_time,
            out_time,
        )?;
        let playlist = build_playlist_mpls(&NativePlaylist {
            clip_id: 0,
            in_time,
            out_time,
            streams: streams.clone(),
            marks,
            still_time: None,
        })
        .map_err(native_error)?;
        let clpi = build_clpi(&NativeClip {
            clip_id: 0,
            source_packets,
            streams: streams.clone(),
            transport: Some(transport),
        })
        .map_err(native_error)?;
        let index = build_index_bdmv(0, 0);
        let movie_object = build_movie_object_with_gpr0_playmark(0).map_err(native_error)?;
        write_file(&bdmv.join("PLAYLIST/00000.mpls"), &playlist)?;
        write_file(&bdmv.join("CLIPINF/00000.clpi"), &clpi)?;
        write_file(&bdmv.join("index.bdmv"), &index)?;
        write_file(&bdmv.join("MovieObject.bdmv"), &movie_object)?;
        write_backup(&bdmv, "PLAYLIST/00000.mpls", &playlist)?;
        write_backup(&bdmv, "CLIPINF/00000.clpi", &clpi)?;
        write_backup(&bdmv, "index.bdmv", &index)?;
        write_backup(&bdmv, "MovieObject.bdmv", &movie_object)?;
        Ok(NativeMovieReport {
            clip: 0,
            playlist: 0,
            object: 0,
            movie_objects: 1,
            source_packets,
            streams,
        })
    }

    /// Builds one native menu clip and appends its MovieObject.
    pub async fn attach_menu(
        &self,
        request: &NativeMenuRequest<'_>,
    ) -> Result<NativeMenuReport, AvchdError> {
        let bdmv = request.bdmv_dir.join("BDMV");
        fs::create_dir_all(&bdmv)?;
        create_bdmv_dirs(&bdmv)?;
        let work = request.work_dir;
        fs::create_dir_all(work)?;
        let needed = pes_transport_packet_count(request.ig_pes_packets);
        let mut last_error = None;
        for multiplier in [2usize, 4usize] {
            let dummy_path = work.join(format!("menu-pgs-{multiplier}.sup"));
            let dummy = dummy_pgs_for_slots(
                request.width,
                request.height,
                needed.saturating_mul(multiplier),
            )
            .map_err(native_error)?;
            fs::write(&dummy_path, dummy)?;
            let raw_path = work.join(format!("menu-{multiplier}.m2ts"));
            let mut args = vec![
                "-hide_banner".to_string(),
                "-y".to_string(),
                "-loglevel".to_string(),
                "error".to_string(),
            ];
            // A raw `.h264` elementary stream has no timestamps (FFmpeg 9
            // refuses to copy it into MPEG-TS); containers are auto-detected.
            if request
                .video_h264
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("h264"))
            {
                args.extend(["-f".to_string(), "h264".to_string()]);
            }
            push_file_input(&mut args, request.video_h264);
            args.extend(["-f".to_string(), "ac3".to_string()]);
            push_file_input(&mut args, request.audio_ac3);
            args.extend(["-f".to_string(), "sup".to_string()]);
            push_file_input(&mut args, &dummy_path);
            args.extend([
                "-map".to_string(),
                "0:v:0".to_string(),
                "-map".to_string(),
                "1:a:0".to_string(),
                "-map".to_string(),
                "2:s:0".to_string(),
                "-c".to_string(),
                "copy".to_string(),
                "-mpegts_m2ts_mode".to_string(),
                "1".to_string(),
                "-mpegts_flags".to_string(),
                "+resend_headers".to_string(),
                "-output_ts_offset".to_string(),
                "600".to_string(),
                "-f".to_string(),
                "mpegts".to_string(),
                raw_path.display().to_string(),
            ]);
            run_stage(&self.ffmpeg, self.cpu, &args, "menú m2ts nativo", None).await?;
            let final_path = bdmv
                .join("STREAM")
                .join(format!("{:05}.m2ts", request.clip_id));
            let injected = inject_ig_into_menu_m2ts_file(
                &raw_path,
                &final_path,
                request.ig_pes_packets,
                IG_PID,
            );
            match injected {
                Ok(_) => {
                    let duration = request.duration_ms.max(1);
                    let in_time = BDAV_START_45K;
                    let out_time = in_time
                        .checked_add(u32::try_from(duration.saturating_mul(45)).unwrap_or(u32::MAX))
                        .ok_or_else(|| {
                            AvchdError::InvalidConfig("duración de menú fuera de rango".into())
                        })?;
                    let mut streams = resolve_menu_streams(
                        &final_path,
                        request.video_attr,
                        request.video_aspect,
                    )?;
                    let (source_packets, transport) = make_bdav_clip(
                        &final_path,
                        &mut streams,
                        request.video_attr,
                        request.video_aspect,
                        in_time,
                        out_time,
                    )?;
                    let playlist = build_playlist_mpls(&NativePlaylist {
                        clip_id: request.clip_id,
                        in_time,
                        out_time,
                        streams: streams.clone(),
                        marks: Vec::new(),
                        still_time: None,
                    })
                    .map_err(native_error)?;
                    let clpi = build_clpi(&NativeClip {
                        clip_id: request.clip_id,
                        source_packets,
                        streams,
                        transport: Some(transport),
                    })
                    .map_err(native_error)?;
                    write_file(
                        &bdmv.join(format!("PLAYLIST/{:05}.mpls", request.playlist_id)),
                        &playlist,
                    )?;
                    write_file(
                        &bdmv.join(format!("CLIPINF/{:05}.clpi", request.clip_id)),
                        &clpi,
                    )?;
                    write_backup(
                        &bdmv,
                        &format!("PLAYLIST/{:05}.mpls", request.playlist_id),
                        &playlist,
                    )?;
                    write_backup(
                        &bdmv,
                        &format!("CLIPINF/{:05}.clpi", request.clip_id),
                        &clpi,
                    )?;
                    let movie_object_path = bdmv.join("MovieObject.bdmv");
                    let movie_object = fs::read(&movie_object_path)?;
                    let (movie_object, object) =
                        append_menu_movie_object(&movie_object, request.playlist_id)
                            .map_err(native_error)?;
                    if object != request.expected_object {
                        return Err(AvchdError::InvalidConfig(format!(
                            "objeto de menú esperado {}, creado {object}",
                            request.expected_object
                        )));
                    }
                    write_file(&movie_object_path, &movie_object)?;
                    write_backup(&bdmv, "MovieObject.bdmv", &movie_object)?;
                    return Ok(NativeMenuReport {
                        playlist: request.playlist_id,
                        clip: request.clip_id,
                        object,
                        source_packets,
                        ig_packets: needed,
                    });
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error
            .map(native_error)
            .unwrap_or_else(|| AvchdError::InvalidConfig("no se pudo reservar IG".into())))
    }

    /// Points First Playback and Top Menu at the root menu object.
    pub fn set_root_menu(&self, bdmv_dir: &Path, object: u16) -> Result<(), AvchdError> {
        let bdmv = bdmv_dir.join("BDMV");
        let index_path = bdmv.join("index.bdmv");
        let index = fs::read(&index_path)?;
        let index = set_index_menu_object(&index, object).map_err(native_error)?;
        write_file(&index_path, &index)?;
        write_backup(&bdmv, "index.bdmv", &index)?;
        Ok(())
    }
}

/// Makes FFmpeg's M2TS at `path` a BDAV stream in place (see the module
/// notes), sets the audio attributes from its AC-3 headers and returns the
/// packet count and the clip-info transport facts (EP map included).
fn make_bdav_clip(
    path: &Path,
    streams: &mut [NativeStream],
    video_attr: u8,
    video_aspect: u8,
    in_time: u32,
    out_time: u32,
) -> Result<(u32, ClipTransport), AvchdError> {
    let scan = scan_m2ts(path).map_err(native_error)?;
    for stream in streams.iter_mut() {
        if let Some(facts) = scan.ac3_for(stream.pid) {
            stream.audio_attr = facts.attribute();
        }
    }
    fix_m2ts_in_place(path, &scan, video_attr, video_aspect).map_err(native_error)?;
    Ok((
        scan.packets,
        ClipTransport::from_scan(&scan, in_time, out_time),
    ))
}

/// Turns the BDMV navigation files under `output_dir` into AVCHD (version
/// 0100 and the AVCHD index extension), as tsMuxeR `--avchd` writes them:
/// the form Sony players (PS3/PS4) expect on DVD media. Call it once the
/// tree is complete (menus attached, root menu set), before the ISO.
pub fn finish_avchd_tree(output_dir: &Path) -> Result<(), AvchdError> {
    let bdmv = output_dir.join("BDMV");
    let mut files = Vec::new();
    for base in [bdmv.clone(), bdmv.join("BACKUP")] {
        for name in ["index.bdmv", "MovieObject.bdmv"] {
            files.push(base.join(name));
        }
        for (folder, extension) in [("PLAYLIST", "mpls"), ("CLIPINF", "clpi")] {
            let Ok(entries) = fs::read_dir(base.join(folder)) else {
                continue;
            };
            for entry in entries {
                let path = entry?.path();
                if path
                    .extension()
                    .is_some_and(|value| value.eq_ignore_ascii_case(extension))
                {
                    files.push(path);
                }
            }
        }
    }
    for path in files.iter().filter(|path| path.is_file()) {
        let bytes = fs::read(path)?;
        let converted = avchd_navigation_file(&bytes).map_err(native_error)?;
        fs::write(path, converted)?;
    }
    fs::create_dir_all(bdmv.join("AUXDATA"))?;
    Ok(())
}

fn normalize_language(language: &str) -> String {
    kinodisc_domain::LanguageCode::from_code(language)
        .unwrap_or(kinodisc_domain::LanguageCode::Undetermined)
        .iso_639_2()
        .to_string()
}

fn native_error(error: impl std::fmt::Display) -> AvchdError {
    AvchdError::InvalidConfig(error.to_string())
}

fn create_bdmv_dirs(bdmv: &Path) -> std::io::Result<()> {
    for directory in [
        "PLAYLIST",
        "CLIPINF",
        "STREAM",
        "BACKUP/PLAYLIST",
        "BACKUP/CLIPINF",
        "BACKUP",
        "AUXDATA",
        "../CERTIFICATE/BACKUP",
    ] {
        fs::create_dir_all(bdmv.join(directory))?;
    }
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)
}

fn write_backup(bdmv: &Path, relative: &str, bytes: &[u8]) -> std::io::Result<()> {
    write_file(&bdmv.join("BACKUP").join(relative), bytes)
}

fn read_pmt_prefix(path: &Path) -> Result<Vec<u8>, AvchdError> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut prefix = vec![0u8; 1024 * 1024];
    let read = file.read(&mut prefix)?;
    prefix.truncate(read);
    Ok(prefix)
}

fn resolve_streams(
    path: &Path,
    audio_languages: &[&str],
    subtitles: &[NativeMovieSubtitle<'_>],
    video_attr: u8,
    video_aspect: u8,
) -> Result<Vec<NativeStream>, AvchdError> {
    let prefix = read_pmt_prefix(path)?;
    let pmt = parse_pmt_from_m2ts_prefix(&prefix).map_err(native_error)?;
    let mut streams = Vec::with_capacity(pmt.len());
    let mut subtitle_index = 0usize;
    let mut audio_index = 0usize;
    for stream in pmt {
        let language = match stream.stream_type {
            0x1B => None,
            0x81 | 0x82 | 0x83 | 0x86 => {
                let language = audio_languages
                    .get(audio_index)
                    .map(|language| normalize_language(language));
                audio_index += 1;
                language
            }
            kinodisc_hdmv::bdmv::PMT_STREAM_TYPE_PGS => {
                let language = subtitles
                    .get(subtitle_index)
                    .map(|subtitle| normalize_language(subtitle.language));
                subtitle_index += 1;
                language
            }
            _ => None,
        };
        streams.push(NativeStream {
            pid: stream.pid,
            stream_type: stream.stream_type,
            language,
            video_attr,
            video_aspect,
            audio_attr: stream.audio_attr,
        });
    }
    Ok(streams)
}

fn resolve_menu_streams(
    path: &Path,
    video_attr: u8,
    video_aspect: u8,
) -> Result<Vec<NativeStream>, AvchdError> {
    let prefix = read_pmt_prefix(path)?;
    let streams = parse_pmt_from_m2ts_prefix(&prefix).map_err(native_error)?;
    Ok(streams
        .into_iter()
        .map(|stream| {
            stream
                .with_video_attr(video_attr)
                .with_video_aspect(video_aspect)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_prefers_native_on_32_bit_and_without_tsmuxer() {
        assert_eq!(MuxBackend::Auto.select(true, 32), MuxBackend::Native);
        assert_eq!(MuxBackend::Auto.select(false, 64), MuxBackend::Native);
        assert_eq!(MuxBackend::Auto.select(true, 64), MuxBackend::TsMuxer);
        assert_eq!(MuxBackend::Native.select(true, 64), MuxBackend::Native);
        assert_eq!(MuxBackend::TsMuxer.select(false, 32), MuxBackend::TsMuxer);
    }
}
