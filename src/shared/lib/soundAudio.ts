type AudioContextCtor = typeof AudioContext;

let sharedCtx: AudioContext | null = null;
let unlockBound = false;
let unlocked = false;
let soundWanted = false;
let suspendTimer: number | null = null;

// A running AudioContext holds an output stream open on the system audio device
// for as long as it lives. In a WKWebView that makes TieZ contend for the active
// audio route even while it is minimized and plays nothing, which on macOS can
// leave other apps silent. So the context is only created once sound effects are
// actually enabled, and it is suspended again whenever no beep is pending.
const SUSPEND_AFTER_MS = 2000;

const getAudioContextCtor = (): AudioContextCtor | null => {
  if (typeof window === "undefined") return null;
  return (
    window.AudioContext ||
    (window as Window & { webkitAudioContext?: AudioContextCtor }).webkitAudioContext ||
    null
  );
};

export const getSoundAudioContext = (): AudioContext | null => {
  const Ctor = getAudioContextCtor();
  if (!Ctor) return null;

  if (!sharedCtx || sharedCtx.state === "closed") {
    sharedCtx = new Ctor();
  }
  return sharedCtx;
};

export const suspendSoundAudioContext = async (): Promise<void> => {
  if (suspendTimer !== null) {
    clearTimeout(suspendTimer);
    suspendTimer = null;
  }

  const ctx = sharedCtx;
  if (!ctx || ctx.state !== "running") return;

  try {
    await ctx.suspend();
  } catch {
    // best-effort: a context that refuses to suspend is not worth failing over
  }
};

/** Give the output device back once the current burst of beeps is over. */
export const scheduleSoundAudioSuspend = (): void => {
  if (suspendTimer !== null) clearTimeout(suspendTimer);
  suspendTimer = window.setTimeout(() => {
    suspendTimer = null;
    void suspendSoundAudioContext();
  }, SUSPEND_AFTER_MS);
};

/**
 * Track whether sound effects are switched on. With them off, nothing here ever
 * builds a context, so TieZ never takes part in audio-route arbitration.
 */
export const setSoundEffectsWanted = (wanted: boolean): void => {
  soundWanted = wanted;
  if (!wanted) void suspendSoundAudioContext();
};

/**
 * Resume Web Audio after a user gesture (required on macOS WKWebView), then
 * suspend straight away. WebKit only needs one gesture-backed start per
 * context; after that a programmatic resume works, so there is no reason to sit
 * on the output device in between.
 */
export const unlockSoundAudioContext = async (): Promise<void> => {
  if (!soundWanted || unlocked) return;

  const ctx = getSoundAudioContext();
  if (!ctx) return;

  try {
    await ctx.resume();
  } catch {
    return; // will retry on the next gesture
  }

  if (ctx.state === "closed") return;

  try {
    const buffer = ctx.createBuffer(1, 1, ctx.sampleRate || 44100);
    const source = ctx.createBufferSource();
    source.buffer = buffer;
    source.connect(ctx.destination);
    source.start(0);
    source.stop(0);
  } catch {
    // silent priming is best-effort
  }

  unlocked = true;
  await suspendSoundAudioContext();
};

export const ensureSoundAudioRunning = async (): Promise<boolean> => {
  if (!soundWanted) return false;

  const ctx = getSoundAudioContext();
  if (!ctx) return false;
  if (ctx.state === "running") return true;
  if (ctx.state === "closed") return false;

  try {
    await ctx.resume();
  } catch {
    return false;
  }

  // Sound effects may have been switched off while resume() was pending. The
  // suspend that ran at that moment saw a suspended context and did nothing,
  // so undo the resume here instead of letting a stale request beep and leave
  // the context running.
  if (!soundWanted) {
    await suspendSoundAudioContext();
    return false;
  }
  return ctx.state !== "suspended" && ctx.state !== "closed";
};

export const bindSoundAudioUnlock = (): void => {
  if (unlockBound || typeof document === "undefined") return;
  unlockBound = true;

  const onGesture = () => {
    void unlockSoundAudioContext();
  };

  document.addEventListener("pointerdown", onGesture, true);
  document.addEventListener("keydown", onGesture, true);
};
