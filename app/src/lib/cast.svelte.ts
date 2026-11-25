// Chromecast: the phone repackages the mirror as HLS (src-tauri/src/cast.rs) and the native
// Cast SDK (CastBridge.kt) asks a TV to play it. This store is the JavaScript half: it polls
// the synchronous native getters and starts/stops the Rust pipeline around the session.

import { invoke } from "@tauri-apps/api/core";

/** A Cast receiver as reported by the native MediaRouter. */
export interface CastDevice {
  id: string;
  name: string;
  description: string;
}

/** `FrameMateCast.castStatus()`: the receiver session, not the HLS pipeline. */
export interface CastStatus {
  available: boolean;
  connected: boolean;
  device: string | null;
  playing: boolean;
  error: string | null;
}

/** What `cast_start` returns: the URL the receiver loads, and the phone's LAN address. */
export interface CastInfo {
  url: string;
  address: string;
}

/** Discovery and playback state change on the TV's schedule, not ours. */
const POLL_MS = 2000;

class Cast {
  /** The native bridge exists (Android build with the Cast SDK). */
  supported = $state(false);
  /** The Cast SDK finished initialising; false until then, or without Play Services. */
  available = $state(false);
  devices = $state<CastDevice[]>([]);
  status = $state<CastStatus | null>(null);
  /** The Rust HLS server, once a cast has started. */
  serving = $state<CastInfo | null>(null);
  busy = $state(false);
  error = $state<string | null>(null);

  #timer: ReturnType<typeof setInterval> | undefined;
  /** Invalidates an in-flight `play()` when the page is left or casting is stopped. */
  #generation = 0;
  #lastNativeError: string | null = null;

  /** A receiver session exists (the route may still be connecting). */
  get casting() {
    return this.status?.connected ?? false;
  }

  get playing() {
    return this.status?.playing ?? false;
  }

  get deviceName() {
    return this.status?.device;
  }

  /** Starts polling and picks up a session that was already running. Safe to call again. */
  init() {
    this.supported = !!window.FrameMateCast;
    if (!this.supported || this.#timer !== undefined) return;
    this.#sync();
    this.#timer = setInterval(() => this.#sync(), POLL_MS);
  }

  /** Fresh devices/status now, e.g. when the picker opens. */
  refresh() {
    this.#sync();
  }

  #sync() {
    const native = window.FrameMateCast;
    if (!native) return;
    try {
      this.available = native.castAvailable();
      const devices = JSON.parse(native.castDevices()) as CastDevice[];
      // The router can report one receiver through more than one route.
      this.devices = devices.filter(
        (device, index) => devices.findIndex(other => other.id === device.id) === index,
      );
      this.status = JSON.parse(native.castStatus()) as CastStatus;
    } catch {
      // The interface only goes away with the WebView; there is nothing useful to show.
      return;
    }
    // Native errors are sticky until the next castPlay. Show each one once, so Dismiss sticks.
    const nativeError = this.status?.error ?? null;
    if (nativeError && nativeError !== this.#lastNativeError) this.error = nativeError;
    this.#lastNativeError = nativeError;
  }

  /** Serves the mirror and asks `device` to play it. Failures end up in `error`. */
  async play(device: CastDevice) {
    if (this.busy) return;
    const generation = ++this.#generation;
    this.busy = true;
    this.error = null;
    try {
      const info = await invoke<CastInfo>("cast_start");
      if (generation !== this.#generation) {
        // The page was left (or Stop was pressed) while the first segment was awaited.
        await invoke("cast_stop").catch(() => {});
        return;
      }
      this.serving = info;
      window.FrameMateCast?.castPlay(device.id, info.url, "FrameMate Mirror");
    } catch (failure) {
      if (generation === this.#generation) this.error = String(failure);
      await invoke("cast_stop").catch(() => {});
    } finally {
      if (generation === this.#generation) this.busy = false;
    }
  }

  /** Ends the TV session and stops serving, e.g. from a Stop button. */
  stop() {
    this.#generation++;
    this.busy = false;
    this.#lastNativeError = null;
    window.FrameMateCast?.castStop();
    this.serving = null;
    this.error = null;
    void invoke("cast_stop").catch(() => {});
  }

  /** Leaving the page: stop casting and polling; the Frame should stop capturing for the TV. */
  halt() {
    this.stop();
    clearInterval(this.#timer);
    this.#timer = undefined;
    this.devices = [];
    this.status = null;
  }

  dismissError() {
    this.error = null;
  }
}

export const cast = new Cast();
