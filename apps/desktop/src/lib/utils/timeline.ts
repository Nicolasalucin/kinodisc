/**
 * Moving on the trim timeline by frame, second or minute.
 *
 * Times are whole milliseconds because the project stores trims that way.
 * The encoders cut with `-ss <ms>`, which keeps the first picture whose
 * timestamp is not before it. So a frame is named by its timestamp rounded
 * DOWN: rounding up would start the cut one picture late.
 */
import type { FrameRate } from "../api/bindings";

export type TimeStep = "frame" | "second" | "minute";

export const TIME_STEPS: TimeStep[] = ["frame", "second", "minute"];

/** The picture on screen at `ms` (0 is the first one). */
export function frameIndexAt(ms: number, rate: FrameRate): number {
  return Math.max(
    0,
    Math.floor(((ms + 0.999) * rate.num) / (rate.den * 1000) + 1e-9),
  );
}

/** Where picture `index` starts, in the milliseconds the project stores. */
export function frameStartMs(index: number, rate: FrameRate): number {
  return Math.floor((Math.max(0, index) * 1000 * rate.den) / rate.num + 1e-6);
}

/** Seconds to give a `<video>` so it shows the picture at `ms`: its middle. */
export function frameDisplaySeconds(ms: number, rate: FrameRate): number {
  return ((frameIndexAt(ms, rate) + 0.5) * rate.den) / rate.num;
}

/** `ms` moved to the grid of the step (frames need the source's rate). */
export function snapMs(
  ms: number,
  step: TimeStep,
  rate: FrameRate | null,
): number {
  if (step === "frame")
    return rate ? frameStartMs(frameIndexAt(ms, rate), rate) : ms;
  const size = step === "minute" ? 60_000 : 1_000;
  return Math.round(ms / size) * size;
}

/** One step back (-1) or forward (+1) from `ms`. */
export function stepMs(
  ms: number,
  direction: -1 | 1,
  step: TimeStep,
  rate: FrameRate | null,
): number {
  if (step === "frame" && rate)
    return frameStartMs(frameIndexAt(ms, rate) + direction, rate);
  return ms + direction * (step === "minute" ? 60_000 : 1_000);
}

const STEP_KEY = "kinodisc_timeline_step";

export function rememberedTimeStep(): TimeStep {
  try {
    const value = localStorage.getItem(STEP_KEY);
    if (value === "frame" || value === "minute") return value;
  } catch {
    // Storage unavailable: the default is fine.
  }
  return "second";
}

export function rememberTimeStep(step: TimeStep): void {
  try {
    if (step === "second") localStorage.removeItem(STEP_KEY);
    else localStorage.setItem(STEP_KEY, step);
  } catch {
    // Not remembered this time.
  }
}
