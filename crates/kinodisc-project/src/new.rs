//! A new, empty project: the disc settings, one root menu and no episodes.
//! The desktop app (`project_new`) and the CLI (`kinodisc-cli project new`)
//! create projects here, so both start from the same shell.

use kinodisc_domain::{
    AspectRatio, AssetId, AssetSource, AudioDefaults, AuthoringMode, AvchdResolution,
    AvchdVideoConfig, BudgetPolicy, BurnPreferences, CanvasSpec, DiscCapacity, DiscConfig,
    DvdVideoConfig, FrameRate, MediaAsset, MediaKind, MediaProbe, Menu, MenuBackground, MenuId,
    MenuPlacement, MenuRole, MenuTiming, PlayAllConfig, Project, ProjectId, SubpictureSpec,
    TvStandard, VideoConfig,
};

/// Creates a blank project. `now` is the RFC 3339 creation time and
/// `app_version` the version written in the file.
#[allow(clippy::too_many_arguments)]
pub fn new_project(
    name: String,
    standard: TvStandard,
    aspect: AspectRatio,
    mode: AuthoringMode,
    avchd_resolution: Option<AvchdResolution>,
    avchd_frame_rate: Option<FrameRate>,
    now: &str,
    app_version: &str,
) -> Result<Project, String> {
    let now = now.to_string();
    let project_id = ProjectId::new();
    let volume_label = sanitize_volume_label(&name);

    let (disc, assets, menus, root_menu, budget) = match mode {
        AuthoringMode::DvdVideo => {
            let menu_id = MenuId::new();
            let background_id = AssetId::new();
            let canvas = CanvasSpec::new(standard, aspect);
            let background = MediaAsset {
                id: background_id,
                display_name: "background.png".to_string(),
                source: AssetSource::Project,
                relative_path: Some("assets/background.png".to_string()),
                sha256: "0".repeat(64),
                bytes: 0,
                kind: MediaKind::Image,
                probe: MediaProbe {
                    format_name: "png".to_string(),
                    duration_ms: None,
                    size_bytes: None,
                    bitrate_bps: None,
                    video: None,
                    audio: Vec::new(),
                    subtitle_streams: 0,
                    subtitle_tracks: Vec::new(),
                    chapters: Vec::new(),
                },
                proxy: None,
                imported_at: now.clone(),
            };
            let menu = Menu {
                id: menu_id,
                name: "Menú principal".to_string(),
                role: MenuRole::Root,
                placement: MenuPlacement::Vmgm,
                canvas,
                background: MenuBackground::Static {
                    asset: background_id,
                },
                scale_mode: kinodisc_domain::MenuScaleMode::Cover,
                dim_percent: 0,
                blur_px: 0,
                audio: None,
                motion_config: None,
                items: Vec::new(),
                buttons: Vec::new(),
                subpicture: SubpictureSpec::default(),
                timing: MenuTiming::default(),
                template: None,
                transition: None,
            };
            (
                DiscConfig {
                    mode: AuthoringMode::DvdVideo,
                    standard,
                    aspect,
                    capacity: DiscCapacity::Dvd5,
                    volume_label: volume_label.clone(),
                    region_mask: 0,
                    layer_break_mode: Default::default(),
                    video: VideoConfig::Dvd(DvdVideoConfig::default()),
                    audio: AudioDefaults::default(),
                    player_preset: Default::default(),
                },
                vec![background],
                vec![menu],
                menu_id,
                BudgetPolicy::default(),
            )
        }
        AuthoringMode::Avchd => {
            let resolution = avchd_resolution.unwrap_or(AvchdResolution::R720p);
            let frame_rate = avchd_frame_rate.unwrap_or_else(|| default_avchd_fps(resolution));
            let mut config = AvchdVideoConfig {
                resolution,
                frame_rate,
                ..AvchdVideoConfig::default()
            };
            config.level = config.effective_level();
            let errors: Vec<String> = config
                .validate()
                .into_iter()
                .filter(kinodisc_domain::Diagnostic::is_error)
                .map(|diagnostic| diagnostic.to_string())
                .collect();
            if !errors.is_empty() {
                return Err(errors.join("; "));
            }
            let effective_standard = avchd_standard(resolution, frame_rate);
            // HD is always 16:9; 480i/576i follow the request like a DVD
            // (the encoder sets the sample aspect ratio).
            let effective_aspect = if matches!(
                resolution,
                AvchdResolution::Sd480i | AvchdResolution::Sd576i
            ) {
                aspect
            } else {
                AspectRatio::SixteenNine
            };
            let menu_id = MenuId::new();
            let background_id = AssetId::new();
            let background = MediaAsset {
                id: background_id,
                display_name: "background.png".to_string(),
                source: AssetSource::Project,
                relative_path: Some("assets/background.png".to_string()),
                sha256: "0".repeat(64),
                bytes: 0,
                kind: MediaKind::Image,
                probe: MediaProbe {
                    format_name: "png".to_string(),
                    duration_ms: None,
                    size_bytes: None,
                    bitrate_bps: None,
                    video: None,
                    audio: Vec::new(),
                    subtitle_streams: 0,
                    subtitle_tracks: Vec::new(),
                    chapters: Vec::new(),
                },
                proxy: None,
                imported_at: now.clone(),
            };
            let menu = Menu {
                id: menu_id,
                name: "Menú principal".to_string(),
                role: MenuRole::Root,
                placement: MenuPlacement::Vmgm,
                canvas: match resolution.dimensions() {
                    (width @ (1920 | 1280), height) => {
                        CanvasSpec::hd(width as u16, height as u16, effective_standard)
                    }
                    _ => CanvasSpec::new(effective_standard, effective_aspect),
                },
                background: MenuBackground::Static {
                    asset: background_id,
                },
                scale_mode: kinodisc_domain::MenuScaleMode::Cover,
                dim_percent: 0,
                blur_px: 0,
                audio: None,
                motion_config: None,
                items: Vec::new(),
                buttons: Vec::new(),
                subpicture: SubpictureSpec::default(),
                timing: MenuTiming::default(),
                template: None,
                transition: None,
            };
            (
                DiscConfig {
                    mode: AuthoringMode::Avchd,
                    standard: effective_standard,
                    aspect: effective_aspect,
                    capacity: DiscCapacity::Dvd5,
                    volume_label,
                    region_mask: 0,
                    layer_break_mode: Default::default(),
                    video: VideoConfig::Avchd(config),
                    audio: AudioDefaults::default(),
                    player_preset: Default::default(),
                },
                vec![background],
                vec![menu],
                menu_id,
                BudgetPolicy {
                    header_margin_ppm: 30_000,
                    mux_overhead_ppm: 50_000,
                    min_video_kbps: 1_000,
                    // AVCHD on DVD media (the planner caps it there too).
                    max_video_kbps: 18_000,
                    ..BudgetPolicy::default()
                },
            )
        }
    };

    Ok(Project {
        schema_version: 1,
        id: project_id,
        name,
        created_at: now.clone(),
        modified_at: now,
        app_version: app_version.to_string(),
        disc,
        assets,
        first_play: None,
        menus,
        root_menu,
        episodes: Vec::new(),
        play_all: PlayAllConfig::default(),
        budget,
        burn: BurnPreferences::default(),
    })
}

fn default_avchd_fps(resolution: AvchdResolution) -> FrameRate {
    match resolution {
        AvchdResolution::R1080p | AvchdResolution::R720p => FrameRate::FPS_23_976,
        AvchdResolution::R1080i | AvchdResolution::Sd480i => FrameRate::FPS_29_97,
        AvchdResolution::Sd576i => FrameRate::PAL,
    }
}

fn avchd_standard(resolution: AvchdResolution, frame_rate: FrameRate) -> TvStandard {
    match resolution {
        AvchdResolution::Sd480i => TvStandard::Ntsc,
        AvchdResolution::Sd576i => TvStandard::Pal,
        _ if frame_rate == FrameRate::PAL || frame_rate == FrameRate::FPS_50 => TvStandard::Pal,
        _ => TvStandard::Ntsc,
    }
}

/// Disc volume label from the project name: ASCII capitals, digits and "_"
/// (the ISO 9660/UDF safe set). Accented Latin letters keep their base
/// letter ("Serie Ñandú" → "SERIE_NANDU"; it used to be "SERIE__AND"), and
/// any other run of characters becomes one "_".
pub fn sanitize_volume_label(name: &str) -> String {
    let mut label = String::new();
    for character in name.chars() {
        let plain = ascii_letter(character);
        if plain.is_empty() {
            if !label.ends_with('_') {
                label.push('_');
            }
        } else {
            label.push_str(&plain.to_ascii_uppercase());
        }
    }
    let mut label = label.trim_matches('_').to_string();
    if label.is_empty() {
        label = "KINODISC".to_string();
    }
    label.truncate(32);
    label.trim_end_matches('_').to_string()
}

/// The ASCII spelling of a letter or digit (accents dropped), or "" for
/// anything else.
fn ascii_letter(character: char) -> String {
    if character.is_ascii_alphanumeric() {
        return character.to_string();
    }
    let plain = match character.to_lowercase().next().unwrap_or(character) {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' | 'ą' => "a",
        'é' | 'è' | 'ê' | 'ë' | 'ē' | 'ę' | 'ě' => "e",
        'í' | 'ì' | 'î' | 'ï' | 'ī' => "i",
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' | 'ō' | 'ő' => "o",
        'ú' | 'ù' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => "u",
        'ý' | 'ÿ' => "y",
        'ñ' | 'ń' | 'ň' => "n",
        'ç' | 'ć' | 'č' => "c",
        'ś' | 'š' | 'ş' => "s",
        'ź' | 'ż' | 'ž' => "z",
        'ł' => "l",
        'ř' => "r",
        'ď' => "d",
        'ť' => "t",
        'ğ' => "g",
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        _ => "",
    };
    plain.to_string()
}
