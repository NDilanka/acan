// View for tla/LaneBudget.tla, read by the statechart command with --view.
// Acan's own file. It is outside the Cargo workspace and is not product code.
// repairRounds must match RepairRounds in the cfg files (2 in each of them).

const repairRounds = 2;

export const title = "LaneBudget: a lane stops only after its repair rounds run out (two items)";

export const colors = { Running: "blue", Stopped: "orange" };

export const group = (v) => (v.stopped ? "Stopped" : "Running");

// The items are interchangeable, so the label counts them. Names would split one state into many.
const count = (v, stage) => Object.keys(v.ep).filter((x) => v.ep[x] === stage).length;

export const state = (v) => {
  const residue = Object.keys(v.ep).filter((x) => v.ep[x] === "residue");
  const ruledResidue = residue.filter((x) => v.ruled.includes(x)).length;
  const items = [];
  if (count(v, "open")) items.push(`${count(v, "open")} open`);
  if (count(v, "bounced")) items.push(`${count(v, "bounced")} bounced`);
  if (residue.length - ruledResidue) items.push(`${residue.length - ruledResidue} residue`);
  if (ruledResidue) items.push(`${ruledResidue} ruled residue`);
  if (count(v, "stopped")) items.push(`${count(v, "stopped")} unrepaired at stop`);
  const head = items.length ? items.join(", ") : "No open items";
  return [head, `${v.rounds} of ${repairRounds} rounds used`, v.stopped ? "Lane stopped" : "Lane running"].join(". ");
};

const labels = {
  Rule: "owner rules",
  Unstop: "owner unstops the lane",
};

export const step = (action, from, to) => {
  if (action !== "Receipt") return labels[action] ?? null;
  if (to.stopped && !from.stopped) return "budget runs out";
  if (to.rounds > from.rounds) return "receipt bounces items [up to the cap]";
  const leaves = Object.keys(to.ep).some((x) => to.ep[x] === "residue" && from.ep[x] !== "residue");
  return leaves ? "receipt leaves residue" : "receipt passes";
};
