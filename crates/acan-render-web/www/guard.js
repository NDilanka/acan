// What the page will not send while the timeline scrubber shows the past. A rewound map is a
// picture of an earlier moment, so a command sent from it would act on a state that no longer
// holds. Each control that changes the origin's state asks here, and shows the reason in place.
// It has no DOM, so Node can test it.

export const HISTORY_REASONS = {
  send: "Go live to send.",
  answer: "Go live to answer.",
  settle: "Go live to settle.",
  adopt: "Go live to adopt.",
  seal: "Go live to seal.",
  strike: "Go live to strike.",
  cancel: "Go live to cancel.",
};

// The reason `command` cannot run now, or null when it can. `held` is whether the scrubber holds
// the map in the past.
export function historyBlock(command, held) {
  if (!held) return null;
  const reason = HISTORY_REASONS[command];
  if (reason === undefined) throw new Error(`no history rule for the command ${command}`);
  return reason;
}
