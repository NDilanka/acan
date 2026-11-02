// The model key setup, as the app shows it. No DOM, so Node can test it. The server answers
// GET /api/setup and POST /api/setup/keys; this file says what each answer means. The key is never
// in this file: the page reads it from its field only to send it, and does not keep or show it.

// The roles a model call can do. Each one routes to a provider. Setup takes one provider today and
// routes every role to it. When the server takes a provider per role, the picker renders one group
// per role, and the save sends the routes role by role.
export const ROLES = ["foreman", "worker", "fast"];

const LABELS = { meta: "Meta Model API", baseten: "Baseten", fireworks: "Fireworks" };

export function providerLabel(name) {
  return LABELS[name] ?? String(name);
}

// The provider each role routes to, for a choice of one provider.
export function routesFor(provider) {
  return Object.fromEntries(ROLES.map((role) => [role, provider]));
}

// The one provider every role routes to, or null when the roles differ. The save sends one
// provider, so a split set of routes cannot be saved from here.
export function singleProvider(routes) {
  const providers = new Set(ROLES.map((role) => routes?.[role]));
  if (providers.size !== 1) return null;
  const [provider] = providers;
  return typeof provider === "string" && provider !== "" ? provider : null;
}

// The providers the form offers, from the server's list, in the server's order.
export function providerChoices(status) {
  const names = Array.isArray(status?.providers) ? status.providers : [];
  return names.map((value) => ({ value, label: providerLabel(value) }));
}

// The status line for GET /api/setup, and whether a key is set.
export function statusText(status) {
  if (status?.source === "routing_file") {
    return status.configured
      ? { text: "This repository's .acan/llm.toml chooses the providers, and their keys are set.", set: true }
      : { text: "This repository's .acan/llm.toml chooses the providers, but no key is set for them. Set their keys in its .env.", set: false };
  }
  if (status?.configured) {
    return { text: `Model calls use ${providerLabel(status.provider)}. Save a new key to replace it.`, set: true };
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
