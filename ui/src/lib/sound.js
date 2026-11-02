let enabled = localStorage.getItem("turuc.sound") !== "off";
let context = null;

function ensure() {
  if (!enabled) return null;
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
