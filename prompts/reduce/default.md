Synthesize file-level observations into a unified conventional commit analysis.

<task>
From the map-phase observations below, determine:
1. Commit TYPE (single classification for entire commit)
2. SCOPE (primary component, or null if multi-component)
3. DETAILS (0-6 summary points, prefer 3-4)
4. CHANGELOG metadata for user-visible changes
</task>

<scope_rules>
1. If >=60% of changes target one component: use that component name
2. If spread across multiple components: use null
3. PROHIBITED generic scopes: src, lib, test, app, main, core, utils
4. Use scope_candidates list as primary source
</scope_rules>

<detail_format>
Each detail point must:
- Start with past-tense verb (added, fixed, moved, extracted, etc.)
- End with period
- Stay under 120 characters
- Group related changes spanning multiple files

Priority order:
1. User-visible behavior changes
2. Performance/security improvements
3. Architecture changes
4. Internal implementation details
</detail_format>

<changelog_metadata>
For each detail, include:
- changelog_category: Added | Changed | Fixed | Deprecated | Removed | Security
- user_visible: true | false

user_visible=true when:
- New features, APIs, or capabilities
- Bug fixes affecting end users
- Breaking changes
- Security fixes

user_visible=false when:
- Internal refactoring
- Test-only changes
- Build/CI infrastructure
</changelog_metadata>

--------------------
{% if types_description %}

<type_definitions>
{{ types_description }}
</type_definitions>
{% endif %}

<observations>
{{ observations }}
</observations>

<diff_statistics>
{{ stat }}
</diff_statistics>

<scope_candidates>
{{ scope_candidates }}
</scope_candidates>
