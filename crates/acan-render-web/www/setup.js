// The model key setup, as the app shows it. No DOM, so Node can test it. The server answers
// GET /api/setup, POST /api/setup/keys, DELETE /api/setup/keys/{provider} and
// PUT /api/setup/roles/{role}; this file says what each answer means. The key is never in this file:
// the page reads it from its field only to send it, and does not keep or show it.
//
// The roles come from the server's answer, in its order. The page names no role itself, so a role
// the server adds later gets its own row with no change here.

const LABELS = { meta: "Meta Model API", baseten: "Baseten", fireworks: "Fireworks" };

export function providerLabel(name) {
  return LABELS[name] ?? String(name);
}

function providersOf(status) {
  const list = Array.isArray(status?.providers) ? status.providers : [];
  return list.filter((provider) => typeof provider?.name === "string" && provider.name !== "");
}

function rolesOf(status) {
  const list = Array.isArray(status?.roles) ? status.roles : [];
  return list.filter((role) => typeof role?.role === "string" && role.role !== "");
}

function capital(text) {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

function listWords(words) {
  if (words.length <= 1) return words.join("");
  return `${words.slice(0, -1).join(", ")} and ${words[words.length - 1]}`;
}

// The providers the key form offers, from the server's list, in the server's order.
export function providerChoices(status) {
  return providersOf(status).map(({ name }) => ({ value: name, label: providerLabel(name) }));
}

// Where a provider's key comes from, in words.
export function sourceText(source) {
  if (source === "environment") return "Set in the environment. Unset it there to remove it.";
  if (source === "saved") return "Saved on this page.";
  return "No key.";
}

// One row per provider, with where its key comes from. Only a key saved on this page can be removed
// here: the server refuses to remove one the environment sets.
export function keyRows(status) {
  const canSave = status?.can_save === true;
  return providersOf(status).map(({ name, source }) => {
    const from = source === "environment" || source === "saved" ? source : null;
    return {
      provider: name,
      label: providerLabel(name),
      source: from,
      text: sourceText(from),
      removable: canSave && from === "saved",
    };
  });
}

// The providers a role can be given: those with a key, from either source.
export function pickChoices(status) {
  return providersOf(status)
    .filter(({ source }) => source === "environment" || source === "saved")
    .map(({ name }) => ({ value: name, label: providerLabel(name) }));
}

// One row per role the server lists. `value` is the picked provider, or "" when the role takes its
// default. The choices always hold the role's own pick, so the picker shows what the server holds.
// A role marked meta_only is offered Meta alone, and no picker when Meta has no key. A server that
// sends no meta_only gets every keyed provider, and its 400 needs_meta answer explains a wrong pick.
export function roleRows(status) {
  const choices = pickChoices(status);
  const meta = choices.find((choice) => choice.value === "meta") ?? null;
  const routed = status?.source === "routing_file";
  return rolesOf(status).map(({ role, provider, picked, off_reason: offReason, meta_only: metaOnly }) => {
    const value = picked === true && typeof provider === "string" ? provider : "";
    let now;
    if (typeof offReason === "string" && offReason !== "") now = `Off: ${offReason}`;
    else if (typeof provider !== "string") now = "Off";
    else if (routed) now = `${providerLabel(provider)}, from the routing file`;
    else now = picked === true ? `${providerLabel(provider)}, picked` : `${providerLabel(provider)}, by default`;
    const off = typeof provider !== "string" || (typeof offReason === "string" && offReason !== "");
    if (metaOnly === true && meta === null) {
      // One sentence: the server's off reason when it gives one, which already says this.
      const said = typeof offReason === "string" && offReason !== "";
      return { role, value: "", options: [], now: said ? now : `${capital(role)} needs a Meta key.`, off: true, picker: false, note: null };
    }
    const own = metaOnly === true ? [meta] : choices;
    const options = [{ value: "", label: "Default" }, ...own];
    if (value !== "" && !options.some((option) => option.value === value)) {
      options.push({ value, label: providerLabel(value) });
    }
    return { role, value, options, now, off, picker: true, note: null };
  });
}

// The status line for GET /api/setup, and whether a key is set.
export function statusText(status) {
  if (status?.source === "routing_file") {
    return status.configured
      ? { text: "This repository's .acan/llm.toml chooses the providers, and their keys are set.", set: true }
      : { text: "This repository's .acan/llm.toml chooses the providers, but no key is set for them. Set their keys in its .env.", set: false };
  }
  if (status?.configured) {
    const used = [];
    for (const { provider } of rolesOf(status)) {
      if (typeof provider === "string" && !used.includes(provider)) used.push(provider);
    }
    return used.length === 0
      ? { text: "Model calls are set up.", set: true }
      : { text: `Model calls use ${listWords(used.map(providerLabel))}. Each role's provider is below.`, set: true };
  }
  return { text: "No model key is set. Model jobs get built-in replies until you add one.", set: false };
}

// The banner shows when no key is in use. Where a routing file decides the providers, the banner
// says so and offers no form: the keys are set in the repository's .env.
export function showBanner(status) {
  return status?.configured === false;
}

export function bannerText(status) {
  return status?.can_save === true
    ? "No model key is set. Model jobs get built-in replies."
    : "This repository's .acan/llm.toml chooses the providers, but no key is set for them. Model jobs get built-in replies until its .env sets one.";
}

// What a save answer says. The key is not in the answer, so no message can show it.
export function saveOutcome(httpStatus, body) {
  const label = providerLabel(body?.provider);
  if (httpStatus === 200) {
    return body?.checked === false
      ? { tone: "warn", text: `Saved, but ${label} did not confirm the key. The first model call will show whether it works.` }
      : { tone: "ok", text: `Saved. ${label} accepted the key, and acan will use it from now on.` };
  }
  if (httpStatus === 422) {
    return { tone: "bad", text: `${label} did not accept this key. Check it and try again. Nothing was saved.` };
  }
  if (httpStatus === 503) {
    return { tone: "bad", text: `Could not reach ${label}. Check the connection and try again. Nothing was saved.` };
  }
  if (httpStatus === 400) {
    return { tone: "bad", text: "That provider or key was not accepted. Paste the whole key, with no spaces." };
  }
  if (httpStatus === 401) {
    return { tone: "bad", text: "This session is not signed in, so the key cannot be saved." };
  }
  if (httpStatus === 403) {
    return { tone: "bad", text: "Only members can set a key, so this session cannot." };
  }
  if (httpStatus === 409 && body?.what === "routing_file") {
    return { tone: "bad", text: "This repository's .acan/llm.toml chooses the providers. Set their keys in its .env." };
  }
  if (httpStatus === 409) {
    return { tone: "bad", text: "This acan cannot save a key from this page." };
  }
  return { tone: "bad", text: "Something went wrong. Nothing was saved." };
}

// The 409 a remove gets when the environment sets the key, as the server words it. A page drawn
// before the environment set the key still offers Remove, so this answer can reach it.
const ENVIRONMENT_KEY = "this key comes from the environment";

// The server's own words for a refused remove, as one sentence. The 409 codes `routing_file` and
// `unavailable` are not words, and anything that is not a short plain phrase is not shown.
function serverWords(what) {
  if (typeof what !== "string") return null;
  const words = what.trim();
  if (words.length === 0 || words.length > 160 || !/^[A-Za-z][A-Za-z ,'-]*\.?$/.test(words) || !words.includes(" ")) {
    return null;
  }
  return capital(words.endsWith(".") ? words : `${words}.`);
}

// What a remove answer says. `provider` is the one the tester asked to remove.
export function removeOutcome(httpStatus, body, provider) {
  const label = providerLabel(provider);
  if (httpStatus === 200) {
    return body?.removed === false
      ? { tone: "ok", text: `${label} had no saved key, so nothing was removed.` }
      : { tone: "ok", text: `The ${label} key is removed. Roles that were picked for it take their default again.` };
  }
  if (httpStatus === 409 && body?.what === "routing_file") {
    return { tone: "bad", text: "This repository's .acan/llm.toml chooses the providers, so keys cannot be removed here." };
  }
  if (httpStatus === 409 && typeof body?.what === "string" && body.what.trim().toLowerCase() === ENVIRONMENT_KEY) {
    return { tone: "bad", text: "This key comes from the environment, so it can't be removed here." };
  }
  if (httpStatus === 409) {
    const words = serverWords(body?.what);
    return { tone: "bad", text: words ?? "This acan cannot remove a key from this page." };
  }
  if (httpStatus === 400) {
    return { tone: "bad", text: "acan does not offer that provider, so nothing was removed." };
  }
  if (httpStatus === 401) {
    return { tone: "bad", text: "This session is not signed in, so the key cannot be removed." };
  }
  if (httpStatus === 403) {
    return { tone: "bad", text: "Only members can remove a key, so this session cannot." };
  }
  return { tone: "bad", text: "Something went wrong. Nothing was removed." };
}

// Where focus goes after a remove answer: to the result sentence when the key went, because its
// Remove button is gone with it, or null to return to the control that had focus.
export function focusAfterRemove(httpStatus) {
  return httpStatus === 200 ? "setup-changes" : null;
}

// What a role pick answer says. `role` is the name the API lists for the role, and `requested` is
// the provider the tester picked, or null for the default. The answers that refuse a pick do not
// name the provider, so the message names the one requested.
export function roleOutcome(httpStatus, body, role, requested = null) {
  const name = capital(String(role));
  if (httpStatus === 200) {
    const provider = typeof body?.provider === "string" ? body.provider : null;
    return provider === null
      ? { tone: "ok", text: `${name} takes its default again.` }
      : { tone: "ok", text: `${name} now uses ${providerLabel(provider)}.` };
  }
  if (httpStatus === 400 && body?.what === "needs_meta") {
    return { tone: "bad", text: `${name} runs on ${providerLabel("meta")} only, so pick it or leave the default.` };
  }
  if (httpStatus === 400 && body?.what === "provider") {
    return { tone: "bad", text: "acan does not offer that provider, so the pick did not change." };
  }
  if (httpStatus === 404) {
    return { tone: "bad", text: `This acan has no role called ${role}, so nothing changed.` };
  }
  if (httpStatus === 409 && body?.what === "no_key") {
    const label = requested === null ? "That provider" : providerLabel(requested);
    return { tone: "bad", text: `${label} has no key yet. Save a key for it first.` };
  }
  if (httpStatus === 409 && body?.what === "routing_file") {
    return { tone: "bad", text: "This repository's .acan/llm.toml chooses the providers, so roles cannot be picked here." };
  }
  if (httpStatus === 409) {
    return { tone: "bad", text: "This acan cannot change a role from this page." };
  }
  if (httpStatus === 401) {
    return { tone: "bad", text: "This session is not signed in, so the pick cannot be changed." };
  }
  if (httpStatus === 403) {
    return { tone: "bad", text: "Only members can pick a provider, so this session cannot." };
  }
  return { tone: "bad", text: "Something went wrong. The pick did not change." };
}
