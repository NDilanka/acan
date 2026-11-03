// The colour themes, with no DOM, so Node can test them. applyTheme sets `<html data-theme>`, and
// settings.css holds the token values for each theme.

export const THEMES = [
  { id: "paper", label: "Light paper" },
  { id: "dark", label: "Dark" },
  { id: "blueprint", label: "Blueprint" },
  { id: "high_contrast", label: "High contrast" },
];

// An id the page does not know gets the first theme.
export function applyTheme(root, id) {
  const applied = THEMES.some((theme) => theme.id === id) ? id : THEMES[0].id;
  root.dataset.theme = applied;
  return applied;
}

// The theme `step` places after `id` (1 for the next, -1 for the previous), wrapping at both ends.
// An unknown id counts as the first theme, as applyTheme treats it.
export function nextTheme(id, step = 1) {
  const count = THEMES.length;
  const at = Math.max(0, THEMES.findIndex((theme) => theme.id === id));
  return THEMES[(((at + step) % count) + count) % count].id;
}
