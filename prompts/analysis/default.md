Analyze git changes and classify as conventional commit with detail points.

═══════════════════════════════════════════════════════════════════════════════
SECTION 1: CONTEXT
═══════════════════════════════════════════════════════════════════════════════

OVERVIEW OF CHANGES:

```
{{ stat }}
```

SCOPE SUGGESTIONS (derived from changed files, line-count weighted): {{ scope_candidates }}

DETAILED DIFF:

```diff
{{ diff }}
```

═══════════════════════════════════════════════════════════════════════════════
SECTION 2: DECISION TREE
═══════════════════════════════════════════════════════════════════════════════

TYPE CLASSIFICATION FLOWCHART:

START: Does this change add NEW public API surface?
├─ YES → feat
│  Examples: new functions/methods, new CLI flags, new endpoints, new exports
│  Diff indicators: "pub fn", "pub struct", "pub enum", "export function"
│
└─ NO ──→ Does this enable NEW user-observable capability/behavior?
   ├─ YES → feat
   │  Examples: protocol changes (HTTP→gRPC), async migration, event model
   │  Diff indicators: architectural shifts, new dependencies for features
   │
   └─ NO ──→ Does this fix incorrect behavior?
      ├─ YES → fix
      │  Examples: bugs, crashes, wrong outputs, race conditions
      │  Diff indicators: "unwrap()" → "?", bounds checks, error handling
      │
      └─ NO ──→ Does this change ONLY internal structure (provably same behavior)?
         ├─ YES → refactor
         │  Examples: renames, module reorg, extract functions, consolidate
         │  Requirement: PROOF of unchanged behavior (same tests, same API)
         │
         └─ NO ──→ Apply OTHER HEURISTICS (below)

OTHER HEURISTICS (ordered by precedence):

1. Commit msg starts "Revert" → revert
2. ONLY .md files / doc comments → docs
3. ONLY test files → test
4. ONLY lock files / .gitignore / config → chore
5. ONLY whitespace / formatting → style
6. Proven perf improvement (benchmarks) → perf
7. Build scripts / Cargo.toml / package.json → build
8. .github/workflows / CI configs → ci

CRITICAL: feat vs refactor decision

- feat: ANY observable behavior change OR new public API
  ✓ "Migrated HTTP client to async" → feat (behavior: now async)
  ✓ "Added pub fn process_batch()" → feat (new API)
  ✓ "Replaced polling with event-driven model" → feat (observable change)

- refactor: ONLY when provably unchanged
  ✓ "Renamed internal module structure" → refactor (no API/behavior change)
  ✓ "Extracted helper functions" → refactor (same outputs)
  ✗ "Migrated to gRPC" → feat (protocol = behavior change)

SCOPE EXTRACTION (>60% threshold examples):

- Use scope when: ≥60% of line changes in single dir/component
  Example: 150 lines in src/api/, 30 in src/lib.rs → scope: api
  Example: 80 lines in src/core/utime.rs, 20 elsewhere → scope: core/utime
  Example: 50 in src/api/, 50 in src/types/ → NO scope (50% each)

- Omit scope when: multi-component, cross-cutting, unclear focus
- Format: lowercase, max 2 segments, use / - \_ only
- Special scopes (even if not suggested): toolchain, deps, config

FORBIDDEN SCOPES (NEVER use these generic placeholder directories):
- src, lib, include, tests, test, benches, examples, docs
- If changes only in these dirs with no meaningful subdir, omit scope entirely

═══════════════════════════════════════════════════════════════════════════════
SECTION 3: OUTPUT SCHEMA
═══════════════════════════════════════════════════════════════════════════════

You will call function `create_change_analysis` with this JSON structure:

{
"commit_type": "feat|fix|refactor|docs|test|chore|style|perf|build|ci|revert",
"scope": "optional-scope" or null,
"details": [
"Added retry logic with exponential backoff for transient failures.",
"Migrated 8 modules to unified error type for consistency."
],
"issues": ["123", "456"] or []
}

DETAIL REQUIREMENTS (0-6 items, prefer 3-4):

1. MUST start with past-tense verb from ALLOWED LIST:
   added, fixed, updated, refactored, removed, replaced, improved, implemented,
   migrated, renamed, moved, merged, split, extracted, restructured, reorganized,
   consolidated, simplified, optimized, documented, tested, changed, introduced,
   deprecated, deleted, corrected, enhanced, reverted

2. MUST end with period

3. Balance WHAT + WHY/HOW (not just "what"):
   ✓ "Added retry logic with exponential backoff for transient failures."
   ✗ "Added retry logic." (missing why/how)

4. Abstraction levels (prefer high):
   Level 3 (BEST): Architectural, user-facing, performance
   "Replaced polling with event-driven model for 10x throughput."
   Level 2 (GOOD): Component/API changes
   "Consolidated three HTTP builders into unified API."
   Level 1 (AVOID): Low-level details
   "Renamed workspacePath to locate." ❌

5. Group ≥3 similar: "Updated 5 test files for new API." NOT 5 bullets

6. Priority order: user-visible > perf/security > architecture > internal

7. Empty array [] if no supporting details needed

EXCLUDE FROM DETAILS:

- Imports/use statements
- Whitespace/formatting/indentation
- Trivial renames (unless part of larger API change)
- Debug prints/temp logging
- Comment changes (unless substantial docs)
- File moves without modification
- Single-line tweaks/typo fixes
- Internal impl details invisible to users

WRITING RULES:

- Plain sentences (bullets/numbering added during formatting)
- Short, direct (≤120 chars per detail)
- Precise nouns (module/file/API names)
- Include why/how when meaningful
- NO meta phrases: "This commit", "Updated code", etc

JSON SCHEMA EXAMPLES:

Example 1 - Feature with new API:
{
"commit_type": "feat",
"scope": "api",
"details": [
"Added TLS mutual authentication with certificate validation.",
"Implemented builder pattern for transport configuration.",
"Migrated 6 integration tests to new API surface."
],
"issues": []
}

Example 2 - Refactor (provably unchanged):
{
"commit_type": "refactor",
"scope": "core",
"details": [
"Extracted validation logic into separate module.",
"Consolidated error handling across 12 functions.",
"Reorganized imports for better dependency clarity."
],
"issues": []
}

Example 3 - Fix with issue:
{
"commit_type": "fix",
"scope": "parser",
"details": [
"Corrected off-by-one error in buffer allocation.",
"Added bounds checking to prevent panic on empty input."
],
"issues": ["456"]
}

Example 4 - Simple change (minimal details):
{
"commit_type": "chore",
"scope": "deps",
"details": [],
"issues": []
}

ISSUE REFERENCE EXTRACTION:

- Extract from context: #123, GH-456, etc
- Return as string array or empty []

NOW ANALYZE THE DIFF AND CALL THE FUNCTION.
