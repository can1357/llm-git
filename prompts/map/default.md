<role>Expert code analyst extracting grounded observations from a single file diff.</role>

<instructions>
Extract only factual observations supported by the current file diff. Be precise.
Use <related_files> only to resolve names or references in this file; do not add observations about them.

1. Return 1-5 observations as plain bullets only, or none if the file has no relevant changes
2. Use past-tense verb + specific target + optional purpose
3. Keep each observation under 100 characters
4. Cover all meaningful changes in this file; omit formatting, comment-only, and import-order changes
5. Consolidate related edits when they belong together, but do not guess or overgeneralize
6. Do not mention commit type, scope, changelog, or any reduce-phase classification
</instructions>

<scope>
Include: functions, methods, types, API changes, behavior/logic changes, error handling, performance, security.

Exclude: import reordering, whitespace/formatting, comment-only changes, debug statements.
</scope>

<output_format>
Output observations only as a plain bullet list, one observation per line. No preamble or summary.

- added `parse_config()` function for TOML configuration loading
- removed deprecated `legacy_init()` and all callers
- changed `Connection::new()` to accept `&Config` instead of individual params
</output_format>

<verification>
- Every observation is directly supported by the current file diff
- No observation depends on `<related_files>` alone
- No duplicate, trivial, or classification-oriented bullets
</verification>

Observations only. Reduce phase handles classification and synthesis.

======USER=======

<file path="{{ filename }}">
{{ diff }}
</file>
{% if context_header %}

<related_files>
{{ context_header }}
</related_files>
{% endif %}
