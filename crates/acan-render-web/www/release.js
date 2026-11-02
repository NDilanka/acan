// The alpha's shell facts: the build's version, the update notice and the bug report. No DOM, so
// Node can test it. The server answers each route; this file only chooses what the page shows from
// the answer. A check that could not run shows nothing, never an error.

// Only an https link is ever shown or opened. Anything else is not a link.
export function safeLink(url) {
  if (typeof url !== "string") return null;
  try {
    const parsed = new URL(url);
    return parsed.protocol === "https:" ? parsed.href : null;
  } catch {
    return null;
  }
}

// The notice shows only when GET /api/update says a newer release exists. Every other state, and
// a release without a usable link, shows nothing.
export function updateNotice(update) {
  if (update?.state !== "available") return null;
  const href = safeLink(update.url);
  if (typeof update.latest !== "string" || update.latest === "" || href === null) return null;
  return { latest: update.latest, href };
}

// The update check is still running, so the page asks again shortly.
export function stillChecking(update) {
  return update?.state === "checking";
}

// The version shown in the shell bar, short enough to leave room for the hint bar beside it.
export function versionLabel(info) {
  return typeof info?.version === "string" ? `v${info.version}` : "";
}

// The full version line, shown as the version's tooltip: the build's version and platform, with its
// commit when it has one.
export function versionLine(info) {
  if (typeof info?.version !== "string") return "";
  const commit =
    typeof info.commit === "string" && info.commit !== "unknown" && info.commit !== ""
      ? ` · ${info.commit.slice(0, 7)}`
      : "";
  return `acan ${info.version} · ${info.os} ${info.arch}${commit}`;
}

// What the bug button does with the answer to GET /api/bug-report. The report is shown in full
// first, and its link opens only when the tester chooses it. `truncated` says the link left log
// lines out, so the page offers the full body to copy. Any other answer is one plain sentence.
export function bugReportOutcome(status, payload) {
  const url = status === 200 ? safeLink(payload?.url) : null;
  if (url !== null) {
    return {
      ok: true,
      url,
      title: typeof payload.title === "string" ? payload.title : "",
      body: typeof payload.body === "string" ? payload.body : "",
      truncated: payload.truncated === true,
    };
  }
  if (status === 403) {
    return {
      ok: false,
      message: "Only members can send a bug report, so this session cannot send one.",
    };
  }
  if (status === 401) {
    return {
      ok: false,
      message: "This session is not signed in, so the bug report cannot be prepared.",
    };
  }
  return {
    ok: false,
    message: "The bug report could not be prepared. Try again in a moment.",
  };
}
