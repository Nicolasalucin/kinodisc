/**
 * H.264 encoder choices for AVCHD and Blu-ray (DVD-Video always uses
 * MPEG-2 on the CPU). The disc size never depends on this choice: the plan
 * already fills the disc, so the encoder only changes quality and time.
 */
import type { EncoderPreference, HardwareEncoder, Project } from "../api/bindings";

export type EncoderChoice = "auto" | "cpuFast" | "cpuOnly" | "cpuSlow" | `hw:${HardwareEncoder}`;

export const HARDWARE_LABELS: Record<HardwareEncoder, string> = {
  nvencH264: "NVIDIA NVENC",
  qsvH264: "Intel Quick Sync",
  vaapiH264: "VA-API",
  amfH264: "AMD AMF",
  videoToolboxH264: "Apple VideoToolbox",
};

/** The value `render_start` expects. */
export function encoderPreference(choice: EncoderChoice): EncoderPreference {
  if (choice.startsWith("hw:")) return { prefer: choice.slice(3) as HardwareEncoder };
  return choice as EncoderPreference;
}

/**
 * Interlaced and PsF video always goes to x264: hardware encoders only write
 * progressive frames (the engine applies the same rule).
 */
export function cpuOnlyFormat(project: Project | null | undefined): boolean {
  const video = project?.disc.video;
  if (!video || video.kind !== "avchd") return false;
  if (video.resolution === "1080i" || video.resolution === "480i" || video.resolution === "576i") return true;
  const fps = video.frameRate.num / video.frameRate.den;
  return video.resolution === "1080p" && (Math.abs(fps - 25) < 0.01 || Math.abs(fps - 29.97) < 0.01);
}

/**
 * The recommended choice for the planned video bitrate: at a low bitrate
 * (many hours on one disc) the slow preset gives visibly fewer blocks; at a
 * comfortable one the medium preset is already close to it.
 */
export function recommendedEncoder(project: Project | null | undefined, videoKbps: number | null | undefined): EncoderChoice {
  const video = project?.disc.video;
  const hd = video?.kind === "avchd" && !(video.resolution === "480i" || video.resolution === "576i");
  const low = hd ? 8000 : 2500;
  return videoKbps != null && videoKbps > 0 && videoKbps < low ? "cpuSlow" : "cpuOnly";
}
