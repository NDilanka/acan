// View for tla/StoppedLane.tla, read by the statechart command with --view.
// Acan's own file. It is outside the Cargo workspace and is not product code.
// The chart follows task a. Task b is not drawn, because the reader's words here are about a.

export const title = "StoppedLane: a stopped lane stops building and ends its leases (task a)";

export const colors = { Running: "blue", Stopped: "orange" };

export const group = (v) => (v.stopped ? "Stopped" : "Running");

export const state = (v) => {
  const stage = v.stage.a;
  const stop = v.stopped;
  const lease = v.lease.a;
  if (stage === "landed") return stop ? "Landed, lane stopped" : "Landed";
  if (stage === "gating") return stop ? "Stopped, receipt pending" : "Built, receipt pending";
  if (stage === "building") return stop ? "Stopped, still building" : "Building";
  if (stage === "declared") return stop ? "Stopped, claim held, not built" : "Claimed, waiting for build";
  if (stage === "open") {
    if (stop) return lease ? "Stopped, lease still held" : "Stopped, task open";
    return lease ? "Open, lease still held" : "Open";
  }
  return stage;
};

const labels = {
  Claim: "agent claims",
  Clear: "origin clears the build",
  Built: "agent reports built",
  Pass: "receipt passes",
  Fail: "receipt fails",
  Stop: "budget runs out",
  Release: "agent releases",
  Unstop: "unstop",
};

export const step = (action) => labels[action] ?? null;
