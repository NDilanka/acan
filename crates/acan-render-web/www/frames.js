// The frame loop's rule, kept apart from the page so it can be tested without a browser.

// What the loop does once a frame is drawn: "stop", "now" for the next display frame, or a wait
// in ms before the next one. `moving` is whether the app asked for another frame, `hidden` is
// `document.hidden`, and `gapMs` is the app's frame gap. A hidden tab stops the loop, so it asks
// for no frames until it shows again.
export function nextFrame({ moving, hidden, gapMs }) {
  if (!moving || hidden) return "stop";
  return gapMs > 0 ? gapMs : "now";
}
