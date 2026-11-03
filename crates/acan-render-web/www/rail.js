// The rail's tabs. At rest the rail shows only what needs the reader: the clash card, the slip,
// the needs-you inbox, and the checklist while setup runs. Agents, Orders and Timeline wait
// behind these tabs; a click opens one and closes the others. The panels' paints agree through
// `isRailOpen`, so a repaint never reopens a closed tab.
const TABS = [
  ["lanes", "Agents"],
  ["history", "Orders"],
  ["scrub", "Timeline"],
];

const open = new Set();

// Whether the tab for the panel `id` is open.
export function isRailOpen(id) {
  return open.has(id);
}

// Binds the tab bar in `#rail-tabs`. `kit` carries requestRender, so the map's names move out of
// the way of a panel that opens.
export function installRailTabs(kit) {
  const bar = document.getElementById("rail-tabs");
  if (bar === null) return;
  const buttons = new Map();
  for (const [id, label] of TABS) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = label;
    button.setAttribute("aria-controls", id);
    button.setAttribute("aria-expanded", "false");
    button.addEventListener("click", () => {
      const on = !open.has(id);
      open.clear();
      if (on) open.add(id);
      for (const [tab, other] of buttons) {
        other.setAttribute("aria-expanded", String(tab === id && on));
        const panel = document.getElementById(tab);
        if (panel !== null) panel.hidden = !(tab === id && on);
      }
      kit.requestRender();
    });
    buttons.set(id, button);
    bar.append(button);
  }
}

// Opens the panel's tab, as a key that goes to a hidden panel does. Used by the G chords.
export function openRailTab(id) {
  if (!open.has(id)) {
    open.clear();
    open.add(id);
    const panel = document.getElementById(id);
    if (panel !== null) panel.hidden = false;
    document.querySelector(`#rail-tabs button[aria-controls="${id}"]`)?.setAttribute("aria-expanded", "true");
  }
}
