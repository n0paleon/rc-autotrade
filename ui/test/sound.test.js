// Verifies the profit-to-clip mapping used by the fill sound:
//  * profit < 0                -> "loss" clip
//  * profit == 0 .. < 1 RLT    -> no clip (the synth fill chirp plays)
//  * 1 RLT <= profit < 3 RLT   -> "good" clip
//  * profit >= 3 RLT           -> "winning" clip
//
// sound.js reads localStorage at module scope, so the store is stubbed first.
import { test } from "bun:test";
import assert from "node:assert/strict";

globalThis.localStorage = { getItem: () => null, setItem: () => {} };

const {
  profitClip,
  PROFIT_CLIPS,
  PROFIT_CLIP_MIN_RLT,
  PROFIT_CLIP_WINNING_RLT,
  sound
} = await import("../src/lib/sound.js");

test("selects the loss clip for strictly negative profit", () => {
  assert.equal(profitClip(-0.000001), "loss");
  assert.equal(profitClip(-1.5), "loss");
  assert.equal(profitClip(-120), "loss");
  assert.equal(profitClip("-2.5"), "loss");
});

test("keeps the synth fill for zero up to just under 1 RLT", () => {
  assert.equal(profitClip(0), null);
  assert.equal(profitClip(-0), null);
  assert.equal(profitClip(0.5), null);
  assert.equal(profitClip(0.999999), null);
  assert.equal(profitClip(null), null);
  assert.equal(profitClip(undefined), null);
  assert.equal(profitClip("not a number"), null);
});

test("selects the good clip from 1 RLT up to just under 3 RLT", () => {
  assert.equal(profitClip(1), "good");
  assert.equal(profitClip(1.000001), "good");
  assert.equal(profitClip(2.5), "good");
  assert.equal(profitClip(2.999999), "good");
  assert.equal(profitClip("2.5"), "good");
});

test("selects the winning clip at 3 RLT and above", () => {
  assert.equal(profitClip(3), "winning");
  assert.equal(profitClip(3.000001), "winning");
  assert.equal(profitClip(120), "winning");
});

test("playProfit plays the clip that matches the profit tier", () => {
  const played = [];
  const previousAudio = globalThis.Audio;
  globalThis.Audio = class {
    constructor(url) {
      this.url = url;
      this.currentTime = 0;
    }
    play() {
      played.push(this.url);
      return Promise.resolve();
    }
  };

  try {
    sound.playProfit(0.5);
    sound.playProfit(0);
    sound.playProfit(0.999999);
    assert.deepEqual(played, []);

    sound.playProfit(-2);
    assert.deepEqual(played, [PROFIT_CLIPS.loss]);

    sound.playProfit(1);
    assert.deepEqual(played, [PROFIT_CLIPS.loss, PROFIT_CLIPS.good]);

    sound.playProfit(3);
    assert.deepEqual(played, [
      PROFIT_CLIPS.loss,
      PROFIT_CLIPS.good,
      PROFIT_CLIPS.winning
    ]);
  } finally {
    globalThis.Audio = previousAudio;
  }
});

test("thresholds and clip URLs are the documented ones", () => {
  assert.equal(PROFIT_CLIP_MIN_RLT, 1);
  assert.equal(PROFIT_CLIP_WINNING_RLT, 3);
  assert.equal(
    PROFIT_CLIPS.loss,
    "https://storage.n0pal.net/artifacts/u_oepgi4ep3v-som_matricula-464025.mp3"
  );
  assert.equal(
    PROFIT_CLIPS.good,
    "https://storage.n0pal.net/artifacts/freesound_community-goodresult-82807.mp3"
  );
  assert.equal(
    PROFIT_CLIPS.winning,
    "https://storage.n0pal.net/artifacts/pw23check-winning-218995.mp3"
  );
});
