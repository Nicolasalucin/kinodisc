# AGENTS.md — Kinodisc engineering guide

This file is the high-signal orientation for any AI coding agent working in this
repository. Read it before changing code. The project ships optical disc images;
several invariants are hardware- or format-critical and are not obvious from a
single crate.

## What Kinodisc is

Kinodisc is a Rust + Tauri v2 + Svelte 5 optical authoring studio for:

- **DVD-Video** (DVD-5/DVD-9, MPEG-2, AC-3, DVDAuthor/Spumux, DVD-9 layer break).
- **AVCHD** (H.264 on DVD media, HDMV Interactive Graphics menus).
- **Blu-ray** (BD-25/BD-50, UDF 2.50 metadata partition, HDMV menus).

The desktop app and CLI share the same render pipeline in `kinodisc-render`.

> Roadmap: [ROADMAP.md](ROADMAP.md) is the authoritative v0.6.0–v1.0.0 plan.
> New check IDs and diagnostic codes proposed there are reserved before
> implementation; keep this file's invariants and the roadmap in sync.

## Repository map

```text
crates/
  kinodisc-domain/       Pure models, validation, geometry, menus, diagnostics
  kinodisc-project/      Project JSON load/save/validate (schema v1)
  kinodisc-media/        FFprobe adapter: streams, tags, chapters, dispositions
  kinodisc-menu/         SPU/menu layer rasterization, templates, Spumux/TextSub XML
  kinodisc-subtitle/     Native SRT/ASS/VTT parsing, ab_glyph rasterizer, PGS/SPU
  kinodisc-plan/         Bitrate planning, DVD-9 ECC layer-break sector math
  kinodisc-dvd/          MPEG-2/AC-3 encoding, DVDAuthor XML, IFO patching
  kinodisc-avchd/        H.264/AC-3 encoding, tsMuxeR metadata, native BDMV muxer
  kinodisc-hdmv/         HDMV IG segments, opcodes, native M2TS/MPLS/CLPI/MovieObject
  kinodisc-iso/          DVD-Video ISO builder and BDMV UDF 2.50 builder
  kinodisc-burn/         Platform burners: growisofs/xorriso, camcontrol, IMAPI2, drutil
  kinodisc-jobs/         Job events, stages, diagnostics-facing types
  kinodisc-toolchain/    Binary discovery, tool doctor, hardware encoder probes
  kinodisc-store/        SQLite recent projects, job history, artifact cache v2
  kinodisc-testkit/      Shared test helpers
  kinodisc-cli/          CLI: doctor, probe, plan, render, verify, devices, burn
  kinodisc-render/       End-to-end DVD/AVCHD render orchestration and cache
  kinodisc-verify/       Independent read-only UDF/BDMV/DVD structural verifier

vendor/libmkiso/         Vendored UDF/ISO writer+reader (upstream 0.1.5 + patches)
vendor/libmkiso/KINODISC-PATCH.md
                         Local patch rationale; read before touching vendor code

apps/desktop/            Svelte 5 + Vite + Tailwind frontend
apps/desktop/src-tauri/  Tauri v2 commands, Specta bindings, platform setup
```

Generated IPC bindings live at:

```text
apps/desktop/src/lib/api/bindings.ts
```

Do not edit that file manually. Regenerate it with `npm run bindings` (or
`cargo run -p kinodisc-desktop --example export-bindings`; it is an example so the
installer never ships it).

## Critical invariants — do not break

### 1. `vendor/libmkiso`: 64-bit sectors, Tag 266, UDF layout

- All ISO/UDF sector and extent arithmetic that can exceed 4 GiB must use
  `u64`/`u128`; never `usize` or `u32`. The CLI and authoring stack are compiled
  for **i686** and must handle BD-25/BD-50 and >4 GiB M2TS files.
- UDF 2.50 metadata files must use **Extended File Entries (Tag 266)** with long
  allocation descriptors. The local patch is documented in
  `vendor/libmkiso/KINODISC-PATCH.md`; the rest is upstream 0.1.5.
- UDF images must keep the trailing AVDPs within the final 32-sector/64 KiB
  boundary, and metadata mirrors must remain readable.
- Do not replace the reader with the writer for validation. The verifier needs
  an independent parse.
- Run `vendor/libmkiso` tests from inside `vendor/libmkiso`; it is intentionally
  excluded from the workspace.

### 2. `kinodisc-render`: MPEG-2 flags and cache determinism

The DVD-Video software encode path is tuned deliberately:

- Scaling uses `flags=lanczos` and resamples the picture ONCE, straight to
  the stored frame (`kinodisc_toolchain::fit_filter`, shared with AVCHD).
  Never fit on a square-pixel canvas and squeeze afterwards: that left a
  16:9 picture 719 pixels wide with a black column. On one film excerpt
  that cost about 15 VMAF, and scored against a reference placed the same
  way the loss disappeared (QA `rel-141`, `cmp-141`): it was geometry, not
  sharpness, and it explains the VMAF and SSIM gap to DVDStyler of `m-132`.
- RD/Trellis: pass 2 uses `-mbd rd -cmp satd -subcmp satd -trellis 2` (SATD
  comparison measured equal to RD comparison at half the time). Pass 1 is
  a light statistics pass: fast by default (`-mbd simple -b_strategy 0`, no
  `-cmp/-subcmp`, `-trellis 0`) and, with `DvdVideoConfig.full_first_pass`,
  the 1.3.0 one (`-mbd rd -b_strategy 2`). Always two passes: they give the
  bits to the hard scenes (QA 1.3.1: one pass lost 1.2 VMAF there). The fast
  pass joins the cache key as `:p1fast`. `qmin 1` is not a fixed setting: it
  moves bits from the hard scenes to the easy ones. It is used only when the
  first pass spends under 85 % of the asked bitrate (`SPEND_BELOW_PERCENT`,
  read from the pass log): then the first pass runs again and both passes
  use `-qmin 1 -lmin 118`, so easy video uses the room the disc has (QA
  `cmp-141`).
- Episodes with the same source bytes share one cached encode: the second
  title gets its own copy (`distinct_episode_file`), because DVDAuthor points
  titles that name one file at the same cells. Quality options kept after timing them:
  `-intra_vlc 1 -last_pred 2 -mpv_flags +mv0`; `qmin 1`, `dia_size` and
  `bidir_refine` were rejected (much slower, no visible gain).
- Interlaced output takes its field order from `setfield=tff/bff` in the
  filter chain; never `-top` (removed in FFmpeg 9).
- DC precision uses `-intra_dc_precision 0/1/2` (8/9/10 bits), not deprecated
  `-dc`.
- Closed GOP is mandatory: `-flags +cgop` and strict `-g 15` (PAL) / `-g 18`
  (NTSC). FFmpeg rejects closed GOP with scene-cut detection, so the code uses
  `-sc_threshold 1000000000`. Do not "restore" `-30000` without revalidating
  FFmpeg.
- Pass 1 writes to `NUL` on Windows and `/dev/null` on Unix; each `EncodeRequest`
  carries its own job-scoped `passlog` path.
- `ENCODER_REV` must be bumped when effective encoder arguments change, and
  `EpisodeRenderInputs`/`inputs_hash` must include every render-affecting field.

Smart Render Cache v2:

- `ArtifactKey` fields: `kind`, `source_fingerprint`, `inputs_hash`,
  `target_profile`, `target_bitrate`, `audio_params_hash`, `encoder_rev`,
  `toolchain_hash`.
- `source_fingerprint` is `fp2:` + full streaming SHA-256. Do not reintroduce
  size/mtime/first-last-8-MiB fingerprints.
- `inputs_hash` is canonical serialized render inputs. Changing trim, fps,
  scale mode, codec args, audio, subtitles, menu composition or encoder choice
  must change the hash.
- Cache failures are non-fatal warnings; they must never fail a render.
- DVD/AVCHD artifacts are published through `cached_artifact_path`; menu cache
  hits must be materialized into the output tree before Spumux.

### 3. `kinodisc-dvd`: DVD-9 layer break and IFO consistency

- The planner computes an ECC-aligned layer-break sector (16 DVD sectors).
- Authoring must call `apply_layer_break` (or equivalent) and set the **SPLIP**
  bit on the containing VTS PGC cell.
- `VTS_01_0.IFO` and `VTS_01_0.BUP` must be byte-identical after patching.
  `VIDEO_TS.IFO`/`VIDEO_TS.BUP` must also remain identical.
- A DVD-9 build without a valid SPLIP cell is structurally broken; the verifier
  must fail it.
- Do not claim in the UI that the layer break is applied unless the authoring
  path actually does it.

### 4. `kinodisc-verify`: independent and read-only

- The verifier is a read-only parser. It must not reuse writer code paths for
  structural validation.
- It runs automatically after render and before burn. `Fail` blocks burning;
  `Warn` is surfaced to the user.
- Checks are stable IDs (`UDF-AVDP`, `UDF-TAG266`, `BDMV-MOBJ`, `DVD-PGC`,
  `DVD-LAYER-BREAK`, `BDMV-PGS-DS`, `BDMV-PGS-TS`, `BDMV-AUDIO-STREAM`,
  `DVD-SPU-SEQ`, `DVD-SPU-PALETTE`, `BDMV-IG-BUTTONS`, `BDMV-IG-NAV`,
  `BDMV-MOTION-THUMB`, `DVD-SPU-BUTTONS`, etc.). Keep IDs stable for UI and tests.
- Do not add writes, temp files or mutation to the verifier.

### 5. Tauri/Svelte IPC

- Every `#[tauri::command]` must be registered in
  `apps/desktop/src-tauri/src/lib.rs` (`collect_commands!`).
- Add any new serialized types to the Specta `.typ::<...>()` list.
- Regenerate `bindings.ts` after every signature or type change.
- Never call `invoke()` directly from components; use the generated
  `commands.*` wrappers and `unwrap()`/`typedError`.
- All command errors are currently `String`; do not silently discard
  `Result`/`Option` in command implementations.
- `project_save` validates with `ensure_valid`; invalid projects must not be
  persisted by the normal save command.

### 6. Native subtitles and audio mastering

- Text subtitle authoring goes through `kinodisc-subtitle`; do not route ASS/VTT
  through the legacy TextSub XML. DVD uses four-color PNGs plus Spumux `<spu>`;
  Blu-ray/AVCHD uses generated PGS `.sup` files in the combined MKV.
- PGS and SPU buffers are bounded; render one 1080p bitmap at a time. Resolved
  font bytes and subtitle style are part of the render cache identity.
- `AudioMastering` (loudnorm/channel mode/codec/passthrough) and per-track gain,
  delay and format must feed one `-af`/`-filter:a:N` chain. Keep mastering
  fields in `audio_params_hash` and bump `ENCODER_REV` when arguments change.
- AVCHD audio is a vector end to end: FFmpeg per-stream args, tsMuxeR meta,
  native PMT/MPLS/CLPI and language registration must all agree. Pass-through
  is only legal for an unfiltered, codec-matching source; otherwise re-encode.
- Subtitle style is persisted on `SubtitleTrack.style`; native DVD/AVCHD
  rendering must use it rather than a global default.

### 7. Domain/project compatibility

- Project schema is version 1. Add new fields with `#[serde(default)]` and
  `specta::Type`; old projects must continue to load.
- `Project::validate` is authoritative: render, plan and save must use it.
- DVD-Video bitmap subtitles are metadata-only and rejected by the text
  pipeline with `W_BITMAP_SUBTITLE_UNSUPPORTED`. Do not feed PGS/VobSub to
  Spumux.
- Blu-ray/AVCHD `hdmv_pgs_subtitle` tracks are stream-copied and registered in
  MPLS/CLPI. Non-PGS bitmap codecs remain unsupported; PGS delay is ignored with
  `W_PGS_DELAY_IGNORED` and mixed episode layouts skip with
  `W_PGS_LAYOUT_MISMATCH`.
- HDMV DTS is kept equal to PTS until menu playback is validated on hardware.
- Menu clip timing fields (`out_ms`, `loop_duration_ms`) must be honored by the
  FFmpeg filter/argument builders.
- Template metadata (`Menu.template`) is additive; applying a template must
  never silently truncate episodes. Page 0 stays the VMGM root and later pages
  are VTSM EpisodeSelect menus with ordinary Prev/Next GotoMenu buttons.
- `Menu.transition` is a pre-rendered clip. It is concatenated before the menu
  loop; subpicture/IG start and menu audio are delayed to the transition end.
  Transition duration/easing and source SHA are part of the menu cache key. The
  current FFmpeg matrix has no fade-curve option, so the boundary fade is linear
  while easing metadata remains authoritative for future encoders.

### 8. HDMV

- IG segments use canonical PDS/WDS/ODS/ICS/END ordering.
- MovieObject commands are 12 bytes. `HdmvCommand::from_bytes`/`decode_command`
  must round-trip canonical commands.
- PGS slot estimation must account for width and height; `dummy_pgs_for_slots`
  measures a real dummy display set, not a worst-case pixel estimate.
- `BdmvIsoBuilder` must validate `index.bdmv`, `MovieObject.bdmv` and the
  playlist tree before publishing; the verifier does the independent deep check.

### 9. Bounded memory and 32-bit I/O

- No production path may `fs::read` an ISO, M2TS, VOB or other multi-gigabyte
  payload. Use the 256 KiB streaming helpers in `kinodisc-verify::stream`,
  `kinodisc-hdmv::inject_ig_into_menu_m2ts_file` or equivalent.
- Buffers are 64 KiB–1 MiB. `vendor/libmkiso` uses a 1 MiB payload/digest
  buffer and a file-backed `FileSource`; never add a full-image allocation.
- `i686` is a first-class target: sector/extent math uses `u64`/`u128`, packet
  counts use checked `u32` conversion, and the i686 check/test jobs must stay
  green. 32-bit Windows carries `/LARGEADDRESSAWARE` in
  `.cargo/config.toml`.

### 10. Native BDMV muxer and portable toolchain

- `kinodisc-hdmv::native` emits canonical INDX0200/MOBJ0200/MPLS0200/HDMV0200
  structures; keep its tests and the `kinodisc-verify` integration test green.
- tsMuxeR is not used (1.3.5, owner's decision). `MuxBackend::Auto` is the
  native muxer on every host, also when tsMuxeR is installed, and a native
  failure is an error: nothing is tried again with tsMuxeR. The doctor does
  not look for it. `KINODISC_MUXER=tsmuxer` asks for it by name: a switch for
  tests, not a feature. Its files stay the byte reference (section 22).
- Portable discovery checks `KINODISC_TOOLCHAIN_DIR`, `<exe>/toolchain/`,
  `<exe>/bin/` and platform-arch variants before resources and PATH.
- Native menu clips are encoded to **Matroska** (`menu.mkv`), not raw
  `.h264`: FFmpeg ≥ 9 refuses to stream-copy timestamp-less H.264 ES into
  MPEG-TS. tsMuxeR keeps the raw `.h264`. `NativeStream::video_attr` must come
  from `AvchdVideoConfig::bdmv_video_attr()` (format << 4 | rate).

### 12. Frame rates, stream copy and audio reconciliation

- 1080p 25/29.97 is **PsF** (`HdScan::Psf`): x264 `fake-interlaced=1:pic-struct=1`,
  CPU only, signalled as 1080i in BDMV. 1080p 50/59.94 forces Level 4.2.
- `kinodisc-plan::stream_copy::avchd_copy_decision` is the single authority
  for Smart Stream Copy; unknown probe data means re-encode. A copy converts
  nothing and is not retagged: a stream that names a colour matrix other than
  the disc's is re-encoded (`COPY_COLOUR`), and one that names none stays
  untagged on the disc. The render reads
  the decision from the plan (`EpisodeBitrate.stream_copy`) and includes it in
  the cache `profile`.
- `reconcile_episode_audio` and `harmonize_avchd_audio` run before every
  encode; never map an audio stream index the probe does not list.

### 14. Bundles, batch and provisioning

- `kinodisc-project::bundle` is the only bundle reader/writer: manifest
  first, deterministic headers (mtime 0, uid/gid 0, mode 0644), streamed
  hashing, traversal-safe import with full rollback. Never trust an entry
  that is not in the manifest.
- `kinodisc-cli batch` reuses `render::render`; keep concurrency 1.
- `kinodisc-toolchain::provision::DOWNLOADS` entries must carry the exact
  size and SHA-256 published for the asset; never extract or run anything
  before the checksum matches. The per-user toolchain folder is searched
  right after `KINODISC_TOOLCHAIN_DIR`.

### 15. Native DVD ISO, tool builds and accessibility

- `kinodisc-iso::dvd_native` writes the DVD-Video image; keep the
  canonical contiguous file order (IFO sector offsets depend on it) and
  validate with `kinodisc-verify` plus the dual-reader test. Never put
  anything but DVD-Video files in VIDEO_TS.
- `.github/workflows/build-dvdauthor.yml` builds the Windows DVDAuthor
  from the checksummed 0.7.2 tarball; changing it changes the zip SHA-256,
  so update `provision::DOWNLOADS` together with a new pre-release.
- Header dialogs must not live under an element with `backdrop-filter`
  (it becomes their containing block). Secondary screens and non-es/en
  locales are lazy; keep start-up JavaScript under 500 kB.

### 16. Kinodisc 1.0 additions

- **Upgrade compatibility** lives in `kinodisc-toolchain::legacy`: read
  environment variables with `legacy::env_var("KINODISC_X")` (it falls back
  to `DVDFORGE_X`/`DVD_FORGE_X`) and get the per-user folder from
  `legacy::app_data_dir()` (migrates `org.dvdforge.desktop` once). Never read
  `std::env::var("KINODISC_…")` directly.
- **Tool trust**: provisioned tools carry per-file SHA-256 in their
  provenance record; `provision::is_trusted_binary` refuses changed/unknown
  files and planted libraries in the per-user tool folder. Tool search must
  never include the current working directory.
- **Design system**: `apps/desktop/src/theme.css` is generated (light/dark
  remap of the slate/cyan/status palettes); components use the `ui-*` classes
  in `app.css`. `slate-950` is the window background and `slate-50` the
  strongest text in both themes; text over video or black overlays must be
  `text-[#fff]` (`text-white` inverts in light mode). Dialogs need
  `role="dialog" aria-modal="true"` (editor shortcuts rely on it).
- **Menu items** (`menu.items`) are drawn by `editor/menuItems.ts` for the
  disc export and by `layers/ItemsLayer.ts` in the editor; keep them in sync.
  Generated ids starting with `tpl:` belong to templates/splits and may be
  replaced.
- **HDMV per-episode playback**: episode MovieObjects start at
  `base + menus`, play their own range playlist from `EPISODE_MARK_GPR` (1)
  and clear it before jumping. MPLS mark tables are 14 bytes per mark
  (`BDMV-MARKS` verifies it).
- **DVD episode encoding** runs `encode_concurrency()` jobs in parallel;
  everything after encoding (cache publish, subtitle mux) stays in disc order.
- New CLI commands share their modules with the app: `split`
  (`kinodisc-render::split`), `scenes` (`kinodisc-menu::chapters`),
  `slideshow` (`kinodisc-render::slideshow`), `inspect`
  (`kinodisc-verify::inspect`, read-only parsers, bounds-checked) and
  `render --folder` (`kinodisc-verify::extract`).

### 17. Kinodisc 1.1 additions

- **Picture fixes** (`Episode.filters`, serde default) go through
  `kinodisc-render::picture` only: the same chain for DVD, AVCHD/BD, the
  preview (`media_filter_preview`) and the quality sample. It is part of the
  episode cache key only when not neutral, and disables stream copy
  (`COPY_FILTERS`). Order: deinterlace → crop → rotate/mirror → denoise →
  colour → blurred sides.
- **Menu artwork without the editor**: `kinodisc-render::compose` draws the
  menus for CLI/batch renders (mirrors `buttonLayout.ts`/`exportMenu.ts`).
  Keep the two in sync. The menu cache key includes the composed PNG.
- **Templates** paint `tpl:bg` (removed when the user picks a background) and
  `tpl:decor-*` shapes; titles use `title_color`. Template names are shown
  translated; never use third-party names or trademarks for templates.
- **Join** (`kinodisc-render::join`) passes `-reinit_filter 0` per input and
  has a stall timeout; keep both. Join and slideshow intermediates are
  limited-range `yuv420p` (`scale=…:out_range=tv` plus `-color_range tv`):
  JPEG/MJPEG input is full range and would otherwise turn off stream copy.
- **Quality sample** (`kinodisc-render::sample`) must build its requests like
  the real render and pick compare frames by number (raw `.m2v` has no
  B-frame timestamps).
- **Network**: the only requests are tool downloads and the daily update
  check (`kinodisc-toolchain::update`, official repo only, nothing sent);
  `open_release_page` opens only official release URLs.
- **Writes chosen by the user**: `image_save_png` writes absolute `.png`
  paths with PNG data only; `system_shutdown` runs fixed arguments through
  absolute system paths.
- `cargo test` must never touch the user's data folder: tests that call
  commands set `XDG_DATA_HOME` to a temporary folder.
- **Production output** (`render_start`): the render works in
  `<output>/.kinodisc-work/<job>` and removes it when it ends (finished,
  failed or cancelled); only the image, named by the caller (`iso_name`,
  one `.iso` file name), is left in the chosen folder. Stale work folders
  older than 6 h are removed on the next render there.
- **QA walkthroughs** run with their own `XDG_DATA_HOME`,
  `KINODISC_CACHE_DIR` and `WEBVIEW2_USER_DATA_FOLDER`, so they never touch
  the user's projects, caches or browser storage.

### 18. Kinodisc 1.2 additions

- **Auto-play** (`MenuTiming.auto_play`, serde default): root menu only,
  DVD-Video only. The root VMGM PGC `<post>` becomes the Play All command.
- **Even volume** is `AudioMastering.loudness` on every track
  (`projectStore.setEvenVolume`); a levelled track is never passed through.
- **Data disc**: `kinodisc-iso::build_data_iso` writes UDF 1.02 from a host
  tree and refuses an output inside the source. Commands:
  `data_folder_size`, `data_iso_create`, `data_iso_cancel`. Nothing in the
  folder is ever executed.
- **AVCHD/Blu-ray audio layout** comes from the episode with the most tracks;
  soundless episodes get silent tracks (`AvchdAudioConfig.silent`;
  `AudioEncodeConfig.silent` on DVD). `Project::avchd_subtitle_layout` is the
  single rule for HD subtitles and is part of the episode cache profile
  (`"pgs"`/`"nopgs"`).
- **Interlaced and PsF HD** never use a hardware encoder.
- **AC-3 bitrates**: one rule, `kinodisc_domain::ac3_bitrate_supported`
  (`AC3_BITRATES_KBPS`, 64–448; 5.1 needs 192+), used by the DVD and AVCHD
  encoders and by `Project::validate` (`E_AC3_KBPS`, skips passthrough,
  counts channels with `EpisodeAudio::encoded_channels`). The UI list is
  `apps/desktop/src/lib/utils/audio.ts`.
- **x264 preset** comes from `EncoderPreference::x264_preset()` (`CpuFast`
  fast, `CpuSlow` slow, everything else medium). It joins the episode cache
  profile only when it is not `medium`, so older caches stay valid. The disc
  size never depends on the encoder choice.
- **Production output**: `render_start` works in
  `<output>/.kinodisc-work/<job>` and publishes only the named image (see
  section 17).
- **QA before every release** uses the isolated launch (see section 17).
- `ButtonAction::SetSubtitleTrack { track: Option<u8> }` (`None` = off): DVD
  `subtitle=N+64` / `subtitle=0`, HDMV `SetStream`. `kinodisc-menu::setup`
  generates the "Ajustes" page and its root link; it is recognised on re-run
  by structure (a non-root submenu whose buttons are only SetAudioTrack /
  SetSubtitleTrack / GotoMenu root), like the scene menus.
- With a subtitle menu on a DVD, the per-title default subtitle `<pre>` is
  guarded by a choice register (g2, or g3 when Play All uses g2) that the
  menu sets, so "Sin subtítulos" is not undone at each title.
- `publish_iso` replaces an earlier image of the same name (the app asks first,
  `render_iso_exists`; the project was produced again); when that image cannot be replaced it publishes "<name> (2).iso" … "(9).iso". Folder mode renders
  to `<name>.kinodisc-tmp.iso`; the export clears only `VIDEO_TS` / `BDMV`.
- A soundless episode makes the whole AVCHD/Blu-ray audio layout AC-3 (no
  pass-through), so the silent tracks can match it.
- Dialogs doing work set `ui.locked`; `ui.open()` does nothing while it is set.
- Deleting asks with the awaited native confirm (`utils/confirm.ts`);
  `window.confirm` does not block in the webview.

### 19. Kinodisc 1.2.1 additions

- **Erase**: `DiscBurner::erase(device, full)` (default: unsupported).
  Windows `IDiscFormat2Erase` after `IsCurrentMediaSupported`; Linux and
  FreeBSD `xorriso -outdev <dev> -blank fast|all`; macOS
  `drutil -drive <sel> erase quick|full`. The `burn_erase` command takes
  the burn slot and refuses media that `probe_media` does not report as
  rewritable.
- **IMAPI errors** keep their HRESULT: `map_com` writes
  `"<reason> (código 0xC0AA….)"` with `windows::imapi_reason`. The frontend
  (`i18n/burnErrors.ts`) matches the code first, then engine phrases, and
  shows reason + advice; keep both tables in sync.
- **Diagnostics are local**: "Copy details" builds the report in the page;
  there is no telemetry upload (see section 17, Network).
- `kinodisc-cli template <project> <name>` uses
  `kinodisc-menu::templates::apply_template` (kebab-case names from
  `MenuTemplateId`, `--list`), validates before saving, never writes an
  invalid project.
- `burn_start` lets an image without VIDEO_TS/BDMV through
  (`PROFILE-UNKNOWN` is not blocking): it is a data disc; any other failed
  check still blocks the burn.

### 20. Kinodisc 1.2.4 additions

- **CPU use** (`kinodisc_toolchain::CpuUsage`: `Background`, `Balanced`
  default, `Full`) is `RenderOptions.cpu_usage`, `render_start`'s
  `cpu_usage` and `kinodisc-cli render --cpu`. It sets episodes at a time
  (`dvd_jobs_for`, `hd_jobs_for`; `KINODISC_ENCODE_JOBS` still overrides),
  `-threads` (`with_thread_limit`, half the threads in the background) and
  the priority (`encoder_command`: `BELOW_NORMAL_PRIORITY_CLASS` on Windows,
  `/usr/bin/nice -n 10` elsewhere). It never changes the output, so it is
  not part of any cache key.
- **AVCHD/Blu-ray episodes encode in parallel** like DVD: preparation
  (audio layout, PGS) stays sequential, the encodes share a semaphore, and
  cache publish and the concat order stay in disc order. A GPU encoder
  always works alone.
- **Older FFmpeg**: the MPEG-2 `intra_dc_precision` private option only
  exists from FFmpeg 9; earlier releases take `-dc` (same 0/1/2 values).
  The DVD `run_stage` asks the binary once
  (`mpeg2_has_intra_dc_precision`) and rewrites the argument
  (`dc_precision_for_ffmpeg`). Keep emitting `-intra_dc_precision` in the
  argument builders.
- **M2TS size**: the AVCHD/Blu-ray plan uses
  `kinodisc_avchd::ts_overhead_bytes_for` (5 % per byte plus PAT/PMT/SDT
  per second, a PES cost per video frame and 4 % of the audio), measured on
  the native backend. Do not go back to a flat percentage: at low bitrates
  the fixed part is 15–20 % of the streams. Changing the muxer's table
  periods or PES packing means re-measuring these constants.
- **AVCHD SD aspect**: `kinodisc_avchd::build_filter` fits the picture on
  the display canvas (854×480 / 1024×576 for 16:9), squeezes it to the
  stored frame and writes `setsar=N/D`. Never `setsar=N:D`: in a filter
  graph `:` separates options, so FFmpeg reads `sar=N` with `max=D`. The
  CLPI video entry carries the aspect code (`NativeStream::video_aspect`,
  3 = 16:9, 2 = 4:3); the MPLS STN has no such field.
- **Automatic menu**: when a new episode card does not fit,
  `regridEpisodeCards` lays all cards out again, smaller, centred row by
  row inside the title-safe area, with the font scaled to the card.
- **HD menu clips** end with `menu_tail` (`setsar` of the episodes + field
  order) and use `encoder_command` with the render's `CpuUsage`.
- **HD audio layout**: `canonical_avchd_layout` replaces an `und` track
  with the language another episode declares at the same position.
- **Errors**: `ProjectError::Invalid` lists its diagnostics like the
  planner ("el proyecto tiene errores: [E_…] error: …"); `messageOf` in the
  app localizes that form everywhere.
- **Installer**: `src-tauri/windows/hooks.nsh` (NSIS pre-install hook)
  deletes the stray `export-bindings.exe` of 1.2.1.
- **Image vs disc**: after the ISO is written, `warn_if_larger_than_disc`
  compares it with `DiscCapacity::medium_bytes()` (recordable sectors ×
  2048) and emits `W_IMAGE_LARGER_THAN_DISC`.

### 21. Kinodisc 1.2.5 additions

- **Windows read-back** (`kinodisc-burn::windows::verify_sync`): open
  `\\.\X:` with `FILE_FLAG_NO_BUFFERING` (sector-aligned 1 MiB buffer),
  `FSCTL_LOCK_VOLUME` + `FSCTL_DISMOUNT_VOLUME` only when the lock succeeds,
  then always `FSCTL_ALLOW_EXTENDED_DASD_IO`. Without it Windows stops reads
  where the mounted UDF volume ends, one sector before a Kinodisc image ends
  (measured on a mounted ISO: 60 786 688 of 60 788 736 bytes). Settling
  errors (21, 1110, 1111, 1112) are retried for up to 30 s. Manual test:
  `crates/kinodisc-burn/tests/windows_readback.rs` (ignored; mount an ISO).
- A short read-back is `BurnError::VerifyIncomplete` built by
  `kinodisc_burn::verify_incomplete(read, iso, detail)` on every platform,
  with bytes, sectors and what the system said. The frontend matches
  "la verificación no pudo leer todo el disco".
- **Job logs** (`src-tauri/src/joblog.rs`): one file per production
  (`produccion-*`), burn (`grabacion-*`) and erase (`borrado-*`) in
  `legacy::app_data_dir()/logs`; the home folder in any spelling (profile,
  forward slashes, the 8.3 form in `TEMP`) is written as `~`; the newest 30
  of those names are kept and nothing else in the folder is touched. A log
  that cannot be written never fails the job. Render `Failed` events carry
  `log_path`. `logs_open` opens the folder (through `open_folder`). Logs are
  local only, like "Copy details" (see section 19).

### 22. Kinodisc 1.2.6 additions: native BDAV output

- **Reference**: the native muxer must write what tsMuxeR 2.7.0 writes (a PS4
  played tsMuxeR's AVCHD and refused Kinodisc 1.2.5's). The fixtures
  `crates/kinodisc-hdmv/tests/fixtures/tsmuxer-avchd-*` are tsMuxeR files;
  `clip_info_is_byte_for_byte_the_one_a_ps4_plays` and
  `avchd_index_and_navigation_files_match_tsmuxer` must stay byte-exact.
  Attribution: `THIRD-PARTY-NOTICES.md` and `LICENSES/Apache-2.0-tsMuxer.txt`.
- **`kinodisc-hdmv::bdav`**: every FFmpeg M2TS goes through `scan_m2ts`
  (PAT/PMT, AC-3 headers, key frames by `random_access_indicator`, else SPS/IDR)
  and `fix_m2ts_in_place` (same size, 1 MiB blocks, a second file handle reads
  the PCR ahead). ATS = PCR line, never closer than `MIN_ATS_GAP` (48 Mbit/s);
  PAT gets program 0 → SIT (0x1F); SDT packets (0x11) become the SIT; PMT
  gets `05 08 HDMV ff 1b <attr> <aspect>f` and `05 04 AC-3` + A/52 descriptor.
  Run it after any packet-level change (IG injection) and before building the
  clip info: the EP map points at packet numbers.
- **Clip info** (`build_clpi` with `ClipTransport`): ClipInfo with the "HDMV"
  TS type block, SequenceInfo (PCR PID, IN/OUT), ProgramInfo, CPI with the EP
  map (coarse every 2^19 PTS ticks or 2^17 packets, one fine entry per key
  frame), empty ClipMark and extension data, 4-byte aligned.
  `NativeStream::audio_attr` comes from the AC-3 header (0x31 stereo, 0x61 5.1).
- **Index / playlist**: AppInfoBDMV length 34, AppInfoPlayList length 14
  (sequential, audio_mix_app_flag), First Playback and Top Menu interactive.
- **AVCHD on DVD media** (`!capacity.is_blu_ray()`): after the tree is complete,
  `kinodisc_avchd::native::finish_avchd_tree` sets version 0100 on index,
  MovieObject, MPLS and CLPI (BDMV and BACKUP), appends the IDEX extension
  (`AVCHD_INDEX_EXTENSION`) and creates `AUXDATA`. Blu-ray keeps 0200.
- **Verifier**: `BDMV-ATS` (Fail when arrival times go back, modulo the 2^30
  wrap) and `BDMV-SIT` (Warn when the PAT has no program 0 → 0x1F), on the
  first 512 packets of each stream.
- **UDF for BDMV/AVCHD** (`BdmvIsoBuilder`): `metadata_short_ad: true` and
  `realtime_suffixes: [".m2ts", ".ssif"]` (see `vendor/libmkiso/KINODISC-PATCH.md`).
  This was what finally made the PS4 open Kinodisc discs: keep both, and keep
  `builds_a_readable_udf_250_bdmv_image` checking them. The data-disc builder
  (`build_data_iso`, UDF 1.02) does not use them.
- **HD menu artwork lives in the menu video**, never in IG in-effects
  (`page.in_objects`): players show in-effects only while the page appears.
  `compile_hd_menu` returns a `MenuPicture` (the flattened static picture, or
  the labels for video/motion backgrounds); the render saves it as
  `menus/NN-<id>-picture.png` and `build_menu_clip_spec` encodes it as the
  menu video (static) or a full-frame overlay (moving backgrounds). The IG
  stream holds only button states (normal transparent, selected white 30 %,
  activated cyan 44 %).
- Developer tool: `cargo run -p kinodisc-iso --example bdmv_iso -- <folder>
  <out.iso> <label>` wraps a BDMV folder with the same UDF writer.

### 23. Kinodisc 1.3 additions

- **x264 (AVCHD/Blu-ray, CPU)**: two passes (`passlog`, `EncodePass`), `bf 3`
  with `b-pyramid=strict`, GOP `gop_frames` = round(fps) × `gop_seconds`
  (1 default, 2 tested on a PS4, 3 very experimental), `ref=4` at 1080 lines,
  progressive sources on 480i/576i coded with `fake-interlaced=1:pic-struct=1`
  (x264 writes pic_struct 0), BT.601 tags on SD, 18 Mb/s cap on DVD media.
  Section 7's "DTS = PTS" rule is lifted: a PS4 plays B-frames. `ContentType`
  only sets `-tune` (animation/grain); `aq-mode=3` and an automatic `hqdn3d`
  measured worse and must not come back without new measurements.
- **DVD Half-D1**: the automatic profile switches below
  `AUTO_HALF_D1_BELOW_KBPS` (1800, measured); `auto_half_d1_after_ms` is no
  longer read.
- **HD navigation, measured on a PS4** (keep these):
  - index.bdmv has **one title** (title 1 = the whole series). A 21-title
    AVCHD index is "Unknown disc".
  - First Play stays on the root menu object. The intro plays from the root
    object once (`INTRO_SEEN_GPR` 7, `CMP_LT` + `JumpObject(intro)`); with
    First Play on the intro object the PS4 refused fast-forward everywhere.
  - Episodes with `post_action = NextEpisode` (the default for new episodes)
    play title 1 from their mark (`SetGPR(0, mark); JumpTitle(1)`): seekable,
    next chapter reaches the next episode. Other episodes `JumpObject` to
    their own range playlist object inside the Top Menu title: not seekable
    on a PS4. Sending title 1 on to an episode object (compare a GPR, then
    `JumpObject`) landed in the wrong episode, so it is not done.
  - Title 1 ends with `JumpObject(root)` (`wire_title_one`); a PS4 ignores it
    and goes to its home screen. tsMuxeR ends that object with `Break`: it is
    replaced by the jump. Check HD discs with tsMuxeR present AND absent:
    with it installed, it must not be started (until 1.3.4 `Auto` used it).
  - The HD idle wait (`HD_IDLE_MIN_MS`, 60 s) counts from when the menu
    appears; HDMV cannot see the remote.
- **Domain**: `MenuTiming.idle_action` (`Wait`, `PlayAll`, `Intro`; read
  through `MenuTiming::idle()`, which maps the old `auto_play`),
  `FirstPlay.at_start` (serde default true), `AvchdVideoConfig.gop_seconds`.
  Diagnostics: `W_IDLE_INTRO_WITHOUT_INTRO`, `W_INTRO_NEVER_PLAYS`,
  `W_GOP_3_EXPERIMENTAL`, `W_LONG_GOP_BLU_RAY`, `E_AVCHD_GOP_SECONDS`.
- **CLI `project`** (`kinodisc-cli/src/project_cmd.rs`): `new`, `add`, `menu`,
  `intro`, `subtitles`, `episodes`, `gop`, `show`. It uses
  `kinodisc-project::new` and `::import`, which mirror the editor's
  `addAsset`/`addEpisodeFromAsset`/`addEpisodeSubtitleTrack`; keep them in
  sync. Every write validates first.
- **Templates** `EpisodeList`, `SeasonIndex`, `CompactIndex` (12/18/24 a
  page); page buttons sit inside the 5 % title-safe area.
- **Moving thumbnails**: over a still background, a menu's shapes and
  pictures go into the background picture and only texts go on the overlay
  (`compose.rs` and `exportMenu.ts`): the overlay is drawn above the cards,
  and a template's opaque `tpl:bg` there hid them. DVD cards read their own
  seconds (`-ss`/`-t` per input), are scaled before `loop` and repeat a
  finite number of times; never loop a whole episode or loop forever (the
  menu never finished, or hung at its end). The menu ends with
  `trim=end_frame` inside the graph (an output `-t` hung it). The stage has a
  stall watchdog on the `-progress` values, never on its lines (FFmpeg repeats
  them while stuck): `MOTION_MENU_STALL` 75 s and `MOTION_MENU_TRIES` 4 on
  DVD, `HD_MENU_STALL` 180 s and `HD_MENU_TRIES` 3 on AVCHD and Blu-ray.
- **Try the options** (`kinodisc-render::sample::compare_options`): four
  encodes of the current settings make the quality curve; SSIM against the
  achieved bitrate; savings within ±75 % and 0 under 0.15 dB.
- `theme.css`: slate-500 is #8a8d95 (dark) / #63676f (light) and the dark
  cyan-500 is #2f6fe0, for 4.5:1 text contrast (white on the accent too);
  muted text uses slate-500, not slate-600.

### 13. Localization

- Spanish is the source language; English must stay complete. Run
  `npm run i18n:check` (fails on missing English keys or on Rust diagnostic
  codes without an entry in `src/lib/i18n/diagnosticCodes.ts`).

### 11. FreeBSD and multi-arch release

- `kinodisc-burn` FreeBSD uses `camcontrol` for enumeration/eject,
  `dvd+rw-mediainfo` for media, and `growisofs` with `xorriso` fallback.
- The headless workspace must compile on FreeBSD (CI VM) and the pure crates are
  cross-checked with `x86_64-unknown-freebsd`.
- Release artifacts cover Windows x86_64/i686, Linux x86_64/i686, macOS
  arm64/x86_64 and the FreeBSD x86_64 CLI (built in a VM). Keep the 32-bit and
  FreeBSD archive names stable. CI runs the whole headless test suite on
  FreeBSD.

## Development commands

Run these before committing. They are also the CI gate:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check --workspace --exclude kinodisc-desktop --target i686-unknown-linux-gnu
cargo test -p kinodisc-plan -p kinodisc-verify -p kinodisc-hdmv --target i686-unknown-linux-gnu
cd apps/desktop && npm run check && npm run build
```

Vendored `libmkiso` tests (not part of the workspace):

```bash
cd vendor/libmkiso
cargo fmt -- --check
cargo test --all-features --locked
cargo test --no-default-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
```

FreeBSD (CI uses a native VM):

```bash
cargo check --workspace --exclude kinodisc-desktop
```

Ignored render smoke tests (require FFmpeg, DVDAuthor, Spumux and fixtures;
xorriso is optional and tsMuxeR is not used):

```bash
cargo test -p kinodisc-render --test render_smoke -- --ignored
```

Bindings:

```bash
cd apps/desktop
npm run bindings
```

## Release checklist

1. Update versions in `Cargo.toml` (workspace), `Cargo.lock`,
   `apps/desktop/package.json`, `apps/desktop/package-lock.json` and
   `apps/desktop/src-tauri/tauri.conf.json`.
2. Update `README.md`, `README.es.md`, `AGENTS.md` and `CHANGELOG.md`.
3. Regenerate bindings only if an IPC-visible type changed.
4. Run the full verification command list above plus the vendored gates.
5. Commit, create annotated tag `vX.Y.Z`, push `main` and the tag.
6. The `Release` workflow builds Windows x86_64/i686, Linux x86_64/i686 and
   macOS arm64/x86_64 artifacts and publishes the GitHub Release; verify the
   32-bit `.zip`/`.tar.gz` names before publishing.

## Common pitfalls

- `cargo test -p libmkiso` fails because the crate is excluded; run its tests
  from `vendor/libmkiso`.
- Tauri command signature changes without regenerating bindings break
  `npm run check`.
- `-dc` is deprecated for `mpeg2video`; use `-intra_dc_precision`.
- `+cgop` + `-sc_threshold -30000` makes FFmpeg fail for MPEG-2.
- The menu cache may return an artifact outside the output directory; always
  materialize it before Spumux/UI tests that expect `menu_base.mpg`.
- Do not use `Store::open_default().ok()` to hide cache errors; emit structured
  diagnostics.
- Do not patch the IFO region mask without also checking the BUP and VTS files.
- Do not add a manual `invoke()` fallback in Svelte.
- Do not commit generated `target/`, `dist/` or cache blobs.
