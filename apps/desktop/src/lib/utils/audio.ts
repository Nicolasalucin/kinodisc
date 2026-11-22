/**
 * AC-3 bitrates offered in the audio selects. The engine accepts every
 * standard AC-3 rate from 64 to 448 kbps (`AC3_BITRATES_KBPS` in
 * kinodisc-domain); 5.1 needs at least 192 kbps.
 */
const AC3_CHOICES = [64, 96, 128, 160, 192, 224, 256, 320, 384, 448];
const AC3_MIN_SURROUND = 192;

/** Every rate the engine accepts (`AC3_BITRATES_KBPS`). */
const AC3_SUPPORTED = [64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448];

/** The same rule as `ac3_bitrate_supported` + `EpisodeAudio::encoded_channels`. */
export function ac3TrackValid(track: {
  ac3Kbps: number;
  format: string;
  mastering?: { passthrough?: boolean; channelMode?: string } | null;
}): boolean {
  if (track.mastering?.passthrough) return true;
  const mode = track.mastering?.channelMode ?? "source";
  const surround =
    mode === "surround51" || mode === "upmix51" || (mode === "source" && track.format === "ac3Surround51");
  return AC3_SUPPORTED.includes(track.ac3Kbps) && (!surround || track.ac3Kbps >= AC3_MIN_SURROUND);
}

/** The bitrates to offer for a track, keeping its current value listed. */
export function ac3Choices(surround: boolean, current?: number): number[] {
  const choices = AC3_CHOICES.filter((kbps) => !surround || kbps >= AC3_MIN_SURROUND);
  if (current && !choices.includes(current)) return [...choices, current].sort((a, b) => a - b);
  return choices;
}
