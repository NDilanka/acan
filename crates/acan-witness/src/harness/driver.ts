// Transport for the witness runner. It runs as the container's only process, as the container
// user, and does only what the host cannot do from outside the app's network namespace: start
// the app with its port variable set, send one request from a chosen loopback source address,
// and return the raw response. The host decides templates, captures, observations and outcomes,
// so this file never reads a probe.
//
// Protocol: one JSON object per line on stdin; one JSON reply per line on stdout, in order.
// Nothing else is written to stdout. The app's output is read as it comes, and only the last lines
// of a start that fails are sent, in the reply's `app_output`.

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

interface RawResponse {
  status: number;
  headers: Record<string, string | string[]>;
  body: string;
  binary: boolean;
  truncated: boolean;
}

interface App {
  exited: Promise<number>;
  kill(): void;
}

interface StartOp {
  op: "start";
  argv: string[];
  cwd: string;
  environment: Record<string, string>;
  port_env: string;
  port: number;
  max_output_bytes: number;
  ready: { path: string; status: number; timeout_ms: number };
}

interface RequestOp {
  op: "request";
  client: string;
  method: string;
  path: string;
  headers: Record<string, string>;
  body: string | null;
}

interface ClockOp {
  op: "clock";
  path: string;
  by_ms: number;
}

interface GetOp {
  op: "get";
  path: string;
}

type Op = StartOp | RequestOp | ClockOp | GetOp;

let app: App | undefined;
let appExitCode: number | undefined;
let port = 0;
let maxOutputBytes = 0;

function fail(detail: string, crashed = false) {
  return { ok: false, detail, crashed };
}

function message(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T> {
  return Promise.race([
    promise,
    Bun.sleep(ms).then(() => {
      throw new Error(`no answer within ${ms} ms`);
    }),
  ]);
}

function exchange(
  client: string,
  method: string,
  path: string,
  headers: Record<string, string>,
  body: string | null,
): Promise<RawResponse> {
  return new Promise((resolve, reject) => {
    const payload = body === null ? null : Buffer.from(body, "utf8");
    const requestHeaders: Record<string, string> = { ...headers };
    if (payload !== null) requestHeaders["content-length"] = String(payload.length);
    const req = http.request(
      // No agent: a pooled socket could carry one client address's request as another's.
      { host: LOOPBACK, port, method, path, headers: requestHeaders, localAddress: client, agent: false },
      (res) => {
        const chunks: Buffer[] = [];
        let kept = 0;
        let truncated = false;
        res.on("data", (chunk: Buffer) => {
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
            headers: res.headers as Record<string, string | string[]>,
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

async function appHasExited(): Promise<boolean> {
  if (app === undefined) return false;
  if (appExitCode !== undefined) return true;
  return Promise.race([
    app.exited.then(() => true),
    Bun.sleep(EXIT_GRACE_MS).then(() => false),
  ]);
}

// The last lines the app wrote, for a start that fails. Each line keeps its first TAIL_LINE_BYTES
// bytes, and the kept lines with their newlines stay within TAIL_BYTES.
class OutputTail {
  private lines: Buffer[] = [];
  private bytes = 0;

  add(line: Buffer): void {
    const kept = line.subarray(0, TAIL_LINE_BYTES);
    this.lines.push(kept);
    this.bytes += kept.length + 1;
    while (this.lines.length > TAIL_LINES || this.bytes > TAIL_BYTES) {
      const dropped = this.lines.shift();
      this.bytes -= (dropped?.length ?? 0) + 1;
    }
  }

  text(): string {
    return this.lines.map((line) => line.toString("utf8")).join("\n");
  }
}

// Reads one of the app's output streams to its end. Whole lines go to the tail, and so does the
// last line when the stream ends without a newline.
async function drain(
  stream: ReadableStream<Uint8Array> | undefined,
  tail: OutputTail,
): Promise<void> {
  if (stream === undefined) return;
  const reader = stream.getReader();
  let partial = Buffer.alloc(0);
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      const data = Buffer.concat([partial, Buffer.from(value)]);
      let start = 0;
      for (let end = data.indexOf(NEWLINE); end !== -1; end = data.indexOf(NEWLINE, start)) {
        tail.add(data.subarray(start, end));
        start = end + 1;
      }
      partial = data.subarray(start, start + TAIL_LINE_BYTES);
    }
  } catch {
    return;
  }
  if (partial.length > 0) tail.add(partial);
}

function startFailed(detail: string, crashed: boolean, tail: OutputTail) {
  return { ...fail(detail, crashed), app_output: tail.text() };
}

async function start(op: StartOp) {
  port = op.port;
  maxOutputBytes = op.max_output_bytes;
  const argv = op.argv.map((arg, index) =>
    index === 0 && arg === "bun" ? process.execPath : arg,
  );
  const child = Bun.spawn({
    cmd: argv,
    cwd: op.cwd,
    env: { ...op.environment, [op.port_env]: String(op.port) },
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });
  app = child;
  child.exited.then((code) => {
    appExitCode = code;
  });
  // The output is read as it comes, so the app never blocks on a full pipe.
  const tail = new OutputTail();
  const drained = Promise.all([drain(child.stdout, tail), drain(child.stderr, tail)]);
  const until = Date.now() + op.ready.timeout_ms;
  while (Date.now() < until) {
    if (appExitCode !== undefined) {
      await Promise.race([drained, Bun.sleep(EXIT_GRACE_MS)]);
      return startFailed(`app exited with code ${appExitCode} before it was ready`, true, tail);
    }
    const probe = await withTimeout(
      exchange(LOOPBACK, "GET", op.ready.path, {}, null),
      READY_ATTEMPT_MS,
    ).catch(() => undefined);
    if (probe !== undefined && probe.status === op.ready.status) {
      return { ok: true, crashed: false, detail: "" };
    }
    await Bun.sleep(POLL_MS);
  }
  return startFailed(`app not ready after ${op.ready.timeout_ms} ms`, false, tail);
}

async function request(op: RequestOp) {
  try {
    const raw = await exchange(op.client, op.method, op.path, op.headers, op.body);
    return { ok: true, crashed: false, detail: "", ...raw };
  } catch (err) {
    return fail(message(err), await appHasExited());
  }
}

async function clock(op: ClockOp) {
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

async function get(op: GetOp) {
  try {
    const raw = await exchange(LOOPBACK, "GET", op.path, {}, null);
    return { ok: true, crashed: false, detail: "", ...raw };
  } catch (err) {
    return fail(message(err), await appHasExited());
  }
}

async function dispatch(op: Op) {
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

async function handle(line: string): Promise<void> {
  let reply: unknown;
  try {
    reply = await dispatch(JSON.parse(line) as Op);
  } catch (err) {
    reply = fail(message(err));
  }
  process.stdout.write(`${JSON.stringify(reply)}\n`);
}

const lines = readline.createInterface({ input: process.stdin });
let queue: Promise<void> = Promise.resolve();
lines.on("line", (line) => {
  queue = queue.then(() => handle(line));
});
lines.on("close", () => {
  app?.kill();
  process.exit(0);
});
