<script lang="ts">
  import { getVersion } from "@tauri-apps/api/app";
  import { commands } from "../api/bindings";
  import { t } from "../i18n/index.svelte";
  import { burnDiagnostics, explainBurnError } from "../i18n/burnErrors";

  let {
    message,
    action,
    device = "",
    image = null,
    extra = [],
    onerase,
  }: {
    message: string;
    action: string;
    device?: string;
    image?: string | null;
    /** More facts for "Copy details" (drive, speed, progress). */
    extra?: string[];
    onerase?: () => void;
  } = $props();

  const explanation = $derived(explainBurnError(message));
  let copied = $state(false);
  let logsError = $state<string | null>(null);

  async function copyDetails() {
    let appVersion = "?";
    try {
      appVersion = await getVersion();
    } catch {
      // Unknown version: still useful.
    }
    try {
      await navigator.clipboard.writeText(burnDiagnostics({ appVersion, action, device, image, message, extra }));
      copied = true;
      setTimeout(() => (copied = false), 2500);
    } catch {
      copied = false;
    }
  }

  /** The folder with one log per burn, erase and production. */
  async function openLogs() {
    const result = await commands.logsOpen();
    logsError = result.status === "error" ? result.error : null;
  }
</script>

<div class="rounded-md bg-rose-500/10 px-3 py-2 text-[12px] leading-5 text-rose-200" role="alert">
  {#if explanation}
    <p class="font-medium">{explanation.reason}</p>
    <p class="mt-0.5 text-rose-100/80">{explanation.advice}</p>
    <details class="mt-1">
      <summary class="cursor-pointer text-[11px] text-rose-100/80">{t("Detalles técnicos")}</summary>
      <p class="mt-1 break-words font-mono text-[11px] text-rose-100/90">{message}</p>
    </details>
  {:else}
    <p class="break-words">{message}</p>
  {/if}
  <div class="mt-2 flex flex-wrap gap-2">
    {#if explanation?.suggestErase && onerase}
      <button type="button" class="ui-btn ui-btn-sm" onclick={onerase}>{t("Borrar disco…")}</button>
    {/if}
    <button type="button" class="ui-btn ui-btn-sm ui-btn-ghost" onclick={copyDetails}>
      {copied ? t("Copiado") : t("Copiar detalles")}
    </button>
    <button type="button" class="ui-btn ui-btn-sm ui-btn-ghost" onclick={openLogs}>{t("Abrir registros")}</button>
  </div>
  {#if logsError}
    <p class="mt-1 break-words text-[11px] text-rose-100/90">{logsError}</p>
  {/if}
</div>
