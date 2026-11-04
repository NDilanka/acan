// The page's first run on an origin with no codebase, and the sign-in link that `acan open` prints.
// This module touches no page state and reads no DOM at import, so Node can test it. The DOM
// functions take the document they paint, so a test can pass a stand-in.

// The line the sign-in gate shows when the link's code is unknown, used, or expired.
export const SIGNIN_EXPIRED = "This sign-in link has expired or was already used. Run acan open again.";

// The sign-in link's fragment: `#signin=<code>`. Any other fragment, such as `#token=`, is not one.
const SIGNIN_FRAGMENT = /^#signin=([^&#\s]+)$/;

// The code of a sign-in fragment, or null for any other fragment.
export function signInCodeOf(hash) {
  const match = SIGNIN_FRAGMENT.exec(String(hash ?? ""));
  return match === null ? null : match[1];
}

// The address once the fragment is gone: the path and the query, as they were.
export function addressWithout(pathname, search) {
  return `${pathname}${search}`;
}

// The request that trades a sign-in code for the owner token. No auth header: the code is the
// credential, and it is single-use and lasts 60 seconds on the server.
export function exchangeRequest(code) {
  return {
    url: "/api/signin/exchange",
    init: {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ code }),
    },
  };
}

// Trades `code` for the owner token. A 200 hands the token to `store` and resolves ok; any other
// answer, or a failed request, resolves { ok: false, message } and stores nothing. The token never
// goes into a URL.
export async function exchangeSignIn(code, { fetch: send = fetch, store }) {
  const { url, init } = exchangeRequest(code);
  let res = null;
  try {
    res = await send(url, init);
  } catch {
    res = null;
  }
  if (res === null || res.status !== 200) return { ok: false, message: SIGNIN_EXPIRED };
  const body = await res.json().catch(() => null);
  if (typeof body?.token !== "string" || body.token === "") return { ok: false, message: SIGNIN_EXPIRED };
  store(body.token);
  return { ok: true };
}

// Whether GET /api/projects answered an empty world: 200 with no codebase. Any other answer is not.
export function isEmptyWorld(status, body) {
  return status === 200 && Array.isArray(body) && body.length === 0;
}

// The "Add a codebase with +" hint. It shows while the origin has no codebase, and once the first plant
// attaches it is hidden for good: it does not come back, even if the list is read empty again.
export function createEmptyWorld() {
  let attached = false;
  let empty = false;
  return {
    listed(list) {
      if (!attached) empty = Array.isArray(list) && list.length === 0;
      return this.showing;
    },
    attach() {
      attached = true;
      empty = false;
    },
    get showing() {
      return empty && !attached;
    },
  };
}

// The page's hint. Its state is kept here, so a list read again cannot bring it back.
const world = createEmptyWorld();

// Called with the project list when it reads empty: the hint shows.
export function noteEmptyList(list) {
  world.listed(list);
  paintEmptyHint(document, world.showing);
}

// Called once the first plant is on the canvas: the hint is hidden and does not come back.
export function noteAttached() {
  world.attach();
  paintEmptyHint(document, false);
}

// Paints the hint in `doc`: shown or hidden, with the "+" lit while it shows.
export function paintEmptyHint(doc, showing) {
  const hint = doc.getElementById("empty-hint");
  if (hint !== null) hint.hidden = !showing;
  doc.getElementById("add-plant")?.classList?.toggle("beckon", showing);
}

// Shows the sign-in gate with the expired-link line. The gate's token form is still there, so the
// owner can paste a token, but the page does not sign in by itself.
export function showSignInExpired(doc) {
  const note = doc.getElementById("gate-note");
  if (note !== null) {
    note.textContent = SIGNIN_EXPIRED;
    note.hidden = false;
  }
  const gate = doc.getElementById("gate");
  if (gate !== null) gate.hidden = false;
}
