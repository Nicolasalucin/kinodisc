/**
 * H.264 encoder choices for AVCHD and Blu-ray (DVD-Video always uses
 * MPEG-2 on the CPU). The disc size never depends on this choice: the plan
 * already fills the disc, so the encoder only changes quality and time.
 */
import type {
  ContentType,
  EncoderPreference,
  HardwareEncoder,
  Project,
} from "../api/bindings";

export type EncoderChoice =
  | "auto"
  | "cpuFast"
  | "cpuOnly"
  | "cpuSlow"
  | "cpuSlower"
  | `hw:${HardwareEncoder}`;

export const HARDWARE_LABELS: Record<HardwareEncoder, string> = {
  nvencH264: "NVIDIA NVENC",
  qsvH264: "Intel Quick Sync",
  vaapiH264: "VA-API",
  amfH264: "AMD AMF",
  videoToolboxH264: "Apple VideoToolbox",
};

/** The value `render_start` expects. */
export function encoderPreference(choice: EncoderChoice): EncoderPreference {
  if (choice.startsWith("hw:"))
    return { prefer: choice.slice(3) as HardwareEncoder };
  return choice as EncoderPreference;
}

/**
 * Interlaced and PsF video always goes to x264: hardware encoders only write
 * progressive frames (the engine applies the same rule).
 */
export function cpuOnlyFormat(project: Project | null | undefined): boolean {
  const video = project?.disc.video;
  if (!video || video.kind !== "avchd") return false;
  if (
    video.resolution === "1080i" ||
    video.resolution === "480i" ||
    video.resolution === "576i"
  )
    return true;
  const fps = video.frameRate.num / video.frameRate.den;
  return (
    video.resolution === "1080p" &&
    (Math.abs(fps - 25) < 0.01 || Math.abs(fps - 29.97) < 0.01)
  );
}

/**
 * The recommended choice for the planned video bitrate: at a low bitrate
 * (many hours on one disc) the slow preset gives visibly fewer blocks; at a
 * comfortable one the medium preset is already close to it.
 */
export function recommendedEncoder(
  project: Project | null | undefined,
  videoKbps: number | null | undefined,
): EncoderChoice {
  const video = project?.disc.video;
  const hd =
    video?.kind === "avchd" &&
    !(video.resolution === "480i" || video.resolution === "576i");
  const low = hd ? 8000 : 2500;
  return videoKbps != null && videoKbps > 0 && videoKbps < low
    ? "cpuSlow"
    : "cpuOnly";
}

/**
 * What the videos are (film, animation, grainy home video): tunes the
 * encoder. Remembered per project on this computer; the quality sample and
 * the disc use the same value.
 */
function contentKey(projectId: string | null | undefined): string {
  return `kinodisc_content_${projectId ?? ""}`;
}

export function rememberedContent(
  projectId: string | null | undefined,
): ContentType {
  try {
    const value = localStorage.getItem(contentKey(projectId));
    if (value === "animation" || value === "grain") return value;
  } catch {
    // Storage unavailable: the default is fine.
  }
  return "film";
}

export function rememberContent(
  projectId: string | null | undefined,
  value: ContentType,
): void {
  try {
    // Only non-default choices are kept (no key per project forever).
    if (value === "film") localStorage.removeItem(contentKey(projectId));
    else localStorage.setItem(contentKey(projectId), value);
  } catch {
    // Not remembered this time.
  }
}
