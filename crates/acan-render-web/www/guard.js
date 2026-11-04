// What the page will not send. A rewound map is a picture of an earlier moment, so a command sent
// from it would act on a state that no longer holds. A map that is still mapping its codebase has
// no index to act on, so no command can land yet either. Each control that changes the origin's
// state asks here, and shows the reason in place. It has no DOM, so Node can test it.

export const HISTORY_REASONS = {
  send: "Go live to send.",
  answer: "Go live to answer.",
  settle: "Go live to settle.",
  adopt: "Go live to adopt.",
  seal: "Go live to seal.",
  strike: "Go live to strike.",
  cancel: "Go live to cancel.",
};

// The reason every command waits, while the first scan of the codebase runs. Null once it has mapped.
let mappingReason = null;

// Sets the reason writes wait for the first scan, or clears it with null. See `scanview.js`.
export function setMappingBlock(reason) {
  mappingReason = reason;
}

// The reason `command` cannot run now, or null when it can. `held` is whether the scrubber holds
// the map in the past. A mapping block comes first, whatever the scrubber shows.
export function historyBlock(command, held) {
  const reason = HISTORY_REASONS[command];
  if (reason === undefined) throw new Error(`no history rule for the command ${command}`);
  if (mappingReason !== null) return mappingReason;
  return held ? reason : null;
}
