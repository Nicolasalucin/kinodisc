//! Adding media to a project outside the desktop app (the CLI `project`
//! command). Mirrors the editor's `addAsset` / `addEpisodeFromAsset`
//! (`apps/desktop/src/lib/stores/project.svelte.ts`): the same episode
//! defaults, one audio track per source stream, the source's chapters and
//! subtitles. Keep the two in sync.

use kinodisc_domain::{
    AssetId, AssetSource, Chapter, ChapterId, Episode, EpisodeAudio, EpisodeId, FrameRate,
    MediaAsset, MediaKind, MediaProbe, Project, SubtitleTrack, VideoConfig,
};
use std::path::Path;

/// The disc frame rate chapter frames are counted in.
pub fn disc_frame_rate(project: &Project) -> FrameRate {
    match &project.disc.video {
        VideoConfig::Avchd(config) => config.frame_rate,
        VideoConfig::Dvd(_) => project.disc.standard.frame_rate(),
    }
}

/// Adds `path` (already probed) as an asset of `kind`, or refreshes the
/// asset that already points at that file. Returns its id.
pub fn add_asset(
    project: &mut Project,
    path: &Path,
    probe: MediaProbe,
    kind: MediaKind,
    now: &str,
) -> AssetId {
    let absolute = path.display().to_string();
    let same = |other: &str| {
        if cfg!(windows) {
            other.eq_ignore_ascii_case(&absolute)
        } else {
            other == absolute
        }
    };
    if let Some(existing) = project.assets.iter_mut().find(|asset| {
        asset.kind == kind
            && matches!(&asset.source, AssetSource::External { absolute_path } if same(absolute_path))
    }) {
        existing.probe = probe;
        return existing.id;
    }
    let id = AssetId::new();
    project.assets.push(MediaAsset {
        id,
        display_name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| absolute.clone()),
        source: AssetSource::External {
            absolute_path: absolute,
        },
        relative_path: None,
        sha256: "0".repeat(64),
        bytes: probe.size_bytes.unwrap_or(0),
        kind,
        probe,
        proxy: None,
        imported_at: now.to_string(),
    });
    id
}

/// Adds an episode for the video asset `asset` (numbered after the last
/// one, titled after the file) and returns its id.
pub fn add_episode(project: &mut Project, asset: AssetId) -> Result<EpisodeId, String> {
    let asset = project
        .asset(asset)
        .filter(|asset| asset.kind == MediaKind::Video)
        .ok_or_else(|| "El archivo no es un vídeo del proyecto.".to_string())?
        .clone();
    let number = project
        .episodes
        .iter()
        .map(|episode| episode.number)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let stem = Path::new(&asset.display_name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let title = if stem.trim().is_empty() {
        format!("Episodio {number}")
    } else {
        stem
    };
    let fps = disc_frame_rate(project).as_f64().max(1.0);
    let mut chapters = vec![Chapter {
        id: ChapterId::new(),
        title: "Inicio".to_string(),
        at_frame: 0,
        thumbnail: None,
    }];
    for (index, chapter) in asset.probe.chapters.iter().enumerate() {
        let at_frame = (chapter.start_ms as f64 / 1000.0 * fps).round() as u64;
        if at_frame == 0 || chapters.iter().any(|known| known.at_frame == at_frame) {
            continue;
        }
        let title = chapter.title.trim();
        chapters.push(Chapter {
            id: ChapterId::new(),
            title: if title.is_empty() {
                format!("Capítulo {}", index + 1)
            } else {
                title.to_string()
            },
            at_frame,
            thumbnail: None,
        });
    }
    chapters.sort_by_key(|chapter| chapter.at_frame);
    // Built through serde so every field the model adds keeps its default.
    let audio = asset
        .probe
        .audio
        .iter()
        .enumerate()
        .map(|(index, stream)| {
            let label = stream
                .title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if stream.commentary {
                        format!("Comentario {}", index + 1)
                    } else {
                        format!("Pista {}", index + 1)
                    }
                });
            serde_json::from_value::<EpisodeAudio>(serde_json::json!({
                "sourceStream": stream.index,
                "language": stream.language.clone().unwrap_or_else(|| "und".to_string()),
                "label": label,
                "ac3Kbps": 192,
                "format": if stream.channels >= 6 { "ac3Surround51" } else { "ac3Stereo" },
                // The disc plays the first track first.
                "default": index == 0,
                "commentary": stream.commentary,
            }))
            .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let subtitle_default = asset
        .probe
        .subtitle_tracks
        .iter()
        .position(|stream| stream.default);
    let subtitles = asset
        .probe
        .subtitle_tracks
        .iter()
        .enumerate()
        .map(|(index, stream)| {
            let label = stream
                .title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("Subtítulo {}", index + 1));
            serde_json::from_value::<SubtitleTrack>(serde_json::json!({
                "sourceStream": stream.index,
                "language": stream.language.clone().unwrap_or_else(|| "und".to_string()),
                "label": label,
                "isForced": stream.forced,
                "isDefault": subtitle_default == Some(index),
                "bitmap": stream.bitmap,
            }))
            .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let id = EpisodeId::new();
    let mut episode: Episode = serde_json::from_value(serde_json::json!({
        "id": id,
        "number": number,
        "title": title,
        "video": asset.id,
        "scaleMode": "contain",
        // A series goes on: the next-chapter key and the end of an episode
        // reach the next one (as in the editor).
        "postAction": "nextEpisode",
    }))
    .map_err(|error| error.to_string())?;
    episode.chapters = chapters;
    episode.audio = audio;
    episode.subtitles = subtitles;
    project.episodes.push(episode);
    sync_play_all(project);
    Ok(id)
}

/// Text subtitle files the disc can carry (the editor's «+ .srt»).
pub const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "ass", "ssa", "vtt"];

/// The language in a subtitle file name: `Episodio 01.spa.srt` → `spa`.
pub fn subtitle_language(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let (_, code) = stem.rsplit_once('.')?;
    (matches!(code.len(), 2 | 3) && code.chars().all(|c| c.is_ascii_alphabetic()))
        .then(|| code.to_ascii_lowercase())
}

/// Whether `subtitle` belongs to `episode` by name: the subtitle file name
/// starts with the episode video's name (`Ep 01.mkv` ↔ `Ep 01.spa.srt`).
pub fn subtitle_matches_episode(project: &Project, episode: &Episode, subtitle: &Path) -> bool {
    let lower = |path: &Path| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().to_lowercase())
    };
    let Some(video) = project
        .asset(episode.video)
        .and_then(|asset| lower(Path::new(&asset.display_name)))
    else {
        return false;
    };
    let Some(name) = lower(subtitle) else {
        return false;
    };
    name == video
        || name
            .strip_prefix(&video)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Adds an external text subtitle to an episode, like the editor: the first
/// track is the default one, the label is the file name.
pub fn add_subtitle_file(
    project: &mut Project,
    episode: EpisodeId,
    path: &Path,
    language: &str,
) -> Result<(), String> {
    let episode = project
        .episodes
        .iter_mut()
        .find(|item| item.id == episode)
        .ok_or_else(|| "Ese episodio no existe.".to_string())?;
    let label = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("Subtítulo {}", episode.subtitles.len() + 1));
    let track = serde_json::from_value::<SubtitleTrack>(serde_json::json!({
        "sourceStream": 0,
        "sourcePath": path.display().to_string(),
        "language": language,
        "label": label,
        "isDefault": episode.subtitles.is_empty(),
    }))
    .map_err(|error| error.to_string())?;
    episode.subtitles.push(track);
    Ok(())
}

/// Play All follows every episode in disc order and is on when a menu has
/// a Play All button (the editor's `syncPlayAll`).
pub fn sync_play_all(project: &mut Project) {
    project.play_all.order = project.episodes.iter().map(|episode| episode.id).collect();
    project.play_all.enabled = project.menus.iter().any(|menu| {
        menu.buttons
            .iter()
            .any(|button| matches!(button.action, kinodisc_domain::ButtonAction::PlayAll))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use kinodisc_domain::{AspectRatio, AuthoringMode, TvStandard};

    fn probe(audio_channels: &[u16]) -> MediaProbe {
        serde_json::from_value(serde_json::json!({
            "formatName": "matroska",
            "durationMs": 1_320_000,
            "sizeBytes": 123,
            "video": {
                "index": 0, "codec": "h264", "width": 720, "height": 480,
                "frameRate": { "num": 30000, "den": 1001 }
            },
            "audio": audio_channels.iter().enumerate().map(|(index, channels)| serde_json::json!({
                "index": index + 1, "codec": "aac", "channels": channels,
                "sampleRate": 48000, "language": if index == 0 { "jpn" } else { "spa" },
                "default": index == 0, "commentary": false
            })).collect::<Vec<_>>(),
            "subtitleStreams": 0,
            "chapters": [
                { "startMs": 0, "title": "" },
                { "startMs": 600_000, "title": "Parte B" }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn subtitle_files_find_their_episode_and_language() {
        let mut project = crate::new::new_project(
            "Serie".to_string(),
            TvStandard::Ntsc,
            AspectRatio::SixteenNine,
            AuthoringMode::DvdVideo,
            None,
            None,
            "2026-11-23T00:00:00Z",
            "1.3.0",
        )
        .unwrap();
        let path = std::env::temp_dir().join("Ep 01.mkv");
        let asset = add_asset(&mut project, &path, probe(&[2]), MediaKind::Video, "now");
        let id = add_episode(&mut project, asset).unwrap();
        let episode = project.episode(id).unwrap().clone();
        assert!(subtitle_matches_episode(
            &project,
            &episode,
            Path::new("Ep 01.spa.srt")
        ));
        assert!(subtitle_matches_episode(
            &project,
            &episode,
            Path::new("ep 01.srt")
        ));
        assert!(!subtitle_matches_episode(
            &project,
            &episode,
            Path::new("Ep 010.srt")
        ));
        assert_eq!(
            subtitle_language(Path::new("Ep 01.spa.srt")).as_deref(),
            Some("spa")
        );
        assert_eq!(subtitle_language(Path::new("Ep 01.srt")), None);
        add_subtitle_file(&mut project, id, Path::new("/x/Ep 01.spa.srt"), "spa").unwrap();
        add_subtitle_file(&mut project, id, Path::new("/x/Ep 01.eng.srt"), "eng").unwrap();
        let subtitles = &project.episode(id).unwrap().subtitles;
        assert_eq!(subtitles.len(), 2);
        assert!(subtitles[0].is_default && !subtitles[1].is_default);
        assert_eq!(
            subtitles[0].source_path.as_deref(),
            Some("/x/Ep 01.spa.srt")
        );
    }

    #[test]
    fn episodes_get_the_editor_defaults() {
        let mut project = crate::new::new_project(
            "Serie".to_string(),
            TvStandard::Ntsc,
            AspectRatio::SixteenNine,
            AuthoringMode::DvdVideo,
            None,
            None,
            "2026-11-22T00:00:00Z",
            "1.3.0",
        )
        .unwrap();
        let path = std::env::temp_dir().join("Episodio 01.mkv");
        let asset = add_asset(&mut project, &path, probe(&[2, 6]), MediaKind::Video, "now");
        // The same file again reuses the asset.
        assert_eq!(
            add_asset(&mut project, &path, probe(&[2, 6]), MediaKind::Video, "now"),
            asset
        );
        let first = add_episode(&mut project, asset).unwrap();
        let second = add_episode(&mut project, asset).unwrap();
        let episode = project.episode(first).unwrap();
        assert_eq!((episode.number, episode.title.as_str()), (1, "Episodio 01"));
        assert_eq!(project.episode(second).unwrap().number, 2);
        assert_eq!(episode.audio.len(), 2);
        assert!(episode.audio[0].default && !episode.audio[1].default);
        assert_eq!(episode.audio[0].language, "jpn");
        assert_eq!(
            episode.audio[1].format,
            kinodisc_domain::AudioFormat::Ac3Surround51
        );
        assert_eq!(episode.chapters.len(), 2, "Inicio + Parte B");
        assert_eq!(episode.chapters[1].title, "Parte B");
        assert_eq!(episode.chapters[1].at_frame, 17_982);
        assert_eq!(project.play_all.order, vec![first, second]);
        assert_eq!(
            episode.post_action,
            kinodisc_domain::PostAction::NextEpisode
        );
        assert!(project
            .validate()
            .iter()
            .all(|diagnostic| !diagnostic.is_error()));
    }
}
