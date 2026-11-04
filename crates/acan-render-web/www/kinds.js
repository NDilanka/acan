// The words and requests of the Set kind menu, with no DOM and no imports, so Node can test them.
// A right click on a machine lists the kinds it can take. A choice writes it through
// PUT /api/plant/kinds, which only the owner may call. The menu itself is in kindmenu.js.

// The kinds by wire name, in the order the server lists them. The last one is the fallback.
export const KIND_NAMES = [
  "intake", "gate", "tank", "splitter", "valve", "crate", "switchboard", "kiosk", "mould",
  "conveyor", "horn", "switchbox", "gantry", "console", "antenna", "toolrack", "assembler",
];

// A kind's name in the menu: its wire name with a capital, as the page names kinds elsewhere.
export function kindWords(name) {
  return name.charAt(0).toUpperCase() + name.slice(1);
}

// The rows of the menu for a machine whose kind is `current`, or "" when the plant names none.
// There is one row per kind, then a row that clears the choice so the name rule decides again.
// A row is `{ kind, label, current }`, and `kind` is null on the clearing row.
export function menuRows(current) {
  const rows = KIND_NAMES.map((kind) => ({ kind, label: kindWords(kind), current: kind === current }));
  rows.push({ kind: null, label: "Use the name rule", current: false });
  return rows;
}

// The PUT body that sets `kind` on the machine called `name`, or clears it for null.
export function kindBody(name, kind) {
  return { [name]: kind };
}

// Whether this page may write a kind. A replayed page has no token, and a held map shows the past,
// so neither offers the menu.
export function canSetKind({ replay, held, token }) {
  return Boolean(token) && !replay && !held;
}

// The sentence a PUT that did not take the kind shows, or null when it did (200). A refusal
// recorded nothing. A failure, such as status 0 for no answer or 5xx, may have recorded it.
export function setKindRefusal(status) {
  if (status === 200) return null;
  if (status === 401) return "The owner token was not accepted, so nothing changed. Sign in again.";
  if (status === 403) return "Only the owner can set a machine's kind. Nothing changed.";
  if (status === 404) return "That machine is not in the plant any more. Nothing changed.";
  if (status === 400) return "This acan does not know that kind. Nothing changed.";
  if (status === 0 || status >= 500) return "The server did not answer, so the kind may not be set. Try again.";
  return "The server refused the change. Nothing changed.";
}

// Where a menu of `width` by `height` opens at (x, y) in a window `bounds` wide and high, moved in
// so it stays on screen with a margin of 4 px.
export function menuPlace(x, y, width, height, bounds) {
  const margin = 4;
  return {
    left: Math.max(margin, Math.min(x, bounds.width - width - margin)),
    top: Math.max(margin, Math.min(y, bounds.height - height - margin)),
  };
}
