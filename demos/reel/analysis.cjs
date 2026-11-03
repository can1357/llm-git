// Deterministic reviewer-attention analysis over parsed unified-diff hunks.

const GENERATED_PATH = /(^|\/)(dist|__snapshots__)(\/|$)|\.(min\.js|map|snap)$/i;
const MANIFEST_PATH = /(^|\/)(package\.json|pyproject\.toml|requirements[^/]*\.txt|Cargo\.toml)$/i;
const TEST_PATH = /(^|\/)(test|tests|spec|__tests__)(\/|$)|\.(test|spec)\./i;
const COMMENT_LINE = /^\s*(?:\/\/|#|\/\*|\*|\*\/|<!--|-->|--|"""|'''|$)/;
const SIGNATURE = /\b(?:def|function|fn)\s+([A-Za-z_$][\w$]*)|\bexport\s+(?:default\s+)?(?:async\s+)?(?:function|const|let|class)\s+([A-Za-z_$][\w$]*)/;
const SECURITY = /\b(subprocess|eval|exec|os\.system|pickle|yaml\.load|token|secret|password|auth|crypto|shell\s*=\s*true)\b/i;
const CONCURRENCY = /\b(async|await|lock|mutex|thread|atomic|semaphore)\b/i;
function contentLines(hunk, prefix) {
  return hunk.lines.filter((line) => line.startsWith(prefix) && !line.startsWith(`${prefix}${prefix}${prefix}`)).map((line) => line.slice(1));
}

function sameList(left, right) {
  if (left.length !== right.length) return false;
  return left.every((value, index) => value === right[index]);
}

function collapsedWhitespace(lines) {
  return lines.map((line) => line.trim().replace(/\s+/g, " "));
}

function codeOutsideTemplates(lines) {
  const code = [];
  let insideTemplate = false;
  for (const line of lines) {
    const ticks = (line.match(/(?<!\\)`/g) || []).length;
    if (!insideTemplate) code.push(ticks ? line.slice(0, line.indexOf("`")) : line);
    if (ticks % 2 === 1) insideTemplate = !insideTemplate;
  }
  return code;
}

function commentsOnly(lines) {
  let docstring = null;
  for (const line of lines) {
    const trimmed = line.trim();
    if (docstring) {
      if (trimmed.includes(docstring)) docstring = null;
      continue;
    }
    if (trimmed.startsWith('"""') || trimmed.startsWith("'''")) {
      const marker = trimmed.slice(0, 3);
      if (!trimmed.slice(3).includes(marker)) docstring = marker;
      continue;
    }
    if (!COMMENT_LINE.test(line)) return false;
  }
  return true;
}

function importNames(lines) {
  const names = [];
  for (const line of lines) {
    const trimmed = line.trim().replace(/;$/, "");
    if (!/^(?:import\b|from\b|use\b|require\s*\()/.test(trimmed)) return null;
    const scrubbed = trimmed.replace(/^(?:import|from|use)\s+/, "").replace(/\s+from\s+['\"].*$/, "").replace(/['\"{}()*]/g, " ");
    names.push(...scrubbed.split(/[\s,.:]+/).filter((part) => part && !/^(as|require)$/.test(part)));
  }
  return names.sort();
}

function normalizedShape(file, hunk) {
  const changed = hunk.lines.filter((line) => /^[+-]/.test(line)).map((line) => {
    const normalized = line.slice(1)
      .replace(/["'`](?:\\.|[^"'`])*["'`]/g, "STR")
      .replace(/\b\d+(?:\.\d+)?\b/g, "N")
      .replace(/\b[A-Za-z_$][\w$]*\b/g, "ID")
      .replace(/\s+/g, "")
      .slice(0, 180);
    return `${line[0]}${normalized}`;
  });
  return `${file.status}:${changed.join("|")}`;
}

function pairedChanges(removed, added) {
  const pairs = [];
  const used = new Set();
  for (const before of removed) {
    let best = null;
    for (let index = 0; index < added.length; index++) {
      if (used.has(index)) continue;
      const after = added[index];
      const common = before.split(/\W+/).filter((word) => word && after.includes(word)).length;
      const total = Math.max(1, new Set([...before.split(/\W+/), ...after.split(/\W+/)]).size);
      const similarity = common / total;
      if (!best || similarity > best.similarity) best = { before, after, index, similarity };
    }
    if (best && best.similarity >= 0.35) {
      used.add(best.index);
      pairs.push(best);
    }
  }
  return pairs;
}

function analyzeHunk(file, hunk) {
  const removed = contentLines(hunk, "-");
  const added = contentLines(hunk, "+");
  const removedCode = codeOutsideTemplates(removed);
  const addedCode = codeOutsideTemplates(added);
  const signals = [];
  const add = (tag, detail, weight, kind = "focus") => signals.push({ tag, detail, weight, kind });
  const location = `${file.path}:${(hunk.header.match(/@@.*@@\s*(.*)$/) || [])[1] || "hunk"}`;

  if (file.status === "renamed" && hunk.lines.length === 0) add("pure-rename", `${file.path} is renamed without content changes`, 0.05, "ezpz");
  if (removed.length && added.length && sameList(collapsedWhitespace(removed), collapsedWhitespace(added)) && !sameList(removed, added)) {
    add("whitespace-only", `${file.path} changes indentation only`, 0.06, "ezpz");
  }
  const allChanged = [...removed, ...added];
  if (allChanged.length && commentsOnly(allChanged)) {
    add("comment-only", `${file.path} changes comments only`, 0.08, "ezpz");
  }
  const beforeImports = importNames(removed);
  const afterImports = importNames(added);
  if (beforeImports && afterImports && sameList(beforeImports, afterImports) && !sameList(removed, added)) {
    add("import-reorder", `${file.path} keeps the same imported names`, 0.07, "ezpz");
  }
  if (GENERATED_PATH.test(file.path)) add("generated", `${file.path} is generated output`, 0.04, "ezpz");

  if (MANIFEST_PATH.test(file.path)) {
    const versionBefore = removed.find((line) => /(?:^|["'])version["']?\s*[:=]|^version\s*=/.test(line));
    const versionAfter = added.find((line) => /(?:^|["'])version["']?\s*[:=]|^version\s*=/.test(line));
    if (versionBefore && versionAfter) add("version-bump", `${file.path} updates ${versionBefore.trim()} → ${versionAfter.trim()}`, 0.09, "ezpz");
    const context = hunk.lines.join("\n");
    const dependency = added.find((line) => {
      if (!/["'][@A-Za-z0-9_.\/-]+["']\s*[:=]\s*["'][^"']+["']/.test(line)) return false;
      if (/version["']?\s*[:=]/.test(line)) return false;
      const key = (line.match(/["']([^"']+)["']\s*[:=]/) || [])[1];
      if (/^(name|private|main|type|license|description|packageManager|engines|scripts|trustedDependencies|dependencies|devDependencies|peerDependencies)$/.test(key || "")) return false;
      return /dependenc|requirements|\[project\]|\[tool\.poetry/i.test(context);
    });
    if (dependency) {
      const name = (dependency.match(/["']([^"']+)["']\s*[:=]/) || [])[1] || dependency.trim();
      add("new-dependency", `${file.path} adds dependency ${name}`, 0.9);
    }
  }

  for (const line of [...removedCode, ...addedCode]) {
    const signature = line.match(SIGNATURE);
    if (signature) add("api-signature", `${file.path} changes signature ${signature[1] || signature[2]}`, 0.88);
  }

  for (const pair of pairedChanges(removedCode, addedCode)) {
    const beforeComparisons = pair.before.match(/<=|>=|==|!=|<|>/g) || [];
    const afterComparisons = pair.after.match(/<=|>=|==|!=|<|>/g) || [];
        const boundaryBase = (line) => line.replace(/<=|>=|==|!=|<|>/g, "CMP").replace(/\b([A-Za-z_$][\w$]*)\s*-\s*1\b/g, "$1").replace(/\s+/g, " ").trim();
    const offByOne = /\b[A-Za-z_$][\w$]*\s*-\s*1\b/.test(pair.before + pair.after);
    if (boundaryBase(pair.before) === boundaryBase(pair.after) && (!sameList(beforeComparisons, afterComparisons) || offByOne)) {
      add("boundary-condition", `${location} changes “${pair.before.trim()}” to “${pair.after.trim()}”`, 0.99);
    }
    const noNegation = (line) => line.replace(/\bnot\b|!(?!=)/g, "").replace(/\s+/g, " ").trim();
        const beforeNegations = pair.before.match(/\bnot\b|!(?!=)/g) || [];
    const afterNegations = pair.after.match(/\bnot\b|!(?!=)/g) || [];
    if (noNegation(pair.before) === noNegation(pair.after) && beforeNegations.length !== afterNegations.length) {
      add("logic-inversion", `${location} flips negation in “${pair.after.trim()}”`, 0.96);
    }
    const numberBefore = pair.before.match(/\b\d+(?:\.\d+)?\b/);
    const numberAfter = pair.after.match(/\b\d+(?:\.\d+)?\b/);
    if (!TEST_PATH.test(file.path) && numberBefore && numberAfter && numberBefore[0] !== numberAfter[0]) {
      const normalizedBefore = pair.before.replace(numberBefore[0], "N");
      const normalizedAfter = pair.after.replace(numberAfter[0], "N");
      if (normalizedBefore === normalizedAfter) add("magic-number", `${location} changes ${numberBefore[0]}→${numberAfter[0]}`, 0.84);
    }
  }

  const errorContext = addedCode.join("\n");
  for (const line of addedCode) {
    const passUnderExcept = /^\s*pass\s*(?:#.*)?$/.test(line) && /\bexcept\b/.test(errorContext);
    if (/\bexcept\s*:|\bexcept\s+(?:Exception|BaseException)\b|\bcatch\s*\([^)]*(?:Exception|Error|\.{3})[^)]*\)\s*\{?\s*$|\bcatch\s*(?:\([^)]*\))?\s*\{\s*\}/.test(line) || passUnderExcept) {
      add("swallowed-error", `${file.path} adds broad or empty error handling: ${line.trim()}`, 0.98);
    }
    const codeOnly = line.replace(/\/(?:\\\/|[^/\n])+\/[dgimsuvy]*/g, "REGEX").replace(/(["'`])(?:\\.|(?!\1).)*\1/g, "STR");
    const nestedDiffLine = /^\s*[+-](?!=)/.test(codeOnly);
    const sensitive = nestedDiffLine ? null : codeOnly.match(SECURITY);
    if (sensitive) add("security-sensitive", `${file.path} adds ${sensitive[1]}`, 0.91);
    const concurrent = nestedDiffLine ? null : codeOnly.match(CONCURRENCY);
    if (concurrent) add("concurrency", `${file.path} adds ${concurrent[1]}`, 0.82);
    const debt = nestedDiffLine ? null : codeOnly.match(/(?:\/\/|#|\/\*).*?\b(TODO|FIXME|HACK|XXX)\b/);
    if (debt) add("review-debt", `${file.path} adds ${debt[1]}: ${line.trim()}`, 0.78);
  }

  if (TEST_PATH.test(file.path) && removed.length >= 3 && removed.length > added.length * 1.5) {
    add("test-deletion", `${file.path} removes ${removed.length} test lines`, 0.94);
  }
  if (added.length > 40 && removed.length === 0) add("large-new-logic", `${file.path} adds ${added.length} novel lines`, 0.86);
  if (!TEST_PATH.test(file.path) && removed.length > 0 && added.length === 0) add("pure-deletion", `${file.path} deletes ${removed.length} lines`, 0.2, "ezpz");

  const unique = [];
  const seen = new Set();
  for (const signal of signals.sort((left, right) => right.weight - left.weight)) {
    const key = `${signal.tag}:${signal.detail}`;
    if (!seen.has(key)) { seen.add(key); unique.push(signal); }
  }
  return { signals: unique, shape: normalizedShape(file, hunk) };
}

function analyzeFiles(files) {
  const records = [];
  for (const file of files) {
    if (file.status === "renamed" && file.hunks.length === 0) {
      records.push({ file, hunk: null, signals: [{ tag: "pure-rename", detail: `${file.path} is renamed without content changes`, weight: 0.05, kind: "ezpz" }], shape: `rename:${file.path}` });
    }
    for (const hunk of file.hunks) records.push({ file, hunk, ...analyzeHunk(file, hunk) });
  }

  const patterns = new Map();
  for (const record of records) {
    if (!record.hunk || record.shape.length < 8) continue;
    if (!patterns.has(record.shape)) patterns.set(record.shape, []);
    patterns.get(record.shape).push(record);
  }
  for (const matches of patterns.values()) {
    if (matches.length < 4) continue;
    matches.forEach((record, index) => {
      record.signals.push(index === 0
        ? { tag: "pattern", detail: `${record.file.path} begins a repeated edit pattern ×${matches.length}`, weight: 0.42, kind: "review" }
        : { tag: "mechanical-repeat", detail: `${record.file.path} repeats the pattern from ${matches[0].file.path}`, weight: 0.05, kind: "ezpz" });
    });
  }

  const output = new Map();
  for (const record of records) {
    const high = record.signals.filter((signal) => signal.kind === "focus");
    const review = record.signals.filter((signal) => signal.kind === "review");
    const onlyMechanicalRepeat = record.signals.some((signal) => signal.tag === "mechanical-repeat") && high.length === 0;
    const tier = high.length ? "focus" : review.length ? "review" : record.signals.length ? "ezpz" : "review";
    const score = high.length ? Math.max(...high.map((signal) => signal.weight)) : tier === "review" ? Math.max(0.42, ...review.map((signal) => signal.weight)) : Math.max(0.04, ...record.signals.map((signal) => signal.weight));
    const finalTier = onlyMechanicalRepeat ? "ezpz" : tier;
    const analysis = { tier: finalTier, score: Math.max(0, Math.min(1, score)), signals: record.signals.map(({ tag, detail }) => ({ tag, detail })) };
    output.set(record.hunk ? record.hunk.id : `file:${record.file.path}`, analysis);
  }
  return output;
}

function summarizeAnalyses(analyses) {
  const items = [...analyses];
  const scores = items.map((analysis) => analysis.score);
  const counts = { focus: 0, review: 0, ezpz: 0 };
  for (const analysis of items) counts[analysis.tier]++;
  const attention = scores.length ? Math.max(...scores) * 0.65 + (scores.reduce((sum, score) => sum + score, 0) / scores.length) * 0.35 : 0;
  const top = items.flatMap((analysis) => analysis.signals.map((signal) => ({ ...signal, score: analysis.score, tier: analysis.tier }))).sort((left, right) => right.score - left.score)[0] || null;
  return { attention, counts, topSignal: top };
}

module.exports = { analyzeFiles, summarizeAnalyses };
