Extract factual observations from this file's diff. Do NOT classify commit type or scope—the reduce phase handles that.

<rules>
## Observation Format
- Past-tense verb + specific target + optional purpose
- Max 100 characters per observation
- Consolidate related changes (e.g., "renamed 5 helper functions" not 5 separate lines)

## Include
- Functions, methods, types: added, removed, modified
- API changes: signatures, parameters, return types
- Behavior or logic changes
- Error handling changes
- Performance or security changes

## Exclude
- Import reordering
- Whitespace or formatting only
- Comment-only changes (unless substantial documentation)
- Debug statements (println!, dbg!)
</rules>

<output_format>
Return 1-5 observations as a plain list. No preamble, no summary, no markdown formatting.

Example:
- added `parse_config()` function for TOML configuration loading
- removed deprecated `legacy_init()` and all callers
- changed `Connection::new()` to accept `&Config` instead of individual params
</output_format>

--------------------

<file path="{{ filename }}">
{{ diff }}
</file>
{% if context_header %}

<related_files>
{{ context_header }}
</related_files>
{% endif %}
