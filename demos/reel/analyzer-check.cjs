const { execFileSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");
const { analyzeFiles } = require("./analysis.cjs");

function parseDiff(text) {
  const files = [];
  for (const section of text.split(/^(?=diff --git )/m)) {
    if (!section.startsWith("diff --git ")) continue;
    const match = section.match(/^diff --git a\/(.+?) b\/(.+)$/m);
    const filePath = match?.[2] || "unknown";
    const lines = section.split("\n");
    const hunks = [];
    let current = null;
    for (const line of lines) {
      if (line.startsWith("@@")) {
        if (current) hunks.push(current);
        current = { header: line, lines: [] };
      } else if (current) current.lines.push(line);
    }
    if (current) hunks.push(current);
    const status = /\nrename from /.test(section) ? "renamed" : /\nnew file mode /.test(section) ? "added" : "modified";
    files.push({ path: filePath, status, hunks });
  }
  return files;
}

function assign(files) {
  files.forEach((file, fileIndex) => file.hunks.forEach((hunk, hunkIndex) => { hunk.id = `${fileIndex}:${hunkIndex}`; }));
  return files;
}

function printTable(label, files) {
  const analyses = analyzeFiles(assign(files));
  console.log(`\n${label}`);
  console.log("tier    score file:hunk                               signals");
  for (const file of files) {
    for (const hunk of file.hunks) {
      const analysis = analyses.get(hunk.id);
      const details = analysis.signals.map((signal) => `${signal.tag} — ${signal.detail}`).join(" | ") || "manual review";
      console.log(`${analysis.tier.padEnd(7)} ${analysis.score.toFixed(2)}  ${(file.path + ":" + hunk.header.slice(0, 22)).padEnd(40)} ${details}`);
      if (analysis.signals.some((signal) => signal.tag === "whitespace-only")) {
        const removed = hunk.lines.filter((line) => line.startsWith("-")).map((line) => line.slice(1).trim().replace(/\s+/g, " "));
        const added = hunk.lines.filter((line) => line.startsWith("+")).map((line) => line.slice(1).trim().replace(/\s+/g, " "));
        if (JSON.stringify(removed) !== JSON.stringify(added)) throw new Error(`false whitespace-only classification in ${file.path}`);
      }
    }
  }
}

const source = fs.readFileSync(path.join(__dirname, "main.cjs"), "utf8");
const sample = source.match(/const SAMPLE_DIFF = `([\s\S]*?)`;\n\n\/\/ ---------- state/)[1];
printTable("BUNDLED SAMPLE", parseDiff(sample));

const repo = process.env.REVIEW_REPO || "/work/llm-git";
let real = execFileSync("git", ["-C", repo, "diff", "--no-color", "--no-ext-diff"], { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 });
const untracked = execFileSync("git", ["-C", repo, "ls-files", "--others", "--exclude-standard"], { encoding: "utf8" }).trim().split("\n").filter(Boolean)
  .filter((file) => !/(^|\/)(bun\.lock|package-lock\.json|yarn\.lock|pnpm-lock\.yaml|uv\.lock|Cargo\.lock|[^/]+\.lockb)$|(^|\/)(node_modules|dist)\//.test(file)).slice(0, 12);
for (const file of untracked) {
  const buffer = fs.readFileSync(path.join(repo, file));
  if (buffer.subarray(0, 8000).includes(0)) continue;
  const lines = buffer.toString("utf8").split("\n").slice(0, 220);
  real += `\ndiff --git a/${file} b/${file}\nnew file mode 100644\n--- /dev/null\n+++ b/${file}\n@@ -0,0 +1,${lines.length} @@\n${lines.map((line) => "+" + line).join("\n")}`;
}
printTable("REAL REPO", parseDiff(real));
