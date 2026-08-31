import { existsSync, mkdirSync, rmSync, statSync } from "node:fs";
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const stage = resolve(root, "dist/linux-x86_64");
const worker = process.argv[2] ? resolve(process.argv[2]) : resolve(stage, "bin/nian-media-worker");
const fixture = resolve(root, "crates/nian-media-ffmpeg/tests/fixtures/playback_h264.mkv");
const smokeRoot = process.argv[3] ? resolve(process.argv[3]) : stage;
const cache = resolve(smokeRoot, "smoke-cache");
const output = resolve(cache, "playback.mp4");
const version = JSON.parse(await import("node:fs").then(({ readFileSync }) => readFileSync(resolve(root, "package.json"), "utf8"))).version;

function timeout(promise, label, milliseconds = 15000) {
  return Promise.race([
    promise,
    new Promise((_, reject) => setTimeout(() => reject(new Error(`${label} timed out`)), milliseconds)),
  ]);
}

if (!existsSync(worker)) throw new Error(`staged worker missing: ${worker}`);
if (!existsSync(fixture)) throw new Error(`media fixture missing: ${fixture}`);
rmSync(cache, { recursive: true, force: true });
mkdirSync(cache, { recursive: true });

const cleanEnv = {
  PATH: "/usr/bin:/bin",
  HOME: process.env.HOME ?? "/tmp",
  LANG: process.env.LANG ?? "C.UTF-8",
};
for (const name of ["TMPDIR", "XDG_RUNTIME_DIR"]) {
  if (process.env[name]) cleanEnv[name] = process.env[name];
}

const child = spawn(worker, ["run"], {
  cwd: smokeRoot,
  env: cleanEnv,
  stdio: ["pipe", "pipe", "pipe"],
});
const lines = createInterface({ input: child.stdout });
const messages = [];
const waiters = [];
let stderr = "";
child.stderr.on("data", (chunk) => { stderr += chunk.toString(); });

function deliver(message) {
  const index = waiters.findIndex(({ predicate }) => predicate(message));
  if (index >= 0) {
    const [{ resolve: done }] = waiters.splice(index, 1);
    done(message);
  } else {
    messages.push(message);
  }
}

lines.on("line", (line) => {
  try {
    deliver(JSON.parse(line));
  } catch (error) {
    child.kill("SIGKILL");
    for (const waiter of waiters.splice(0)) waiter.reject(new Error(`invalid worker JSON: ${error.message}`));
  }
});

function next(predicate) {
  const index = messages.findIndex(predicate);
  if (index >= 0) return Promise.resolve(messages.splice(index, 1)[0]);
  return new Promise((resolvePromise, reject) => waiters.push({ predicate, resolve: resolvePromise, reject }));
}

function send(id, method, params = {}) {
  child.stdin.write(`${JSON.stringify({ type: "request", v: 1, id, method, params })}\n`);
  return timeout(next((message) => message.type === "response" && message.id === id), method);
}

try {
  const hello = await timeout(next((message) => message.type === "event" && message.name === "hello"), "worker HELLO");
  if (hello.v !== 1 || hello.data?.protocol !== 1) throw new Error("worker HELLO protocol mismatch");
  if (hello.data?.application_version !== version) {
    throw new Error(`worker version ${hello.data?.application_version ?? "<missing>"} does not match ${version}`);
  }
  const ffmpeg = hello.data?.ffmpeg;
  if (ffmpeg?.libavformat_major !== 62 || ffmpeg?.libavcodec_major !== 62 || ffmpeg?.libavutil_major !== 60) {
    throw new Error(`worker FFmpeg ABI mismatch: ${JSON.stringify(ffmpeg)}`);
  }

  const probe = await send(1, "camera.probe", {
    source: { kind: "file", path: fixture },
    timeout_ms: 5000,
  });
  if (!probe.ok || !probe.result?.reachable || !probe.result?.video_stream_found) {
    throw new Error(`fixture probe failed: ${JSON.stringify(probe)}`);
  }

  const playback = await send(2, "playback.prepare", {
    source_path: fixture,
    output_path: output,
    timeout_ms: 15000,
  });
  if (!playback.ok || !existsSync(output) || statSync(output).size === 0) {
    throw new Error(`fixture playback.prepare failed: ${JSON.stringify(playback)}`);
  }

  const shutdown = await send(3, "shutdown");
  if (!shutdown.ok) throw new Error(`worker shutdown failed: ${JSON.stringify(shutdown)}`);
  child.stdin.end();
  const exitCode = await timeout(new Promise((resolvePromise) => child.once("exit", resolvePromise)), "worker exit");
  if (exitCode !== 0) throw new Error(`worker exited ${exitCode}: ${stderr.trim()}`);
  process.stdout.write("clean staged runtime smoke passed\n");
} finally {
  if (child.exitCode === null) child.kill("SIGKILL");
  rmSync(cache, { recursive: true, force: true });
}
