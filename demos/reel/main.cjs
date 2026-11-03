// Glass — floating review card for unstaged changes.
// Main process owns all state: git parsing, clustering, decisions, undo, apply.
const { app, BrowserWindow, ipcMain, Menu, nativeTheme, screen } = require("electron");
const { execFileSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");
const crypto = require("node:crypto");
const { analyzeFiles, summarizeAnalyses } = require("./analysis.cjs");

/** Repo under review: positional arg > REVIEW_REPO > cwd, widened to the git toplevel. */
function resolveRepo() {
  const arg = process.argv.slice(2).find((a) => !a.startsWith("-"));
  const want = path.resolve(arg || process.env.REVIEW_REPO || process.cwd());
  try {
    return execFileSync("git", ["-C", want, "rev-parse", "--show-toplevel"], { encoding: "utf8" }).trim();
  } catch {
    return want;
  }
}
const REPO = resolveRepo();
const SHOT_DIR = process.env.SHOT_DIR || null;
const AI_ENABLED = process.env.REEL_AI !== "0";
const AI_URL = process.env.REEL_AI_URL || "http://localhost:4000/v1/chat/completions";
const AI_MODEL = process.env.REEL_AI_MODEL || "anthropic/claude-haiku-4-5";
const CARD = { width: 340, height: 640 };
const POPUP = { width: 680, height: 700 };
const GAP = 10;

// ---------- git ----------

function git(args, input) {
  return execFileSync("git", ["-C", REPO, ...args], {
    encoding: "utf8",
    input,
    maxBuffer: 64 * 1024 * 1024,
  });
}

/** Parse a unified diff into per-file records with raw hunk text preserved for apply. */
function parseDiff(text) {
  const files = [];
  for (const section of text.split(/^(?=diff --git )/m)) {
    if (!section.startsWith("diff --git ")) continue;
    const lines = section.split("\n");
    const head = lines[0].match(/^diff --git a\/(.+?) b\/(.+)$/);
    const filePath = head ? head[2] : "unknown";
    let status = "modified";
    if (section.includes("\n+++ /dev/null")) status = "deleted";
    else if (section.includes("\n--- /dev/null") || /\nnew file mode /.test(section)) status = "added";
    else if (/\nrename from /.test(section)) status = "renamed";
    const firstHunk = lines.findIndex((l) => l.startsWith("@@"));
    const headerText = firstHunk === -1 ? section : lines.slice(0, firstHunk).join("\n") + "\n";
    const hunks = [];
    if (firstHunk !== -1) {
      let current = null;
      for (const line of lines.slice(firstHunk)) {
        if (line.startsWith("@@")) {
          if (current) hunks.push(current);
          current = { header: line, lines: [] };
        } else if (current && line !== "") {
          current.lines.push(line);
        } else if (current) {
          current.lines.push(line);
        }
      }
      if (current) hunks.push(current);
      // drop trailing empty line artifacts
      for (const h of hunks) while (h.lines.length && h.lines[h.lines.length - 1] === "") h.lines.pop();
    }
    let adds = 0;
    let dels = 0;
    for (const h of hunks)
      for (const l of h.lines) {
        if (l.startsWith("+")) adds++;
        else if (l.startsWith("-")) dels++;
      }
    files.push({ path: filePath, status, headerText, hunks, adds, dels, untracked: false, binary: hunks.length === 0 });
  }
  return files;
}

/** Untracked files rendered as new-file pseudo-diffs (staged via `git add`, never `git apply`). */
function untrackedFiles() {
  const out = git(["ls-files", "--others", "--exclude-standard"]).trim();
  if (!out) return [];
  const files = [];
  for (const rel of out.split("\n")) {
    const abs = path.join(REPO, rel);
    let buf;
    try {
      buf = fs.readFileSync(abs);
    } catch {
      continue;
    }
    const binary = buf.subarray(0, 8000).includes(0);
    if (binary) {
      files.push({ path: rel, status: "added", headerText: "", hunks: [], adds: 0, dels: 0, untracked: true, binary: true });
      continue;
    }
    const body = buf.toString("utf8").split("\n").slice(0, 200);
    files.push({
      path: rel,
      status: "added",
      headerText: "",
      hunks: [{ header: `@@ -0,0 +1,${body.length} @@`, lines: body.map((l) => "+" + l) }],
      adds: body.length,
      dels: 0,
      untracked: true,
      binary: false,
    });
  }
  return files;
}

// ---------- clustering ----------

const DOC_EXT = /\.(md|txt|rst|adoc)$/i;

function makeClusters(files) {
  const buckets = { core: [], ripples: [], tests: [], chores: [] };
  for (const file of files) {
    if (/(^|\/)(test|tests|spec|__tests__)(\/|$)|\.(test|spec)\./i.test(file.path)) buckets.tests.push(file);
    else if (DOC_EXT.test(file.path) || /(^|\/)(config|docs)(\/|$)|(^|\/)\.env|\.json$/i.test(file.path)) buckets.chores.push(file);
    else if (/auth|session|service|model|domain|store|engine|core/i.test(file.path)) buckets.core.push(file);
    else buckets.ripples.push(file);
  }
  const definitions = [
    ["core", "The behavior changes at its source", "Start with the code that defines what the system does."],
    ["ripples", "The rest of the app catches up", "These call sites depend on the API change above."],
    ["tests", "The promise gets pinned down", "Tests follow behavior so the contract is easy to challenge."],
  ];
  const createCluster = (key, title, reason, members) => {
    const adds = members.reduce((sum, file) => sum + file.adds, 0);
    const dels = members.reduce((sum, file) => sum + file.dels, 0);
    const summary = summarizeAnalyses(members.flatMap((file) => file.hunks.map((hunk) => hunk.analysis)));
    return {
      id: key,
      key,
      title,
      reason,
      gist: `${members.length} file${members.length === 1 ? "" : "s"} · +${adds} −${dels}`,
      files: members,
      adds,
      dels,
      attention: summary.attention,
      counts: summary.counts,
      topSignal: summary.topSignal,
    };
  };
  const narrative = definitions.flatMap(([key, title, reason]) => buckets[key].length ? [createCluster(key, title, reason, buckets[key])] : []);
  const kept = [];
  for (const cluster of narrative) {
    if (cluster.counts.ezpz > 0 && cluster.counts.focus === 0 && cluster.counts.review === 0) buckets.chores.push(...cluster.files);
    else kept.push(cluster);
  }
  const implementation = kept.filter((cluster) => cluster.key !== "tests").sort((left, right) => right.attention - left.attention);
  const tests = kept.filter((cluster) => cluster.key === "tests");
  const ordered = [...implementation, ...tests];
  if (buckets.chores.length) ordered.push(createCluster("chores", "The easy stuff, gathered", "Low-risk changes are bundled so attention stays on behavior.", buckets.chores));
  return ordered;
}

// ---------- sample fallback ----------

const SAMPLE_DIFF = `diff --git a/src/auth/session.ts b/src/auth/session.ts
index 3f1a2b4..9c8d7e6 100644
--- a/src/auth/session.ts
+++ b/src/auth/session.ts
@@ -8,6 +8,12 @@ export async function restoreSession(token: string) {
-  return decode(token)
+  const session = await decode(token)
+  if (session.expiresAt < Date.now()) return null
+  return session
 }
+
+export const refreshWindow = 60_000
@@ -30,3 +36,3 @@ export function sessionLabel(session: Session) {
-  const label = session.user.name
+    const label = session.user.name
diff --git a/src/auth/refresh.ts b/src/auth/refresh.ts
new file mode 100644
--- /dev/null
+++ b/src/auth/refresh.ts
@@ -0,0 +1,7 @@
+import { refreshWindow } from './session'
+export async function refreshSession(session: Session) {
+  if (session.expiresAt - Date.now() > refreshWindow) return session
+  return api.post('/session/refresh', { id: session.id })
+}
diff --git a/src/components/Login.tsx b/src/components/Login.tsx
index 11aa22b..33cc44d 100644
--- a/src/components/Login.tsx
+++ b/src/components/Login.tsx
@@ -2,5 +2,8 @@ export function Login() {
-  return <SignInForm />
+  const session = useSession()
+  if (session.recovering) return <SessionRecovery />
+  return <SignInForm />
 }
diff --git a/tests/auth/session.test.ts b/tests/auth/session.test.ts
index 55ee66f..77aa88b 100644
--- a/tests/auth/session.test.ts
+++ b/tests/auth/session.test.ts
@@ -12,4 +12,9 @@ describe('restoreSession', () => {
+  it('drops expired sessions', async () => {
+    const stale = await restoreSession(expiredToken)
+    expect(stale).toBeNull()
+  })
 })
diff --git a/README.md b/README.md
index abc1234..def5678 100644
--- a/README.md
+++ b/README.md
@@ -20,3 +20,5 @@ ## Sessions
+Sessions now self-heal: expired tokens are dropped and refreshed
+within a 60s window before requests fail.
`;

// ---------- state ----------

let sample = false;
let clusters = [];
let currentIndex = 0;
const decisions = new Map(); // clusterId -> approved | rejected
const vetoes = new Set(); // hunk ids excluded from an approval
const approvedHunks = new Set(); // individually auto-skimmed ezpz hunks
const history = []; // { clusterId, previous }
let popupOpen = true;
let applyResult = null;
let aiState = { status: AI_ENABLED ? "loading" : "offline", narrative: null };
let enrichmentPromise = Promise.resolve();
let aiProgressCapture = null;

function load() {
  let diffText = "";
  let extra = [];
  try {
    diffText = git(["diff", "--no-color", "--no-ext-diff"]);
    extra = untrackedFiles();
  } catch {
    diffText = "";
  }
  const files = [...parseDiff(diffText), ...extra].filter((file) => !/(^|\/)(bun\.lock|package-lock\.json|yarn\.lock|pnpm-lock\.yaml|uv\.lock|Cargo\.lock|[^/]+\.lockb)$|(^|\/)(node_modules|dist)\//.test(file.path));
  sample = files.length === 0;
  const reviewFiles = sample ? parseDiff(SAMPLE_DIFF) : files;
  reviewFiles.filter((file) => file.status === "renamed" && file.hunks.length === 0).forEach((file) => file.hunks.push({ header: "@@ rename metadata @@", lines: [] }));
  reviewFiles.forEach((file, fileIndex) => file.hunks.forEach((hunk, hunkIndex) => {
    hunk.id = `${fileIndex}:${hunkIndex}`;
  }));
  const analyses = analyzeFiles(reviewFiles);
  reviewFiles.forEach((file) => file.hunks.forEach((hunk) => {
    hunk.analysis = analyses.get(hunk.id) || { tier: "review", score: 0.42, signals: [] };
  }));
  clusters = makeClusters(reviewFiles);
  currentIndex = 0;
  decisions.clear();
  vetoes.clear();
  approvedHunks.clear();
  history.length = 0;
  applyResult = null;
  aiState = { status: AI_ENABLED ? "loading" : "offline", narrative: null };
}

/** Build a bounded, symbol-rich digest for one batched enrichment request. */
function buildAIDigest() {
  let remaining = 36 * 1024;
  const chaptersDigest = [];
  for (const cluster of clusters) {
    const chapter = { id: cluster.id, heuristicTitle: cluster.title, files: [] };
    for (const file of cluster.files) {
      if (file.binary || remaining <= 0) continue;
      const lines = [];
      for (const hunk of file.hunks) {
        if (lines.length >= 150 || remaining <= 0) break;
        lines.push(hunk.header);
        for (const rawLine of hunk.lines) {
          if (lines.length >= 150 || remaining <= 0) break;
          const line = rawLine.length > 512 ? `${rawLine.slice(0, 246)}[… ]${rawLine.slice(-246)}` : rawLine;
          lines.push(line);
          remaining -= Buffer.byteLength(line, "utf8") + 4;
        }
      }
      chapter.files.push({
        path: file.path,
        hunks: file.hunks.map((hunk) => ({ tier: hunk.analysis.tier, score: hunk.analysis.score, signals: hunk.analysis.signals.slice(0, 4).map((signal) => ({ tag: signal.tag, detail: signal.detail.slice(0, 240) })) })),
        diff: lines,
      });
    }
    chaptersDigest.push(chapter);
  }
  return { chapters: chaptersDigest };
}

function applyAIEnrichment(result) {
  if (!result || !Array.isArray(result.chapters)) return false;
  const byId = new Map(clusters.map((cluster) => [cluster.id, cluster]));
  const suppliedIds = result.chapters.map((item) => item?.id);
  if (suppliedIds.length !== clusters.length || new Set(suppliedIds).size !== clusters.length || suppliedIds.some((id) => !byId.has(id))) return false;
  let applied = 0;
  for (const item of result.chapters) {
    const cluster = byId.get(item?.id);
    if (!cluster || typeof item.headline !== "string" || typeof item.gist !== "string") continue;
    cluster.aiHeadline = item.headline.trim().slice(0, 50);
    cluster.aiGist = item.gist.trim().slice(0, 280);
    cluster.aiBriefing = Array.isArray(item.briefing) ? item.briefing.filter((note) => typeof note === "string").slice(0, 3).map((note) => note.slice(0, 280)) : [];
    cluster.aiOrder = Number.isFinite(item.order) ? item.order : null;
    const rawAttention = Number(item.attention);
    cluster.aiAttention = Number.isFinite(rawAttention) ? Math.max(0, Math.min(100, rawAttention <= 1 ? rawAttention * 100 : rawAttention)) : null;
    cluster.heuristicAttention ??= cluster.attention;
    if (cluster.aiAttention !== null) cluster.attention = cluster.heuristicAttention * 0.5 + (cluster.aiAttention / 100) * 0.5;
    applied++;
  }
  if (!applied) return false;
  const selectedId = clusters[currentIndex]?.id;
  if (decisions.size === 0) {
    clusters.sort((left, right) => (left.aiOrder ?? Number.MAX_SAFE_INTEGER) - (right.aiOrder ?? Number.MAX_SAFE_INTEGER));
    currentIndex = Math.max(0, clusters.findIndex((cluster) => cluster.id === selectedId));
  }
  aiState = { status: "ready", narrative: typeof result.narrative === "string" ? result.narrative.slice(0, 360) : null };
  broadcast();
  return true;
}

const AI_CACHE_PATH = path.join(os.tmpdir(), "reel-ai-cache.json");

function readAICache() {
  try {
    const cached = JSON.parse(fs.readFileSync(AI_CACHE_PATH, "utf8"));
    if (cached.version === 2) return cached;
  } catch {}
  return { version: 2, fast: {}, observations: {}, reductions: {} };
}

function writeAICache(cache) {
  try {
    fs.writeFileSync(AI_CACHE_PATH, JSON.stringify(cache));
  } catch {}
}

async function callAIJSON(prompt, model, timeoutMs, maxTokens) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(AI_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ model, temperature: 0.2, max_tokens: maxTokens, messages: [{ role: "user", content: prompt }] }),
      signal: controller.signal,
    });
    if (!response.ok) throw new Error(`AI HTTP ${response.status}`);
    const envelope = await response.json();
    let content = envelope?.choices?.[0]?.message?.content;
    if (Array.isArray(content)) content = content.map((part) => part?.text || "").join("");
    if (typeof content !== "string") throw new Error("AI response has no text");
    return JSON.parse(content.trim().replace(/^```(?:json)?\s*/i, "").replace(/\s*```$/, ""));
  } finally {
    clearTimeout(timeout);
  }
}

function scrubLine(rawLine) {
  return rawLine.length > 512 ? `${rawLine.slice(0, 246)}[...omitted...]${rawLine.slice(-246)}` : rawLine;
}

function fileMapInput(file) {
  const lines = [];
  let bytes = 0;
  for (const hunk of file.hunks) {
    if (bytes >= 7600) break;
    lines.push(hunk.header);
    for (const rawLine of hunk.lines) {
      const line = scrubLine(rawLine);
      const cost = Buffer.byteLength(line, "utf8") + 1;
      if (bytes + cost > 8000) break;
      lines.push(line);
      bytes += cost;
    }
  }
  return {
    path: file.path,
    status: file.status,
    signals: file.hunks.map((hunk) => ({ tier: hunk.analysis.tier, signals: hunk.analysis.signals.slice(0, 5) })),
    diff: lines.join("\n"),
  };
}

function fallbackObservation(file) {
  const signals = file.hunks.flatMap((hunk) => hunk.analysis.signals);
  const topRisk = signals.find((signal) => file.hunks.some((hunk) => hunk.analysis.tier === "focus" && hunk.analysis.signals.includes(signal)));
  const symbols = signals.filter((signal) => signal.tag === "api-signature").map((signal) => signal.detail.split(/\s+/).pop()).filter(Boolean).slice(0, 8);
  const role = /(^|\/)(test|tests|spec|__tests__)(\/|$)|\.(test|spec)\./i.test(file.path) ? "test"
    : DOC_EXT.test(file.path) || /(^|\/)(config|docs)(\/|$)|\.json$/i.test(file.path) ? "chore"
      : /auth|session|service|model|domain|store|engine|core/i.test(file.path) ? "core" : "ripple";
  return {
    path: file.path,
    status: file.status,
    role,
    summary: `${file.status} ${file.path} (+${file.adds} −${file.dels})`,
    key_symbols: [...new Set(symbols)],
    risk: topRisk?.detail || null,
    fallback: true,
  };
}

function validateObservation(value, file) {
  if (!value || value.path !== file.path || typeof value.summary !== "string" || !["core", "ripple", "test", "chore"].includes(value.role)) return null;
  return {
    path: file.path,
    status: file.status,
    role: value.role,
    summary: value.summary.slice(0, 280),
    key_symbols: Array.isArray(value.key_symbols) ? value.key_symbols.filter((symbol) => typeof symbol === "string").slice(0, 12).map((symbol) => symbol.slice(0, 100)) : [],
    risk: typeof value.risk === "string" ? value.risk.slice(0, 360) : null,
  };
}

async function mapFileObservations(files, cache) {
  const model = process.env.REEL_AI_MAP_MODEL || AI_MODEL;
  const observations = new Array(files.length);
  let cursor = 0;
  let mapped = 0;
  aiState = { status: "mapping", mapped: 0, total: files.length, narrative: null };
  broadcast();
  const started = Date.now();

  const worker = async () => {
    while (true) {
      const index = cursor++;
      if (index >= files.length) return;
      const file = files[index];
      const input = fileMapInput(file);
      const observationKey = crypto.createHash("sha256").update(`${file.path}\n${file.hunks.map((hunk) => `${hunk.header}\n${hunk.lines.join("\n")}`).join("\n")}`).digest("hex");
      let observation = cache.observations[observationKey] || null;
      if (observation?.fallback) observation = null;
      if (!observation && !file.binary) {
        const prompt = `Analyze exactly one changed file for a code reviewer. Heuristic signals are ground truth. Return STRICT JSON only: {"path":"exact provided path","status":"added|deleted|renamed|modified","role":"core|ripple|test|chore","summary":"one factual line","key_symbols":["real symbols only"],"risk":"specific reviewer risk or null"}. Never invent symbols or paths.\n${JSON.stringify(input)}`;
        try {
          observation = validateObservation(await callAIJSON(prompt, model, 20_000, 700), file);
        } catch {}
      }
      if (!observation) observation = fallbackObservation(file);
      observations[index] = observation;
      if (!observation.fallback) cache.observations[observationKey] = observation;
      mapped++;
      aiState = { status: "mapping", mapped, total: files.length, narrative: null };
      broadcast();
      if (typeof aiProgressCapture === "function") aiProgressCapture();
    }
  };
  await Promise.all(Array.from({ length: Math.min(8, files.length) }, () => worker()));
  writeAICache(cache);
  console.log(`Reel AI map: ${files.length} observations in ${Date.now() - started}ms`);
  return observations;
}

function applyRegroupedAI(result, observations) {
  if (!result || !Array.isArray(result.chapters) || result.chapters.length === 0 || result.chapters.length > 12 || decisions.size > 0) return false;
  const allFiles = clusters.flatMap((cluster) => cluster.files);
  const fileByPath = new Map(allFiles.map((file) => [file.path, file]));
  const suppliedPaths = result.chapters.flatMap((chapter) => Array.isArray(chapter.files) ? chapter.files : []);
  if (suppliedPaths.length !== allFiles.length || new Set(suppliedPaths).size !== allFiles.length || suppliedPaths.some((filePath) => !fileByPath.has(filePath))) return false;
  const observationByPath = new Map(observations.map((observation) => [observation.path, observation]));
  const next = [];
  for (let index = 0; index < result.chapters.length; index++) {
    const item = result.chapters[index];
    if (typeof item.headline !== "string" || typeof item.gist !== "string") return false;
    const members = item.files.map((filePath) => fileByPath.get(filePath));
    const summary = summarizeAnalyses(members.flatMap((file) => file.hunks.map((hunk) => hunk.analysis)));
    const rawAttention = Number(item.attention);
    const aiAttention = Number.isFinite(rawAttention) ? Math.max(0, Math.min(100, rawAttention <= 1 ? rawAttention * 100 : rawAttention)) : summary.attention * 100;
    const roles = members.map((file) => observationByPath.get(file.path)?.role);
    const adds = members.reduce((sum, file) => sum + file.adds, 0);
    const dels = members.reduce((sum, file) => sum + file.dels, 0);
    let id = typeof item.id === "string" && item.id.trim() ? item.id.trim().replace(/[^\w-]/g, "-").slice(0, 40) : `workstream-${index + 1}`;
    if (next.some((cluster) => cluster.id === id)) id = `${id}-${index + 1}`;
    next.push({
      id,
      key: roles.every((role) => role === "chore") ? "chores" : id,
      title: `Workstream ${index + 1}`,
      reason: summary.topSignal?.detail || "AI-grouped related changes.",
      gist: `${members.length} files · +${adds} −${dels}`,
      files: members,
      adds,
      dels,
      heuristicAttention: summary.attention,
      attention: summary.attention * 0.5 + (aiAttention / 100) * 0.5,
      counts: summary.counts,
      topSignal: summary.topSignal,
      aiHeadline: item.headline.trim().slice(0, 50),
      aiGist: item.gist.trim().slice(0, 280),
      aiBriefing: Array.isArray(item.briefing) ? item.briefing.filter((note) => typeof note === "string").slice(0, 3).map((note) => note.slice(0, 280)) : [],
      aiOrder: Number.isFinite(item.order) ? item.order : index,
    });
  }
  next.sort((left, right) => left.aiOrder - right.aiOrder);
  clusters = next;
  currentIndex = 0;
  aiState = { status: "ready", narrative: typeof result.narrative === "string" ? result.narrative.slice(0, 360) : null };
  broadcast();
  return true;
}

async function enrichWithAI() {
  if (!AI_ENABLED) return;
  const files = clusters.flatMap((cluster) => cluster.files);
  const digestText = JSON.stringify(buildAIDigest());
  const cache = readAICache();
  const isFast = files.length <= 12 && Buffer.byteLength(digestText, "utf8") <= 25 * 1024;

  if (isFast) {
    const key = crypto.createHash("sha256").update(`${AI_MODEL}\n${digestText}`).digest("hex");
    let result = cache.fast[key] || null;
    if (!result) {
      const prompt = `You are enriching a code review navigator. Heuristic signals are ground truth. Return STRICT JSON only: {"chapters":[{"id":"provided id","headline":"concrete behavior, <=50 chars","gist":"one sentence","briefing":["1-3 pointed notes naming real symbols"],"attention":0,"order":0}],"narrative":"one sentence on reading order"}. Include every id exactly once. Use integer attention 0-100. Do not repeat heuristicTitle. Never invent symbols or files.\n${digestText}`;
      try {
        result = await callAIJSON(prompt, AI_MODEL, 15_000, 2000);
        cache.fast[key] = result;
        writeAICache(cache);
      } catch {}
    }
    if (!applyAIEnrichment(result)) {
      aiState = { status: "offline", narrative: null };
      broadcast();
      return;
    }
    if (SHOT_DIR) {
      fs.mkdirSync(SHOT_DIR, { recursive: true });
      fs.writeFileSync(path.join(SHOT_DIR, "ai.json"), JSON.stringify(result, null, 2));
    }
    return;
  }

  const observations = await mapFileObservations(files, cache);
  if (SHOT_DIR) {
    fs.mkdirSync(SHOT_DIR, { recursive: true });
    fs.writeFileSync(path.join(SHOT_DIR, "observations.json"), JSON.stringify(observations, null, 2));
  }
  aiState = { status: "reducing", mapped: files.length, total: files.length, narrative: null };
  broadcast();
  const skeleton = clusters.map((cluster) => ({ id: cluster.id, title: cluster.title, files: cluster.files.map((file) => file.path), attention: Math.round(cluster.attention * 100), topSignal: cluster.topSignal?.detail || null }));
  const reduceKey = crypto.createHash("sha256").update(`${AI_MODEL}\n${JSON.stringify(observations)}\n${JSON.stringify(skeleton)}`).digest("hex");
  let result = cache.reductions[reduceKey] || null;
  let freshResult = false;
  const reduceStarted = Date.now();
  if (!result) {
    const prompt = `Compose a reviewer reading plan from per-file observations and a heuristic skeleton. Return STRICT JSON only: {"chapters":[{"id":"short workstream id","files":["exact paths"],"headline":"what this workstream DOES, <=50 chars","gist":"one sentence","briefing":["1-3 pointed notes citing observed risks/symbols"],"attention":0,"order":0}],"narrative":"one sentence on reading order"}. Cover every observed path exactly once in <=12 coherent workstreams. Order core risk, ripples, tests, then chores. Use integer attention 0-100. Never invent paths or symbols.\n${JSON.stringify({ observations, skeleton })}`;
    try {
      result = await callAIJSON(prompt, AI_MODEL, 20_000, 3000);
      freshResult = true;
    } catch {}
  }
  console.log(`Reel AI reduce: ${Date.now() - reduceStarted}ms`);
  const regrouped = applyRegroupedAI(result, observations);
  if (regrouped && freshResult) {
    cache.reductions[reduceKey] = result;
    writeAICache(cache);
  }
  if (!regrouped) {
    aiState = result && Array.isArray(result.chapters) && decisions.size > 0
      ? { status: "ready", narrative: typeof result.narrative === "string" ? result.narrative.slice(0, 360) : null }
      : { status: "offline", narrative: null };
    broadcast();
    if (aiState.status === "offline") return;
  }
  if (SHOT_DIR) fs.writeFileSync(path.join(SHOT_DIR, "ai.json"), JSON.stringify(result, null, 2));
}

/** Full detail is sent only for the selected chapter and its popup. */
function snapshot() {
  const current = clusters[currentIndex] || null;
  const serializeFile = (f) => ({
    path: f.path,
    status: f.status,
    adds: f.adds,
    dels: f.dels,
    binary: f.binary,
    untracked: f.untracked,
        hunks: f.hunks.map((h) => ({ id: h.id, header: h.header, lines: h.lines.slice(0, 400), truncated: h.lines.length > 400, vetoed: vetoes.has(h.id), approved: approvedHunks.has(h.id), analysis: h.analysis })),
  });
  return {
    sample,
    shotMode: Boolean(SHOT_DIR),
    ai: aiState,
    repo: path.basename(REPO),
    currentIndex,
    total: clusters.length,
    popupOpen,
    canUndo: history.length > 0,
    current: current && {
      id: current.id,
      title: current.aiHeadline || current.title,
      heuristicTitle: current.title,
      gist: current.aiGist || current.gist,
      briefing: current.aiBriefing || [],
      reason: current.reason,
           decision: decisions.get(current.id) || null,
      attention: current.attention,
      counts: current.counts,
      topSignal: current.topSignal,
      adds: current.adds,
      dels: current.dels,
      fileCount: current.files.length,
      files: current.files.map(serializeFile),
    },
    chapters: clusters.map((cluster) => ({
      id: cluster.id,
      title: cluster.aiHeadline || cluster.title,
      reason: cluster.reason,
      heuristicTitle: cluster.title,
      gist: cluster.aiGist || cluster.gist,
      adds: cluster.adds,
      dels: cluster.dels,
           decision: decisions.get(cluster.id) || null,
      attention: cluster.attention,
      counts: cluster.counts,
      topSignal: cluster.topSignal,
      fileCount: cluster.files.length,
      files: cluster.files.slice(0, 4).map((file) => file.path),
    })),
    recap: {
      approved: clusters.filter((c) => decisions.get(c.id) === "approved").map((c) => ({ title: c.title, gist: c.gist })),
      rejected: clusters.filter((c) => decisions.get(c.id) === "rejected").map((c) => ({ title: c.title, gist: c.gist })),
    },
        autoSkim: {
      hunks: approvedHunks.size,
      files: new Set(clusters.flatMap((cluster) => cluster.files.filter((file) => file.hunks.some((hunk) => approvedHunks.has(hunk.id))).map((file) => file.path))).size,
    },
    canApply: !sample && (clusters.some((c) => decisions.get(c.id) === "approved") || approvedHunks.size > 0) && !applyResult,
    applyResult,
  };
}

// ---------- actions ----------

function applyApproved() {
  const staged = [];
  const errors = [];
  for (const cluster of clusters) {
    for (const file of cluster.files) {
            const clusterApproved = decisions.get(cluster.id) === "approved";
      const included = file.hunks.filter((h) => !vetoes.has(h.id) && (clusterApproved || approvedHunks.has(h.id)));
      if (included.length === 0) continue;
      try {
        if (file.untracked || file.binary || included.length === file.hunks.length) {
          if (file.status === "deleted") git(["rm", "--cached", "--quiet", "--", file.path]);
          else git(["add", "--", file.path]);
          staged.push(file.path);
        } else if (included.length > 0) {
          const patch = file.headerText + included.map((h) => h.header + "\n" + h.lines.join("\n") + "\n").join("");
          git(["apply", "--cached", "--whitespace=nowarn"], patch);
          staged.push(`${file.path} (${included.length}/${file.hunks.length} hunks)`);
        }
      } catch (err) {
        errors.push(`${file.path}: ${String(err.message || err).slice(0, 120)}`);
      }
    }
  }
  applyResult = { staged: staged.length, errors };
}

function act(action) {
  switch (action.type) {
    case "decide": {
      const cluster = clusters[currentIndex];
      if (!cluster) break;
      const previous = decisions.get(cluster.id) || null;
            const priorAuto = cluster.files.flatMap((file) => file.hunks.filter((hunk) => approvedHunks.has(hunk.id)).map((hunk) => hunk.id));
      history.push({ type: "decide", clusterId: cluster.id, previous, priorAuto });
      if (previous === action.decision) decisions.delete(cluster.id);
            else decisions.set(cluster.id, action.decision);
      if (action.decision === "rejected") cluster.files.forEach((file) => file.hunks.forEach((hunk) => approvedHunks.delete(hunk.id)));
      applyResult = null;
      break;
        }
    case "acceptEzpz": {
      const cluster = clusters[currentIndex];
      if (!cluster) break;
      const ids = cluster.files.flatMap((file) => file.hunks.filter((hunk) => hunk.analysis.tier === "ezpz" && !approvedHunks.has(hunk.id)).map((hunk) => hunk.id));
            if (!ids.length && cluster.key !== "chores") break;
      const previous = decisions.get(cluster.id) || null;
      history.push({ type: "ezpz", clusterId: cluster.id, previous, ids });
      ids.forEach((id) => approvedHunks.add(id));
            if (cluster.key === "chores" || (cluster.counts.focus === 0 && cluster.counts.review === 0)) decisions.set(cluster.id, "approved");
      applyResult = null;
      break;
    }
    case "navigate":
      currentIndex = Math.max(0, Math.min(clusters.length - 1, action.index));
      break;
    case "undo": {
      const last = history.pop();
      if (!last) break;
            if (last.type === "ezpz") last.ids.forEach((id) => approvedHunks.delete(id));
      if (last.type === "decide") {
        const cluster = clusters.find((item) => item.id === last.clusterId);
        if (cluster) cluster.files.forEach((file) => file.hunks.forEach((hunk) => approvedHunks.delete(hunk.id)));
        last.priorAuto.forEach((id) => approvedHunks.add(id));
      }
      if (last.previous) decisions.set(last.clusterId, last.previous);
      else decisions.delete(last.clusterId);
      currentIndex = Math.max(0, clusters.findIndex((cluster) => cluster.id === last.clusterId));
      applyResult = null;
      break;
    }
    case "veto": {
      if (vetoes.has(action.hunkId)) vetoes.delete(action.hunkId);
            else {
        vetoes.add(action.hunkId);
        approvedHunks.delete(action.hunkId);
      }
      break;
    }
    case "popup":
      setPopup(action.open ?? !popupOpen);
      break;
    case "apply":
      if (!sample) applyApproved();
      break;
    case "reload":
      load();
      break;
    case "quit":
      app.quit();
      return;
  }
  broadcast();
}

// ---------- windows ----------

let card = null;
let popup = null;

function broadcast() {
  const state = snapshot();
  for (const win of [card, popup]) {
    if (win && !win.isDestroyed()) win.webContents.send("state", state);
  }
}

function placePopup() {
  if (!card || !popup) return;
  const b = card.getBounds();
  const display = screen.getDisplayMatching(b);
  const area = display.workArea;
  let x = b.x + b.width + GAP;
  if (x + POPUP.width > area.x + area.width) x = b.x - GAP - POPUP.width;
  const y = Math.min(Math.max(b.y, area.y), area.y + area.height - POPUP.height);
  popup.setBounds({ x, y, width: POPUP.width, height: POPUP.height });
}

function setPopup(open) {
  popupOpen = open;
  if (!popup) return;
  if (SHOT_DIR) return;
  if (open) {
    placePopup();
    popup.showInactive();
  } else {
    popup.hide();
  }
}

function makeWindow(size, extra = {}) {
  return new BrowserWindow({
    ...size,
    frame: false,
    transparent: true,
    vibrancy: "hud",
    visualEffectState: "active",
    backgroundColor: "#00000000",
    roundedCorners: true,
    hasShadow: true,
    resizable: false,
    fullscreenable: false,
    show: false,
    webPreferences: { preload: path.join(__dirname, "preload.cjs"), offscreen: Boolean(SHOT_DIR) },
    ...extra,
  });
}

app.whenReady().then(() => {
  nativeTheme.themeSource = "dark";
  Menu.setApplicationMenu(Menu.buildFromTemplate([{ role: "appMenu" }, { role: "editMenu" }]));
  load();

  ipcMain.handle("getState", () => snapshot());
  ipcMain.on("act", (_event, action) => act(action));

  let cardLoaded = false;  enrichmentPromise = enrichWithAI();

  let popupLoaded = false;
  let shotStarted = false;
  let progressShot = false;
  const captureOffscreen = async () => {

    if (!SHOT_DIR || shotStarted || !cardLoaded || !popupLoaded) return;
    shotStarted = true;
    await enrichmentPromise;
        if (clusters[0]) {
      decisions.set(clusters[0].id, "approved");
      clusters[0].files.forEach((file) => file.hunks.filter((hunk) => hunk.analysis.tier === "ezpz").forEach((hunk) => approvedHunks.add(hunk.id)));
    }
    broadcast();
    await new Promise((resolve) => setTimeout(resolve, 120));
    fs.mkdirSync(SHOT_DIR, { recursive: true });
    const decisionImage = await card.webContents.capturePage();
    fs.writeFileSync(path.join(SHOT_DIR, "reel-decision.png"), decisionImage.toPNG());
    await new Promise((resolve) => setTimeout(resolve, 380));
    const [cardImage, popupImage] = await Promise.all([card.webContents.capturePage(), popup.webContents.capturePage()]);
    fs.writeFileSync(path.join(SHOT_DIR, "reel-card.png"), cardImage.toPNG());
        fs.writeFileSync(path.join(SHOT_DIR, "reel-diff.png"), popupImage.toPNG());
    const choresIndex = clusters.findIndex((cluster) => cluster.key === "chores");
    if (choresIndex >= 0) {
      currentIndex = choresIndex;
      const chores = clusters[choresIndex];
      chores.files.forEach((file) => file.hunks.filter((hunk) => hunk.analysis.tier === "ezpz").forEach((hunk) => approvedHunks.add(hunk.id)));
      decisions.set(chores.id, "approved");
      broadcast();
      await new Promise((resolve) => setTimeout(resolve, 500));
      const choresImage = await card.webContents.capturePage();
      fs.writeFileSync(path.join(SHOT_DIR, "reel-chores.png"), choresImage.toPNG());
    }
    console.log(`Reel offscreen captures written to ${SHOT_DIR}`);
    app.quit();
  };

  card = makeWindow(CARD, { alwaysOnTop: true, vibrancy: "sidebar" });
  card.webContents.on("console-message", (_event, level, message) => console.log(`renderer[${level}] ${message}`));
  card.webContents.on("render-process-gone", (_event, details) => console.error("renderer gone", details));
  card.webContents.once("did-finish-load", () => {
    cardLoaded = true;
    aiProgressCapture = async () => {
      if (!SHOT_DIR || progressShot || aiState.status !== "mapping") return;
      progressShot = true;
      await new Promise((resolve) => setTimeout(resolve, 80));
      fs.mkdirSync(SHOT_DIR, { recursive: true });
      const image = await card.webContents.capturePage();
      fs.writeFileSync(path.join(SHOT_DIR, "reel-mapping.png"), image.toPNG());
    };
    if (!SHOT_DIR) {
      card.show();
      card.focus();
    }
    console.log("Reel widget ready");
    captureOffscreen();
  });
  card.loadFile(path.join(__dirname, "dist/index.html"));
  card.on("move", placePopup);
  card.on("closed", () => app.quit());

  popup = makeWindow(POPUP, { parent: card, alwaysOnTop: true, focusable: true, vibrancy: "hud" });
  popup.webContents.once("did-finish-load", () => {
    popupLoaded = true;
    captureOffscreen();
  });
  popup.loadFile(path.join(__dirname, "dist/index.html"), { hash: "popup" });
  popup.once("ready-to-show", () => {
    if (!SHOT_DIR) setPopup(true);
  });
});

app.on("window-all-closed", () => app.quit());
