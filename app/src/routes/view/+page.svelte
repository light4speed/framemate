<script lang="ts">
  // Same protocol as the agent's /stream page (JSON {codec}, fMP4 init, moof+mdat → MSE).
  // The agent only captures while someone watches, so leaving this page stops it.
  import { onDestroy, onMount } from "svelte";
  import { pushState } from "$app/navigation";
  import { page } from "$app/state";
  import { agent } from "$lib/agent.svelte";
  import { cast, type CastDevice } from "$lib/cast.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import Row from "$lib/components/Row.svelte";
  import Section from "$lib/components/Section.svelte";

  let video: HTMLVideoElement;
  let status = $state("Connecting…");
  let socket: WebSocket | null = null;
  let retry: ReturnType<typeof setTimeout> | undefined;
  let closed = false;
  let objectUrl: string | undefined;
  let picker = $state(false);
  /** Android 17+ blocks discovery until "Nearby devices" is granted. */
  let blocked = $state(false);
  const stats = $derived(agent.state?.stream);
  const fullscreen = $derived(!!page.state.fullscreen);

  // Fullscreen is a shallow history entry: Android's back button (and Esc on desktop)
  // leave it like any other navigation.
  function enterFullscreen() {
    pushState("", { fullscreen: true });
  }

  function applyFullscreen(enabled: boolean) {
    if (window.FrameMateAndroid) {
      window.FrameMateAndroid.setFullscreen(enabled);
    } else if (enabled && !document.fullscreenElement) {
      document.documentElement.requestFullscreen?.().catch(() => {});
    } else if (!enabled && document.fullscreenElement) {
      document.exitFullscreen().catch(() => {});
    }
  }

  $effect(() => applyFullscreen(fullscreen));

  // Leaving browser fullscreen via Esc: drop our history entry too.
  function onFullscreenChange() {
    if (!document.fullscreenElement && page.state.fullscreen) history.back();
  }

  function openCast() {
    blocked = window.FrameMateAndroid?.localNetworkAllowed?.() === false;
    cast.refresh();
    picker = true;
  }

  async function chooseCast(device: CastDevice) {
    await cast.play(device);
    // Keep the sheet open on failure, so the error is next to the device list.
    if (!cast.error) picker = false;
  }

  function connect() {
    const ws = new WebSocket(agent.socketUrl("/api/stream/ws"));
    socket = ws;
    ws.binaryType = "arraybuffer";
    let buffer: SourceBuffer | null = null;
    const queue: ArrayBuffer[] = [];
    const pump = () => {
      if (buffer && !buffer.updating && queue.length) buffer.appendBuffer(queue.shift()!);
    };

    ws.onmessage = event => {
      if (typeof event.data !== "string") {
        queue.push(event.data);
        pump();
        return;
      }
      const { codec } = JSON.parse(event.data);
      const type = `video/mp4; codecs="${codec}"`;
      if (!MediaSource.isTypeSupported(type)) {
        status = `This device can't play ${codec}`;
        closed = true;
        ws.close();
        return;
      }
      const source = new MediaSource();
      if (objectUrl) URL.revokeObjectURL(objectUrl); // previous connection's source
      objectUrl = URL.createObjectURL(source);
      video.src = objectUrl;
      source.addEventListener(
        "sourceopen",
        () => {
          buffer = source.addSourceBuffer(type);
          buffer.mode = "sequence";
          buffer.addEventListener("updateend", () => {
            keepLive(buffer!);
            pump();
          });
          pump();
        },
        { once: true },
      );
      status = "";
    };
    ws.onclose = () => {
      if (closed) return;
      status = "Reconnecting…";
      retry = setTimeout(connect, 2000);
    };
  }

  // Catch up by playing faster; seeking restarts decoding at the last keyframe.
  function keepLive(buffer: SourceBuffer) {
    if (buffer.updating || !buffer.buffered.length) return;
    const start = buffer.buffered.start(0);
    const end = buffer.buffered.end(buffer.buffered.length - 1);
    const behind = end - video.currentTime;
    if (video.currentTime < start || behind > 2) video.currentTime = Math.max(start, end - 0.2);
    video.playbackRate = behind > 0.3 ? 1.1 : 1.0;
    if (video.paused) video.play().catch(() => {});
    if (video.currentTime - start > 10) buffer.remove(start, video.currentTime - 5);
  }

  // In the background the socket dies or, worse, keeps the Frame capturing for nobody:
  // stop it, and start fresh (new keyframe, no stale buffer) when the app comes back.
  function onVisibilityChange() {
    if (closed) return;
    clearTimeout(retry);
    if (socket) socket.onclose = null;
    socket?.close();
    socket = null;
    if (document.visibilityState === "visible") {
      status = "Connecting…";
      connect();
    }
  }

  onMount(() => {
    connect();
    cast.init();
  });
  onDestroy(() => {
    if (fullscreen) applyFullscreen(false);
    closed = true;
    clearTimeout(retry);
    socket?.close();
    if (objectUrl) URL.revokeObjectURL(objectUrl);
    // The TV is the only viewer left once this page is gone; don't keep the Frame capturing.
    cast.halt();
  });
</script>

<svelte:document onfullscreenchange={onFullscreenChange} onvisibilitychange={onVisibilityChange} />

<div class="player" class:fullscreen>
  <video bind:this={video} autoplay muted playsinline></video>
  {#if status}<div class="overlay">{status}</div>{/if}
  {#if cast.supported}
    <button class="control cast-button" onclick={openCast} aria-label="Cast to a TV" title="Cast to a TV">
      <Icon name="cast" size={22} />
    </button>
  {/if}
  {#if cast.casting}
    <button class="casting" onclick={openCast} aria-label="Casting to {cast.deviceName ?? "a TV"}">
      <Icon name="cast" size={16} />
      <span>{cast.deviceName ?? "TV"}</span>
    </button>
  {/if}
  {#if fullscreen}
    <button class="control exit" onclick={() => history.back()} aria-label="Exit fullscreen">
      <Icon name="back" size={22} />
    </button>
  {:else}
    <button class="control enter" onclick={enterFullscreen} aria-label="Fullscreen">
      <svg viewBox="0 0 36 36" width="22" height="22" fill="currentColor" aria-hidden="true">
        <path d="M4 4h11v4H8v7H4zM21 4h11v11h-4V8h-7zM4 21h4v7h7v4H4zM28 21h4v11H21v-4h7z" />
      </svg>
    </button>
  {/if}
</div>

{#if !picker && cast.error}
  <div class="cast-note error">
    <span class="cast-label">{cast.error}</span>
    <button onclick={() => cast.dismissError()}>Dismiss</button>
  </div>
{/if}
{#if cast.serving}
  <div class="cast-note">
    <Icon name="cast" size={18} />
    <span class="cast-label">
      {#if cast.playing}
        Casting to {cast.deviceName ?? "the TV"}
      {:else if cast.casting}
        Connecting to {cast.deviceName ?? "the TV"}…
      {:else}
        Casting to {cast.deviceName ?? "the TV"}…
      {/if}
    </span>
    <button class="stop" onclick={() => cast.stop()}>Stop casting</button>
  </div>
{/if}

<Section title="Mirroring">
  <Row label="Resolution" value={stats ? `${stats.width}×${stats.height}` : null} />
  <Row label="Frame rate" value={stats ? `${stats.fps.toFixed(0)} fps (headset ${stats.source_fps.toFixed(0)})` : null} />
  <Row label="Bitrate" value={stats ? `${(stats.kbit_per_sec / 1000).toFixed(1)} Mbit/s` : null} />
  <Row label="Viewers" value={stats?.viewers} />
</Section>
<p class="hint muted">Mirroring only runs while someone watches. A sleeping headset shows black frames.</p>

{#if picker}
  <button class="scrim" type="button" aria-label="Close" onclick={() => (picker = false)}></button>
  <div class="sheet" role="dialog" aria-modal="true" aria-label="Cast to a TV">
    <div class="sheet-head">
      <h2>Cast to</h2>
      <button class="sheet-close" onclick={() => (picker = false)} aria-label="Close">
        <svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor" aria-hidden="true">
          <path d="M6.4 5 5 6.4 10.6 12 5 17.6 6.4 19 12 13.4 17.6 19 19 17.6 13.4 12 19 6.4 17.6 5 12 10.6z" />
        </svg>
      </button>
    </div>
    {#if !cast.available}
      <p class="sheet-note muted">{cast.error ?? "Starting the Cast service…"}</p>
    {:else if cast.devices.length}
      <div class="devices">
        {#each cast.devices as device (device.id)}
          <button class="device" onclick={() => chooseCast(device)} disabled={cast.busy}>
            <Icon name="cast" size={22} />
            <span class="device-text">
              <span class="device-name">{device.name}</span>
              {#if device.description}<span class="device-desc muted">{device.description}</span>{/if}
            </span>
          </button>
        {/each}
      </div>
    {:else}
      <p class="sheet-note muted">
        Looking for TVs on your Wi-Fi… The TV must be on and support Chromecast built in.
      </p>
      {#if blocked}
        <p class="sheet-note muted">
          Allow “Nearby devices” for FrameMate in Android settings so it can find the TV.
        </p>
      {/if}
    {/if}
    {#if cast.busy}<p class="sheet-note muted">Serving the mirror…</p>{/if}
    {#if cast.error && cast.available}<p class="sheet-note error">{cast.error}</p>{/if}
    {#if cast.serving}
      <button
        class="stop-sheet"
        onclick={() => {
          cast.stop();
          picker = false;
        }}>Stop casting</button
      >
    {/if}
  </div>
{/if}

<style>
  .player {
    position: relative;
    background: #000;
    aspect-ratio: 16 / 9;
  }
  video {
    width: 100%;
    height: 100%;
    display: block;
  }
  .player.fullscreen {
    position: fixed;
    inset: 0;
    z-index: 10;
    aspect-ratio: auto;
  }
  .control {
    position: absolute;
    display: grid;
    place-items: center;
    width: 44px;
    height: 44px;
    border: 0;
    border-radius: 2px;
    background: rgb(0 0 0 / 0.5);
    color: #fff;
  }
  .enter {
    right: 8px;
    bottom: 8px;
  }
  .exit {
    left: 12px;
    top: 12px;
  }
  .overlay {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    color: var(--muted);
  }
  .hint {
    padding: 12px var(--gutter);
    font-size: 13px;
  }
  .cast-button {
    left: 8px;
    bottom: 8px;
  }
  .casting {
    position: absolute;
    top: 8px;
    right: 8px;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    max-width: min(60%, 240px);
    padding: 6px 10px;
    border: 0;
    border-radius: 2px;
    background: rgb(0 0 0 / 0.55);
    color: #fff;
    font-size: 13px;
  }
  .casting span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .cast-note {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px var(--gutter);
    border-bottom: 1px solid var(--divider);
    background: var(--surface);
    font-size: 13px;
  }
  .cast-label {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .cast-note.error {
    color: var(--red);
  }
  .cast-note button {
    padding: 0;
    border: 0;
    background: none;
    color: var(--accent);
    font-weight: 600;
  }
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 20;
    padding: 0;
    border: 0;
    background: rgb(0 0 0 / 0.55);
  }
  .sheet {
    position: fixed;
    z-index: 21;
    left: 0;
    right: 0;
    bottom: 0;
    max-height: 70vh;
    overflow-y: auto;
    overscroll-behavior: contain;
    padding: 12px var(--gutter) calc(12px + env(safe-area-inset-bottom));
    border-top: 1px solid var(--divider);
    border-radius: 8px 8px 0 0;
    background: var(--surface);
    box-shadow: 0 -8px 30px rgb(0 0 0 / 0.4);
  }
  .sheet-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 8px;
  }
  .sheet-head h2 {
    margin: 0;
    font-size: 17px;
    font-weight: 600;
  }
  .sheet-close {
    display: grid;
    place-items: center;
    width: 40px;
    height: 40px;
    margin-right: -8px;
    padding: 0;
    border: 0;
    border-radius: 50%;
    background: none;
    color: var(--muted);
  }
  .sheet-note {
    margin: 8px 0;
    font-size: 13px;
  }
  .sheet-note.error {
    color: var(--red);
  }
  .devices {
    display: flex;
    flex-direction: column;
  }
  .device {
    display: flex;
    align-items: center;
    gap: 12px;
    width: 100%;
    padding: 12px 0;
    border: 0;
    border-bottom: 1px solid var(--divider);
    background: none;
    text-align: left;
  }
  .device:disabled {
    opacity: 0.5;
  }
  .device-text {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .device-name,
  .device-desc {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .device-desc {
    font-size: 12px;
  }
  .stop-sheet {
    width: 100%;
    margin-top: 12px;
    padding: 12px;
    border: 1px solid var(--divider);
    border-radius: 2px;
    background: var(--surface-2);
    color: var(--red);
    font-weight: 600;
  }
  .cast-note button,
  .sheet-close,
  .device,
  .stop-sheet {
    cursor: pointer;
  }
</style>
