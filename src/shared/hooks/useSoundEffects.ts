import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";
import { isTauriRuntime } from "../lib/tauriRuntime";
import {
  bindSoundAudioUnlock,
  ensureSoundAudioRunning,
  getSoundAudioContext,
  scheduleSoundAudioSuspend,
  setSoundEffectsWanted,
  suspendSoundAudioContext,
  unlockSoundAudioContext
} from "../lib/soundAudio";

interface UseSoundEffectsOptions {
  soundEnabled: boolean;
  pasteSoundEnabled: boolean;
  soundVolume: number;
}

export const useSoundEffects = ({
  soundEnabled,
  pasteSoundEnabled,
  soundVolume
}: UseSoundEffectsOptions) => {
  useEffect(() => {
    if (!isTauriRuntime()) return;

    // With sound effects off, no context is built at all, so TieZ never joins
    // audio-route arbitration.
    setSoundEffectsWanted(soundEnabled);
    if (!soundEnabled) return;

    bindSoundAudioUnlock();
    void unlockSoundAudioContext();

    const playCrispBeep = (
      ctx: AudioContext,
      durationSec = 0.1,
      baseFreqHz = 1400,
      volume = 0.35
    ) => {
      if (ctx.state === "suspended") void ctx.resume();

      const t0 = ctx.currentTime;
      const tEnd = t0 + Math.max(0.05, durationSec);

      const osc = ctx.createOscillator();
      osc.type = "triangle";

      osc.frequency.setValueAtTime(baseFreqHz * 1.25, t0);
      osc.frequency.exponentialRampToValueAtTime(
        Math.max(80, baseFreqHz * 0.92),
        t0 + Math.min(0.18, durationSec * 0.25)
      );

      const filter = ctx.createBiquadFilter();
      filter.type = "bandpass";
      filter.frequency.setValueAtTime(Math.min(4000, baseFreqHz * 1.3), t0);
      filter.Q.setValueAtTime(6, t0);

      const gain = ctx.createGain();
      gain.gain.setValueAtTime(0.0001, t0);
      gain.gain.exponentialRampToValueAtTime(Math.max(0.0001, volume), t0 + 0.004);

      const mid = t0 + Math.min(0.08, durationSec * 0.2);
      gain.gain.exponentialRampToValueAtTime(Math.max(0.0001, volume * 0.35), mid);
      gain.gain.exponentialRampToValueAtTime(0.0001, tEnd);

      const noiseDur = Math.min(0.03, durationSec * 0.1);
      const sampleRate = ctx.sampleRate || 44100;
      const bufferSize = Math.floor(sampleRate * noiseDur);

      let noiseBuf: AudioBuffer | undefined;
      try {
        noiseBuf = ctx.createBuffer(1, bufferSize > 0 ? bufferSize : 1, sampleRate);
        const data = noiseBuf.getChannelData(0);
        for (let i = 0; i < data.length; i++) {
          const decay = 1 - i / data.length;
          data[i] = (Math.random() * 2 - 1) * decay;
        }
      } catch (e) {
        console.error("Audio buffer error", e);
      }

      if (noiseBuf) {
        const noiseNode = ctx.createBufferSource();
        noiseNode.buffer = noiseBuf;

        const noiseHP = ctx.createBiquadFilter();
        noiseHP.type = "highpass";
        noiseHP.frequency.setValueAtTime(1500, t0);

        const noiseGain = ctx.createGain();
        noiseGain.gain.setValueAtTime(Math.max(0.0001, volume * 0.25), t0);
        noiseGain.gain.exponentialRampToValueAtTime(0.0001, t0 + noiseDur);

        noiseNode.connect(noiseHP);
        noiseHP.connect(noiseGain);
        noiseGain.connect(ctx.destination);

        noiseNode.start(t0);
        noiseNode.stop(t0 + noiseDur);
      }

      osc.connect(filter);
      filter.connect(gain);
      gain.connect(ctx.destination);

      osc.start(t0);
      osc.stop(tEnd + 0.01);
    };

    // The paste tail beep is scheduled 110 ms out. Without tracking it, a
    // teardown in that window (sound switched off, volume changed) only suspends
    // the context, and the callback then resumes it again for a stale beep.
    let cancelled = false;
    const pendingTails = new Set<ReturnType<typeof setTimeout>>();

    const unlisten = listen<string>("play-sound", (event) => {
      const type = event.payload;
      if (type === "paste" && !pasteSoundEnabled) return;
      const masterVol = Math.min(1, Math.max(0, soundVolume));

      const play = (ctx: AudioContext) => {
        try {
          if (type === "copy") {
            playCrispBeep(ctx, 0.06, 500, Math.min(1, masterVol * 0.8));
          } else if (type === "paste") {
            playCrispBeep(ctx, 0.09, 950, Math.min(1, masterVol * 0.9));
            const tail = setTimeout(() => {
              pendingTails.delete(tail);
              if (cancelled || ctx.state === "closed") return;
              playCrispBeep(ctx, 0.075, 1150, Math.min(1, masterVol * 0.75));
              scheduleSoundAudioSuspend();
            }, 110);
            pendingTails.add(tail);
          }
          scheduleSoundAudioSuspend();
        } catch (e) {
          console.error("Sound play error", e);
        }
      };

      void ensureSoundAudioRunning().then((running) => {
        if (!running || cancelled) return;
        const ctx = getSoundAudioContext();
        if (ctx) play(ctx);
      });
    });

    return () => {
      cancelled = true;
      pendingTails.forEach(clearTimeout);
      pendingTails.clear();
      unlisten.then((f) => f());
      void suspendSoundAudioContext();
    };
  }, [soundEnabled, pasteSoundEnabled, soundVolume]);
};
