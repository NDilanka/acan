// The request the "+" sends to add a codebase to a running server, and the line the form shows for
// the answer. The server takes the add as POST /api/settings/projects with the owner's token, and
// answers 202 with the new codebase's id and name while its first scan runs.

export const ADD_PATH = "/api/settings/projects";

// The request that adds the folder at `path`, for the owner's `token`.
export function addRequest(token, path) {
  return {
    url: ADD_PATH,
    init: {
      method: "POST",
      headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
      body: JSON.stringify({ path: String(path ?? "").trim() }),
    },
  };
}

// The line the form shows for the server's status and body. Every answer but 202 changed nothing.
export function addNote(status, body) {
  if (status === 202) return "Added. Its first scan is running.";
  if (status === 0) return "Could not reach the server. Nothing changed.";
  if (status === 400) return "Type a folder path. Nothing changed.";
  if (status === 401 || status === 403) return "Only the owner can add a codebase. Nothing changed.";
  if (status === 409) {
    if (body?.what === "already_served") return "That folder is already served. Nothing changed.";
    return "Adding is not available on this server. Nothing changed.";
  }
  if (status === 422) {
    const why = Array.isArray(body?.fields) ? body.fields.find((field) => typeof field?.why === "string")?.why : undefined;
    return `Not added: the path ${why ?? "is not a folder this server can serve"}. Nothing changed.`;
  }
  return "Not added: the folder could not be opened. Nothing changed.";
}
