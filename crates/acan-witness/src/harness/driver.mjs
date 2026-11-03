// Transport for the witness runner, for a node app. It is driver.ts with the same protocol and the
// same duties: it runs as the container's only process, as the container user, and does only what
// the host cannot do from outside the app's network namespace: start the app with its port
// variable set, send one request from a chosen loopback source address, and return the raw
// response. The host decides templates, captures, observations and outcomes, so this file never
// reads a probe.
//
// Protocol: one JSON object per line on stdin; one JSON reply per line on stdout, in order.
// Nothing else is written to stdout. The app's output is read as it comes, and only the last lines
// of a start that fails are sent, in the reply's `app_output`.

import { spawn } from "node:child_process";
import * as http from "node:http";
import * as readline from "node:readline";

const LOOPBACK = "127.0.0.1";
const READY_ATTEMPT_MS = 1000;
const POLL_MS = 5;
const EXIT_GRACE_MS = 200;
const TAIL_LINES = 200;
const TAIL_LINE_BYTES = 512;
const TAIL_BYTES = 64 * 1024;
const NEWLINE = 10;

let app;
let appExitCode;
let spawnError;
let port = 0;
let maxOutputBytes = 0;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function fail(detail, crashed = false) {
  return { ok: false, detail, crashed };
}

function message(err) {
  return err instanceof Error ? err.message : String(err);
}

function withTimeout(promise, ms) {
  return Promise.race([
    promise,
    sleep(ms).then(() => {
      throw new Error(`no answer within ${ms} ms`);
    }),
  ]);
}

function exchange(client, method, path, headers, body) {
  return new Promise((resolve, reject) => {
    const payload = body === null ? null : Buffer.from(body, "utf8");
    const requestHeaders = { ...headers };
    if (payload !== null) requestHeaders["content-length"] = String(payload.length);
    const req = http.request(
      // No agent: a pooled socket could carry one client address's request as another's.
      { host: LOOPBACK, port, method, path, headers: requestHeaders, localAddress: client, agent: false },
      (res) => {
        const chunks = [];
        let kept = 0;
        let truncated = false;
        res.on("data", (chunk) => {
          const room = maxOutputBytes - kept;
          if (chunk.length <= room) {
            chunks.push(chunk);
            kept += chunk.length;
          } else {
            if (room > 0) {
              chunks.push(chunk.subarray(0, room));
              kept += room;
            }
            truncated = true;
          }
        });
        res.on("error", reject);
        res.on("end", () => {
          const bytes = Buffer.concat(chunks);
          let text = "";
          let binary = false;
          try {
            text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
          } catch {
            binary = true;
          }
          resolve({
            status: res.statusCode ?? 0,
            headers: res.headers,
            body: text,
            binary,
            truncated,
          });
        });
      },
    );
    req.on("error", reject);
    if (payload !== null) req.write(payload);
    req.end();
  });
}

async function appHasExited() {
  if (app === undefined) return false;
  if (appExitCode !== undefined) return true;
  return Promise.race([
    app.exited.then(() => true),
    sleep(EXIT_GRACE_MS).then(() => false),
  ]);
}

// The last lines the app wrote, for a start that fails. Each line keeps its first TAIL_LINE_BYTES
// bytes, and the kept lines with their newlines stay within TAIL_BYTES.
class OutputTail {
  lines = [];
  bytes = 0;

  add(line) {
    const kept = line.subarray(0, TAIL_LINE_BYTES);
    this.lines.push(kept);
    this.bytes += kept.length + 1;
    while (this.lines.length > TAIL_LINES || this.bytes > TAIL_BYTES) {
      this.bytes -= this.lines.shift().length + 1;
    }
  }

  text() {
    return this.lines.map((line) => line.toString("utf8")).join("\n");
  }
}

// Reads one of the app's output streams to its end. Whole lines go to the tail, and so does the
// last line when the stream ends without a newline.
function drain(stream, tail) {
  if (!stream) return Promise.resolve();
  return new Promise((resolve) => {
    let partial = Buffer.alloc(0);
    stream.on("data", (chunk) => {
      const data = Buffer.concat([partial, chunk]);
      let start = 0;
      for (let end = data.indexOf(NEWLINE); end !== -1; end = data.indexOf(NEWLINE, start)) {
        tail.add(data.subarray(start, end));
        start = end + 1;
      }
      partial = data.subarray(start, start + TAIL_LINE_BYTES);
    });
    const finish = () => {
      if (partial.length > 0) tail.add(partial);
      partial = Buffer.alloc(0);
      resolve();
    };
    stream.on("end", finish);
    stream.on("error", finish);
  });
}

function startFailed(detail, crashed, tail) {
  return { ...fail(detail, crashed), app_output: tail.text() };
}

async function start(op) {
  port = op.port;
  maxOutputBytes = op.max_output_bytes;
  const argv = op.argv.map((arg, index) =>
    index === 0 && arg === "node" ? process.execPath : arg,
  );
  const child = spawn(argv[0], argv.slice(1), {
    cwd: op.cwd,
    env: { ...op.environment, [op.port_env]: String(op.port) },
    stdio: ["ignore", "pipe", "pipe"],
  });
  // The output is read as it comes, so the app never blocks on a full pipe.
  const tail = new OutputTail();
  const drained = Promise.all([drain(child.stdout, tail), drain(child.stderr, tail)]);
  const finish = (code) => {
    if (appExitCode === undefined) appExitCode = code;
    return appExitCode;
  };
  app = {
    exited: new Promise((resolve) => {
      child.on("error", (err) => {
        spawnError = message(err);
        resolve(finish(-1));
      });
      child.on("exit", (code) => resolve(finish(code ?? -1)));
    }),
    kill: () => child.kill(),
  };
  const until = Date.now() + op.ready.timeout_ms;
  while (Date.now() < until) {
    if (appExitCode !== undefined) {
      const detail =
        spawnError !== undefined
          ? `app did not start: ${spawnError}`
          : `app exited with code ${appExitCode} before it was ready`;
      await Promise.race([drained, sleep(EXIT_GRACE_MS)]);
      return startFailed(detail, true, tail);
    }
    const probe = await withTimeout(
      exchange(LOOPBACK, "GET", op.ready.path, {}, null),
      READY_ATTEMPT_MS,
    ).catch(() => undefined);
    if (probe !== undefined && probe.status === op.ready.status) {
      return { ok: true, crashed: false, detail: "" };
    }
    await sleep(POLL_MS);
  }
  return startFailed(`app not ready after ${op.ready.timeout_ms} ms`, false, tail);
}

async function request(op) {
  try {
    const raw = await exchange(op.client, op.method, op.path, op.headers, op.body);
    return { ok: true, crashed: false, detail: "", ...raw };
  } catch (err) {
    return fail(message(err), await appHasExited());
  }
}

async function clock(op) {
  const body = JSON.stringify({ by_ms: op.by_ms });
  try {
    const raw = await exchange(
      LOOPBACK,
      "POST",
      op.path,
      { "content-type": "application/json" },
      body,
    );
    return { ok: true, crashed: false, detail: "", ...raw };
  } catch (err) {
    return fail(message(err), await appHasExited());
  }
}

async function get(op) {
  try {
    const raw = await exchange(LOOPBACK, "GET", op.path, {}, null);
    return { ok: true, crashed: false, detail: "", ...raw };
  } catch (err) {
    return fail(message(err), await appHasExited());
  }
}

async function dispatch(op) {
  switch (op.op) {
    case "start":
      return start(op);
    case "request":
      return request(op);
    case "clock":
      return clock(op);
    case "get":
      return get(op);
    default:
      return fail("unknown operation");
  }
}

async function handle(line) {
  let reply;
  try {
    reply = await dispatch(JSON.parse(line));
  } catch (err) {
    reply = fail(message(err));
  }
  process.stdout.write(`${JSON.stringify(reply)}\n`);
}

const lines = readline.createInterface({ input: process.stdin });
let queue = Promise.resolve();
lines.on("line", (line) => {
  queue = queue.then(() => handle(line));
});
lines.on("close", () => {
  app?.kill();
  process.exit(0);
});
