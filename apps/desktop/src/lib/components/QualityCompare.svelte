<script lang="ts">
  import { onMount } from "svelte";
  import {
    commands,
    type ContentType,
    type Denoise,
    type Episode_Serialize,
    type OptionComparison,
    type QualitySample,
    type SampleOption,
  } from "../api/bindings";
  import { messageOf, unwrap } from "../api/client";
  import { t, i18n } from "../i18n/index.svelte";
  import { projectStore } from "../stores/project.svelte";
  import {
    encoderPreference,
    recommendedEncoder,
    rememberContent,
    rememberedContent,
  } from "../utils/encoder";

  interface Props {
    episode: Episode_Serialize;
    /** Offset inside the episode where the 10 s sample starts. */
    atMs?: number;
    onclose: () => void;
  }

  let { episode, atMs, onclose }: Props = $props();

  let sample = $state<QualitySample | null>(null);
  let original = $state<string | null>(null);
  let disc = $state<string | null>(null);
  let error = $state<string | null>(null);
  let split = $state(50);

  // "Try the options": the same seconds with each option.
  let comparison = $state<OptionComparison | null>(null);
  let comparing = $state(false);
  let compareError = $state<string | null>(null);
  let optionImages = $state<string[]>([]);
  let currentImage = $state<string | null>(null);
  /** -1: the current settings; otherwise the index of the option shown. */
  let shown = $state(-1);
  let notice = $state<string | null>(null);
  /** Options applied from this table (their button turns into a note). */
  let applied = $state<number[]>([]);

  const kbps = $derived(
    projectStore.plan?.perEpisode.find((entry) => entry.episode === episode.id)?.videoKbps ??
      projectStore.plan?.videoKbps ??
      0,
  );
  const hd = $derived(projectStore.project?.disc.video.kind === "avchd");

  const verdict = $derived.by(() => {
    const psnr = sample?.psnrDb ?? 0;
    if (psnr >= 42) return { label: t("Excelente: la compresión no se nota"), tone: "text-emerald-300" };
    if (psnr >= 38) return { label: t("Muy buena: solo se nota mirando muy de cerca"), tone: "text-emerald-300" };
    if (psnr >= 34) return { label: t("Buena: puede notarse en escenas con mucho movimiento"), tone: "text-amber-300" };
    return {
      label: t("Se notará la compresión: usa un disco más grande o reparte en varios discos"),
      tone: "text-rose-300",
    };
  });

  const start = $derived.by(() => {
    const duration = projectStore.episodeEffectiveDuration(episode);
    return atMs ?? Math.max(0, Math.round(duration / 2 - 5_000));
  });

  onMount(async () => {
    const project = projectStore.project;
    if (!project) return;
    if (!kbps) {
      error = t("Primero se tiene que calcular el plan del disco.");
      return;
    }
    try {
      // The sample uses the encoder the production window recommends.
      const result = await unwrap(
        commands.renderQualitySample(
          project,
          episode.id,
          start,
          kbps,
          encoderPreference(recommendedEncoder(project, kbps)),
          rememberedContent(project.id),
        ),
      );
      sample = result;
      original = await unwrap(commands.readImageDataUrl(result.originalPng));
      disc = await unwrap(commands.readImageDataUrl(result.discPng));
    } catch (cause) {
      error = messageOf(cause);
    }
  });

  /** Every option that differs from what the episode and project use now. */
  function optionsToTry(): SampleOption[] {
    const project = projectStore.project;
    if (!project) return [];
    const options: SampleOption[] = [];
    const denoise = episode.filters?.denoise ?? "off";
    for (const level of ["off", "light", "strong"] as Denoise[]) {
      if (level !== denoise) options.push({ kind: "denoise", level });
    }
    const video = project.disc.video;
    if (video.kind === "avchd") {
      const content = rememberedContent(project.id);
      for (const item of ["film", "animation", "grain"] as ContentType[]) {
        if (item !== content) options.push({ kind: "content", content: item });
      }
      const current = recommendedEncoder(project, kbps);
      for (const encoder of ["cpuFast", "cpuOnly", "cpuSlow", "cpuSlower"] as const) {
        if (encoder !== current) options.push({ kind: "preset", encoder });
      }
      const gop = video.gopSeconds ?? 1;
      for (const seconds of [1, 2, 3]) {
        if (seconds !== gop) options.push({ kind: "gop", seconds });
      }
    }
    return options;
  }

  async function tryOptions() {
    const project = projectStore.project;
    if (!project || comparing) return;
    comparing = true;
    compareError = null;
    notice = null;
    try {
      const result = await unwrap(
        commands.renderCompareOptions(
          project,
          episode.id,
          start,
          kbps,
          encoderPreference(recommendedEncoder(project, kbps)),
          rememberedContent(project.id),
          optionsToTry(),
        ),
      );
      currentImage = await unwrap(commands.readImageDataUrl(result.currentPng));
      optionImages = await Promise.all(
        result.options.map((item) => unwrap(commands.readImageDataUrl(item.discPng))),
      );
      comparison = result;
      shown = -1;
      applied = [];
    } catch (cause) {
      compareError = messageOf(cause);
    } finally {
      comparing = false;
    }
  }

  const shownImage = $derived(
    comparison ? (shown < 0 ? currentImage : (optionImages[shown] ?? currentImage)) : disc,
  );

  function optionLabel(option: SampleOption): string {
    switch (option.kind) {
      case "denoise":
        return option.level === "off"
          ? t("Sin quitar ruido")
          : option.level === "light"
            ? t("Quitar ruido: suave")
            : t("Quitar ruido: fuerte");
      case "content":
        return option.content === "film"
          ? t("Película o serie filmada")
          : option.content === "animation"
            ? t("Animación o anime")
            : t("Vídeo casero o con mucho grano");
      case "preset":
        return option.encoder === "cpuFast"
          ? t("CPU rápido (x264 fast)")
          : option.encoder === "cpuOnly"
            ? t("CPU medio (x264 medium)")
            : option.encoder === "cpuSlow"
              ? t("CPU lento (x264 slow)")
              : t("CPU máximo (x264 slower)");
      case "gop":
        return t("GOP de {seconds} s", { seconds: option.seconds });
    }
  }

  function savingLabel(percent: number | null): string {
    const value = Math.round(percent ?? 0);
    if (Math.abs(value) < 1) return t("sin cambio");
    // The engine limits the estimate to ±75 % (past the measured curve).
    if (value >= 75) return t("ahorra más de 75 %");
    if (value <= -75) return t("gasta más de 75 % más");
    return value > 0
      ? t("ahorra ~{percent} %", { percent: value })
      : t("gasta ~{percent} % más", { percent: -value });
  }

  /** Applies an option that lives in the project or on this computer. */
  function apply(option: SampleOption, index: number) {
    const project = projectStore.project;
    if (!project) return;
    // One choice per kind: applying another row replaces the earlier one.
    applied = [
      ...applied.filter((other) => comparison?.options[other]?.option.kind !== option.kind),
      index,
    ];
    if (option.kind === "denoise") {
      projectStore.updateEpisodeFilters(episode.id, { denoise: option.level });
      notice = t("Aplicado a este episodio.");
    } else if (option.kind === "content") {
      rememberContent(project.id, option.content);
      notice = t("Aplicado: se usa al producir este proyecto.");
    } else if (option.kind === "gop") {
      projectStore.setGopSeconds(option.seconds);
      notice = t("Aplicado a todo el disco.");
    }
  }

  function seconds(ms: number): string {
    const total = Math.round(ms / 1000);
    return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, "0")}`;
  }
</script>

<svelte:window onkeydown={(event) => event.key === "Escape" && !comparing && onclose()} />

<div class="fixed inset-0 z-50 flex items-center justify-center p-4">
  <button type="button" aria-label={t("Cerrar")} class="ui-backdrop" onclick={() => !comparing && onclose()}></button>
  <div class="ui-dialog w-full max-w-4xl" role="dialog" aria-modal="true" aria-labelledby="quality-title">
    <header class="ui-dialog-header">
      <div>
        <h2 id="quality-title" class="ui-title">{t("Probar calidad")} · {episode.title}</h2>
        <p class="ui-hint mt-0.5">
          {t("10 segundos codificados exactamente como en el disco, a {kbps} kbps.", { kbps: kbps.toLocaleString(i18n.locale) })}
        </p>
      </div>
    </header>

    <div class="max-h-[75vh] overflow-y-auto px-5 py-4">
      {#if error}
        <p class="rounded-md bg-rose-500/10 px-3 py-2 text-[12px] text-rose-200" role="alert">{error}</p>
      {:else if !sample || !original || !disc}
        <div class="ui-empty py-16">
          <p class="text-[13px] text-slate-300">{t("Codificando la muestra…")}</p>
          <p class="ui-hint mt-1">{t("Tarda unos segundos: usa el mismo codificador que el disco.")}</p>
        </div>
      {:else}
        <div class="relative mx-auto aspect-video w-full select-none overflow-hidden rounded-md border border-slate-800 bg-black">
          <img src={original} alt={t("Original")} class="absolute inset-0 h-full w-full object-contain" />
          <img
            src={shownImage ?? disc}
            alt={t("Disco")}
            class="absolute inset-0 h-full w-full object-contain"
            style={`clip-path: inset(0 0 0 ${split}%)`}
          />
          <div class="pointer-events-none absolute inset-y-0 w-px bg-[#fff]" style={`left: ${split}%`}></div>
          <span class="absolute left-2 top-2 rounded bg-black/70 px-1.5 py-0.5 text-[11px] text-[#fff]">{t("Original")}</span>
          <span class="absolute right-2 top-2 rounded bg-black/70 px-1.5 py-0.5 text-[11px] text-[#fff]">
            {comparison && shown >= 0 ? optionLabel(comparison.options[shown].option) : t("Disco")}
          </span>
        </div>
        <input
          type="range"
          min="0"
          max="100"
          bind:value={split}
          class="mt-2 w-full"
          aria-label={t("Comparar")}
        />
        <div class="mt-2 flex flex-wrap items-baseline justify-between gap-2">
          {#if comparison && shown >= 0}
            <!-- The verdict and the PSNR are those of the current settings: an
                 option shows its own name and its own figure. -->
            <p class="text-[14px] font-medium text-slate-200">{optionLabel(comparison.options[shown].option)}</p>
          {:else}
            <p class="text-[14px] font-medium {verdict.tone}">{verdict.label}</p>
          {/if}
          <p class="font-mono text-[12px] text-slate-400">
            {#if comparison && shown >= 0}
              SSIM {(comparison.options[shown]?.ssimDb ?? 0).toFixed(2)} dB
            {:else}
              PSNR {(sample.psnrDb ?? 0).toFixed(1)} dB{comparison ? ` · SSIM ${(comparison.currentSsimDb ?? 0).toFixed(2)} dB` : ""}
            {/if} · {seconds(sample.startMs ?? 0)}–{seconds((sample.startMs ?? 0) + (sample.seconds ?? 0) * 1000)}
          </p>
        </div>

        <section class="mt-4 rounded-lg border border-slate-800 p-3" aria-labelledby="options-title">
          <div class="flex flex-wrap items-center justify-between gap-2">
            <div>
              <h3 id="options-title" class="ui-section-title">{t("Probar filtros y opciones")}</h3>
              <p class="ui-hint">
                {hd
                  ? t("Codifica estos segundos con cada filtro y opción del codificador y estima cuánto ahorra cada uno frente a lo que tienes ahora. Tarda unos minutos.")
                  : t("Codifica estos segundos con cada nivel de quitar ruido y estima cuánto ahorra frente a lo que tienes ahora. Tarda un par de minutos.")}
              </p>
            </div>
            <button type="button" class="ui-btn ui-btn-sm" onclick={tryOptions} disabled={comparing}>
              {comparing ? t("Probando…") : comparison ? t("Probar otra vez") : t("Probar")}
            </button>
          </div>
          {#if compareError}
            <p class="mt-2 rounded-md bg-rose-500/10 px-3 py-2 text-[12px] text-rose-200" role="alert">{compareError}</p>
          {/if}
          {#if comparison}
            <table class="mt-3 w-full text-[12px]">
              <thead>
                <tr class="text-left text-slate-400">
                  <th class="py-1 font-medium">{t("Opción")}</th>
                  <th class="py-1 font-medium">SSIM</th>
                  <th class="py-1 font-medium">{t("Bits para la misma calidad")}</th>
                  <th class="py-1"><span class="sr-only">{t("Acciones")}</span></th>
                </tr>
              </thead>
              <tbody>
                <tr class={shown === -1 ? "border-t border-slate-800 bg-cyan-500/10" : "border-t border-slate-800"}>
                  <td class="py-1.5">
                    <button type="button" class="text-left text-slate-200 underline-offset-2 hover:underline" onclick={() => (shown = -1)}>
                      {t("Lo que tienes ahora")}
                    </button>
                  </td>
                  <td class="font-mono text-slate-300">{(comparison.currentSsimDb ?? 0).toFixed(2)} dB</td>
                  <td class="text-slate-400">—</td>
                  <td></td>
                </tr>
                {#each comparison.options as item, index}
                  <tr class={shown === index ? "border-t border-slate-800 bg-cyan-500/10" : "border-t border-slate-800"}>
                    <td class="py-1.5">
                      <button type="button" class="text-left text-slate-200 underline-offset-2 hover:underline" onclick={() => (shown = index)}>
                        {optionLabel(item.option)}
                      </button>
                    </td>
                    <td class="font-mono text-slate-300">{(item.ssimDb ?? 0).toFixed(2)} dB</td>
                    <td class={(item.savingPercent ?? 0) >= 1 ? "text-emerald-300" : (item.savingPercent ?? 0) <= -1 ? "text-amber-300" : "text-slate-400"}>
                      {savingLabel(item.savingPercent)}
                    </td>
                    <td class="text-right">
                      {#if applied.includes(index)}
                        <span class="text-[11px] text-emerald-300">{t("Aplicado")}</span>
                      {:else if item.option.kind !== "preset"}
                        <button type="button" class="ui-btn ui-btn-sm ui-btn-ghost" onclick={() => apply(item.option, index)}>{t("Aplicar")}</button>
                      {/if}
                    </td>
                  </tr>
                {/each}
              </tbody>
            </table>
            <p class="ui-hint mt-2">
              {t("Clic en una opción para verla en la comparación. Quitar ruido se compara con el original con su ruido: el número cuenta lo que borra como pérdida, así que mira la imagen. El codificador (CPU rápido, medio…) se elige en Producción.")}
            </p>
            {#if notice}
              <p class="mt-1 text-[12px] text-emerald-300" role="status">{notice}</p>
            {/if}
          {/if}
        </section>
      {/if}
    </div>

    <footer class="ui-dialog-footer">
      <button type="button" class="ui-btn ui-btn-primary" onclick={onclose} disabled={comparing}>{t("Cerrar")}</button>
    </footer>
  </div>
</div>
