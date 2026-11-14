import { test } from "bun:test";
import assert from "node:assert/strict";

import { FEED_BOTTOM_SLACK, feedAtBottom, feedScrollIntent } from "../src/lib/feed.js";

function view({ height = 1_000, top = 0, viewport = 400 }) {
  return { scrollHeight: height, scrollTop: top, clientHeight: viewport };
}

test("feedAtBottom treats a short or empty feed as already at the newest line", () => {
  assert.equal(feedAtBottom(view({ height: 400, top: 0 })), true);
  assert.equal(feedAtBottom(null), false, "no mounted feed is not a bottom claim");
  assert.equal(feedAtBottom(view({ height: 1_000, top: 1_000 - 400 - (FEED_BOTTOM_SLACK - 1) })), true);
  assert.equal(feedAtBottom(view({ height: 1_000, top: 1_000 - 400 - FEED_BOTTOM_SLACK })), false);
});

test("content growth never pauses the tail", () => {
  // A queued programmatic scroll fires after two new lines have grown the
  // feed: the offset is unchanged, so this is not the user leaving the line.
  assert.equal(
    feedScrollIntent({ top: 500, previous: 500, gesture: false, atBottom: false, tailing: true }),
    null
  );
  // The same event can arrive while the wheel is still warm from a downwards
  // scroll; the offset did not move up, so it still must not pause.
  assert.equal(
    feedScrollIntent({ top: 500, previous: 500, gesture: true, atBottom: false, tailing: true }),
    null
  );
});

test("a programmatic pin moves the offset down and is ignored", () => {
  assert.equal(
    feedScrollIntent({ top: 600, previous: 500, gesture: true, atBottom: false, tailing: true }),
    null
  );
});

test("layout shifts that move the offset up without a gesture are ignored", () => {
  // Scroll anchoring when the 120-line window drops a row, a resize clamp, or
  // a font swap can all reduce scrollTop without the user asking for it.
  assert.equal(
    feedScrollIntent({ top: 400, previous: 500, gesture: false, atBottom: false, tailing: true }),
    null
  );
});

test("a real upward gesture holds the feed", () => {
  assert.equal(
    feedScrollIntent({ top: 400, previous: 500, gesture: true, atBottom: false, tailing: true }),
    "pause"
  );
  // Sub-pixel jitter is not a deliberate scroll.
  assert.equal(
    feedScrollIntent({ top: 499, previous: 500, gesture: true, atBottom: false, tailing: true }),
    null
  );
});

test("reaching the newest line re-arms a held feed only after a gesture", () => {
  assert.equal(
    feedScrollIntent({ top: 600, previous: 500, gesture: true, atBottom: true, tailing: false }),
    "resume"
  );
  // A shrink or a font swap can put the feed back at the bottom; a held line
  // must not jump on its own.
  assert.equal(
    feedScrollIntent({ top: 600, previous: 500, gesture: false, atBottom: true, tailing: false }),
    null
  );
  assert.equal(
    feedScrollIntent({ top: 600, previous: 500, gesture: true, atBottom: true, tailing: true }),
    null
  );
});
