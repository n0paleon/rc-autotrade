/**
 * Tail-follow rules for the live event log.
 *
 * A native `scroll` event is not proof that the user scrolled: the browser
 * also fires one when the capped window drops its oldest row (scroll
 * anchoring), when a resize reflows the lines, and when the view itself
 * queues a programmatic scroll. Only a preceding gesture plus an actual
 * upward move may hold the feed; otherwise content churn would pause the
 * tail on its own.
 */

export const FEED_BOTTOM_SLACK = 24;

/** True while the newest line is visible, or the feed is too short to scroll. */
export function feedAtBottom(feed) {
  return Boolean(feed) && feed.scrollHeight - feed.scrollTop - feed.clientHeight < FEED_BOTTOM_SLACK;
}

/**
 * Classifies one scroll event for the feed.
 *
 * `gesture` says a wheel, touch, scrollbar drag or scroll key happened just
 * before the event. Returns `"pause"` when a gesture moved the feed up from
 * its previous offset, `"resume"` when a gesture reached the newest line,
 * and `null` for layout-driven scrolls that must not touch the tail.
 */
export function feedScrollIntent({ top, previous, gesture, atBottom, tailing }) {
  if (atBottom) return !tailing && gesture ? "resume" : null;
  if (!gesture) return null;
  return top < previous - 1 ? "pause" : null;
}
