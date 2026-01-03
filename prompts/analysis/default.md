Classify this git diff into conventional commit format and generate changelog-ready detail points.

<instructions>
## 1. Determine Scope

Apply scope when ≥60% of line changes target a single component:
- 150 lines in src/api/, 30 in src/lib.rs → `"api"`
- 50 lines in src/api/, 50 in src/types/ → `null` (50/50 split)

**Use `null` for:**
- Cross-cutting or multi-component changes
- No dominant component (below 60% threshold)
- Project-wide refactoring

**Forbidden scopes** (always use `null`):
- Generic directories: `src`, `lib`, `include`, `tests`, `benches`, `examples`, `docs`
- Repository or project name
- Overly broad: `app`, `main`, `entire`, `all`, `misc`

**Scope priority:** Prefer scopes from `<common_scopes>` over new ones. Introduce new scopes only for components absent from history.

## 2. Generate Details (0-6 items)

Each detail must:
1. Start with past-tense verb, end with period
2. Explain impact/rationale, not just what changed
3. Use precise names (modules, APIs, files) over generic terms
4. Stay under 120 characters

**Abstraction preference:**
- BEST: "Replaced polling with event-driven model for 10x throughput."
- GOOD: "Consolidated three HTTP builders into unified API."
- AVOID: "Renamed workspacePath to locate."

**Grouping:** Combine 3+ similar changes into one bullet.
- YES: "Updated 5 test files for new API."
- NO: Five separate bullets for each test file.

**Issue references:** Include inline when applicable.
- Single: `(#123)`
- Multiple: `(#123, #456)`
- Range: `(#123-#125)`

**Priority order:** user-visible → perf/security → architecture → internal

**Exclude:**
- Import/use changes, whitespace, formatting
- Trivial renames (unless part of API change)
- Debug prints, temp logging, comment-only changes
- File moves without modification, single-line tweaks

**Constraint:** Do not fabricate motivations. If rationale isn't visible in diff, use neutral statements: "Updated logic for correctness." or "Refactored for consistency."

## 3. Assign Changelog Metadata

For each detail, set:

| Condition | `changelog_category` |
|-----------|---------------------|
| New public API, feature, capability | `"Added"` |
| Modified existing behavior | `"Changed"` |
| Bug fix, correction | `"Fixed"` |
| Feature marked for removal | `"Deprecated"` |
| Feature/API removed | `"Removed"` |
| Security fix or improvement | `"Security"` |

**`user_visible: true`** for:
- New features, APIs, breaking changes
- Bug fixes affecting users
- User-facing documentation
- Security fixes

**`user_visible: false`** for:
- Internal refactoring
- Performance optimizations (unless documented)
- Test, build, CI changes
- Code style/formatting

Omit `changelog_category` when `user_visible: false`.
</instructions>

<output_format>
Call `create_change_analysis` with:

```json
{
  "commit_type": "feat|fix|refactor|docs|test|chore|style|perf|build|ci|revert",
  "scope": "component-name" | null,
  "details": [
    {
      "text": "Past-tense description ending with period.",
      "changelog_category": "Added|Changed|Fixed|Deprecated|Removed|Security",
      "user_visible": true
    },
    {
      "text": "Internal change description.",
      "user_visible": false
    }
  ],
  "issue_refs": []
}
```
</output_format>

<examples>
<example name="feature-with-api">
```json
{
  "commit_type": "feat",
  "scope": "api",
  "details": [
    {
      "text": "Added TLS mutual authentication to prevent man-in-the-middle attacks (#100).",
      "changelog_category": "Added",
      "user_visible": true
    },
    {
      "text": "Implemented builder pattern to simplify transport configuration (#101).",
      "changelog_category": "Added",
      "user_visible": true
    },
    {
      "text": "Migrated 6 integration tests to exercise new security features.",
      "user_visible": false
    }
  ],
  "issue_refs": []
}
```
</example>

<example name="internal-refactor">
```json
{
  "commit_type": "refactor",
  "scope": "parser",
  "details": [
    {
      "text": "Extracted validation logic into separate module for reusability.",
      "user_visible": false
    },
    {
      "text": "Consolidated error handling across 12 functions to reduce duplication.",
      "user_visible": false
    }
  ],
  "issue_refs": []
}
```
</example>

<example name="bug-fix">
```json
{
  "commit_type": "fix",
  "scope": "parser",
  "details": [
    {
      "text": "Corrected off-by-one error causing buffer overflow on large inputs (#456).",
      "changelog_category": "Fixed",
      "user_visible": true
    },
    {
      "text": "Added bounds checking to prevent panic on empty files (#457).",
      "changelog_category": "Fixed",
      "user_visible": true
    }
  ],
  "issue_refs": []
}
```
</example>

<example name="mixed-visibility">
```json
{
  "commit_type": "feat",
  "scope": null,
  "details": [
    {
      "text": "Added structured error types for programmatic error recovery (#200).",
      "changelog_category": "Added",
      "user_visible": true
    },
    {
      "text": "Migrated from String errors to typed enums for better matching (#201).",
      "changelog_category": "Changed",
      "user_visible": true
    },
    {
      "text": "Updated 15 files for consistent error propagation.",
      "user_visible": false
    }
  ],
  "issue_refs": []
}
```
</example>

<example name="minimal-chore">
```json
{
  "commit_type": "chore",
  "scope": "deps",
  "details": [],
  "issue_refs": []
}
```
</example>
</examples>

--------------------
{% if project_context %}
<project_context>
{{ project_context }}
</project_context>
{% endif %}
{% if types_description %}

<commit_types>
{{ types_description }}
</commit_types>
{% endif %}

<diff_statistics>
{{ stat }}
</diff_statistics>

<scope_candidates>
{{ scope_candidates }}
</scope_candidates>
{% if common_scopes %}

<common_scopes>
{{ common_scopes }}
</common_scopes>
{% endif %}
{% if recent_commits %}

<style_patterns>
{{ recent_commits }}
</style_patterns>
{% endif %}

<diff>
{{ diff }}
</diff>
