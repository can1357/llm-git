Generate: {{ commit_type }}({{ scope }}): <YOUR OUTPUT>

<task>
Synthesize the detail points into a commit description (the text after the colon).
Maximum {{ chars }} characters. No trailing period.
</task>

<format>
1. Start with lowercase past-tense verb
2. Include WHY when it clarifies intent, not just WHAT changed
3. Name the specific subsystem, file, or component affected
4. One focused concept—no conjunctions combining unrelated changes
</format>

<verb_selection>
Your verb MUST differ from the commit type "{{ commit_type }}".

| Type     | Use instead                                        |
|----------|---------------------------------------------------|
| feat     | added, introduced, implemented, enabled            |
| fix      | corrected, resolved, patched, addressed            |
| refactor | restructured, reorganized, migrated, simplified    |
| perf     | optimized, reduced, eliminated, accelerated        |
| docs     | documented, clarified, expanded                    |
| build    | upgraded, pinned, configured                       |
| chore    | cleaned, removed, renamed, organized               |
</verb_selection>

<banned_words>
comprehensive, various, several, improved, enhanced, quickly, simply, basically, this change, this commit, now
</banned_words>

<examples>
feat | TLS encryption added to HTTP client for MITM prevention
→ added TLS support to prevent man-in-the-middle attacks

refactor | Consolidated HTTP transport into unified builder pattern
→ migrated HTTP transport to unified builder API

fix | Race condition in connection pool causing exhaustion under load
→ corrected race condition causing connection pool exhaustion

perf | Batch processing optimized to reduce memory allocations
→ eliminated allocation overhead in batch processing

build | Updated serde to fix CVE-2024-1234
→ upgraded serde to 1.0.200 for CVE-2024-1234
</examples>

<bad_output_patterns>
"added retry logic" — missing motivation
"restructured error handling" — no problem statement
"optimized database queries" — unspecific
"updated HTTP client" — which aspect?
</bad_output_patterns>

Output the description text only.

--------------------
{% if user_context %}

<user_context>
{{ user_context }}
</user_context>
{% endif %}

<detail_points>
{{ details }}
</detail_points>

<diff_stat>
{{ stat }}
</diff_stat>
