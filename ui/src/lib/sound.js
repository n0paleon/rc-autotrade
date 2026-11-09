let enabled = localStorage.getItem("turuc.sound") !== "off";
let context = null;

/** Profit clip URLs, keyed by the profit tier they play for. */
export const PROFIT_CLIPS = Object.freeze({
  loss: "https://storage.n0pal.net/artifacts/u_oepgi4ep3v-som_matricula-464025.mp3",
  good: "https://storage.n0pal.net/artifacts/freesound_community-goodresult-82807.mp3",
  winning: "https://storage.n0pal.net/artifacts/pw23check-winning-218995.mp3"
});

/** A fill must be worth at least this many RLT to earn the "good" clip. */
export const PROFIT_CLIP_MIN_RLT = 1;
/** At or above this many RLT the fill earns the "winning" clip instead. */
export const PROFIT_CLIP_WINNING_RLT = 3;

const players = new Map();

/**
 * Picks the clip for a fill profit: a loss clip for strictly negative profit,
 * a victory clip from 1 RLT up, or null when the fill keeps the synthesized
 * fill chirp (profit from 0 up to just under 1 RLT).
 */
export function profitClip(profitRlt) {
  const profit = Number(profitRlt);
  if (!Number.isFinite(profit)) return null;
  if (profit < 0) return "loss";
  if (profit >= PROFIT_CLIP_WINNING_RLT) return "winning";
  if (profit >= PROFIT_CLIP_MIN_RLT) return "good";
  return null;
}

function player(kind) {
  const url = PROFIT_CLIPS[kind];
  if (!url) return null;
  let audio = players.get(kind);
  if (!audio) {
    audio = new Audio(url);
    audio.preload = "auto";
    audio.volume = 0.65;
    players.set(kind, audio);
  }
  return audio;
}

/** Warms the clip cache so the first fill does not wait on the network. */
function prime() {
  if (!enabled) return;
  try {
    player("loss");
    player("good");
    player("winning");
  } catch {
    /* audio is optional */
  }
}

function ensure() {
  if (!enabled) return null;
  prime();
  try {
    if (!context) {
      const AudioContext = window.AudioContext || window.webkitAudioContext;
      if (!AudioContext) return null;
      context = new AudioContext();
    }
    if (context.state === "suspended") context.resume().catch(() => {});
    return context;
  } catch {
    return null;
  }
}

function tone(frequency, duration = 0.06, delay = 0, type = "square", volume = 0.035) {
  const ctx = ensure();
  if (!ctx) return;
  try {
    const start = ctx.currentTime + delay;
    const oscillator = ctx.createOscillator();
    const gain = ctx.createGain();
    oscillator.type = type;
    oscillator.frequency.setValueAtTime(frequency, start);
    gain.gain.setValueAtTime(0.0001, start);
    gain.gain.exponentialRampToValueAtTime(volume, start + 0.008);
    gain.gain.exponentialRampToValueAtTime(0.0001, start + duration);
    oscillator.connect(gain).connect(ctx.destination);
    oscillator.start(start);
    oscillator.stop(start + duration + 0.02);
  } catch {
    /* audio is optional */
  }
}

export const sound = {
  get enabled() {
    return enabled;
  },
  setEnabled(value) {
    enabled = Boolean(value);
    localStorage.setItem("turuc.sound", enabled ? "on" : "off");
    if (enabled) sound.play("click");
  },
  ensure,
  prime,
  /**
   * Plays the clip that matches a fill's profit: the "loss" clip for negative
   * profit, the "good" clip for 1–3 RLT, the "winning" clip for 3+ RLT, and the
   * synth fill chirp from 0 up to just under 1 RLT.
   */
  playProfit(profitRlt) {
    if (!enabled) return;
    const kind = profitClip(profitRlt);
    if (!kind) {
      sound.play("fill");
      return;
    }
    try {
      const audio = player(kind);
      if (!audio) {
        sound.play("fill");
        return;
      }
      audio.currentTime = 0;
      const played = audio.play();
      if (played && typeof played.catch === "function") {
        played.catch(() => sound.play("fill"));
      }
    } catch {
      sound.play("fill");
    }
  },
  play(kind) {
    if (!enabled) return;
    if (kind === "fill") {
      tone(880, 0.05, 0);
      tone(1174, 0.05, 0.06);
      tone(1568, 0.09, 0.12);
    } else if (kind === "connect") {
      tone(523, 0.05, 0);
      tone(784, 0.07, 0.06);
    } else if (kind === "error") {
      tone(196, 0.1, 0, "sawtooth", 0.03);
      tone(147, 0.14, 0.09, "sawtooth", 0.03);
    } else {
      tone(440, 0.03, 0, "square", 0.02);
    }
  }
};
