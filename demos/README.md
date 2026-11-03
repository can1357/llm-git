# Reel — review widget for unstaged changes

The surviving design (of six explored — see `gallery.png` for the history): a frameless, transparent macOS desktop widget.

- **Strip** (340×640, sidebar vibrancy): snap-scroll narrative chapters — high-attention logic first, ripples, tests, then an auto-merged "easy stuff" chores chapter.
- **Diff panel** (680×700, HUD vibrancy, right-anchored, open by default): tracks the focused chapter; focus hunks expanded with signal chips, ezpz hunks collapsed to one quiet line.
- **Analysis pass** (`reel/analysis.cjs`, deterministic, no LLM): per-hunk tiers — `focus` (API signature changes, boundary/comparison edits, logic inversion, swallowed errors, security-sensitive/concurrency additions, magic-number changes, new deps, test deletions, large novel logic) vs `ezpz` (whitespace-only, comment-only, import reorder, renames, version bumps, generated files, repeated mechanical edits). Signals name real symbols from the diff. Standalone check: `node reel/analyzer-check.cjs`.
- **AI enrichment** (async, on top of heuristics): one batched call to LiteLLM (`http://localhost:4000/v1/chat/completions`, `anthropic/claude-haiku-4-5`) with the scrubbed diff + heuristic signals → plain-language headlines/gists, a grounded per-chapter reviewer briefing in the diff panel, AI/heuristic attention blend, and pre-decision chapter reordering. Cached by diff hash; 15s timeout with silent heuristic fallback; `✦ ai` footer status. Env: `REEL_AI=0` off, `REEL_AI_URL`/`REEL_AI_MODEL` override.

Run in any repo: `demos/reel/reel [dir]` — reviews the unstaged changes of `dir` (default: your current directory, widened to its git toplevel). Symlink it onto your PATH: `ln -s /work/llm-git/demos/reel/reel ~/bin/reel`, then just `reel` anywhere.
First-time setup: `cd demos/reel && bun install`. Also works as `bun start [dir]` or `REVIEW_REPO=… bun start`. Sample fallback when clean; lockfiles/`node_modules`/`dist` excluded.
Keys: `j`/`k` chapters · `←`/`→` reject/approve · `A` skim all ezpz in chapter · `⏎` toggle diff panel · `U`/`⌘Z` undo · `Q` quit. Staging = explicit gated action on recap.
Offscreen captures (no visible windows): `SHOT_DIR=/tmp/x bunx electron .`
