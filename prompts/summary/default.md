Draft conventional commit summary (WITHOUT type/scope prefix).

═══════════════════════════════════════════════════════════════════════════════
READ-ONLY CONTEXT (DO NOT modify type/scope, already decided in analysis phase)
═══════════════════════════════════════════════════════════════════════════════

COMMIT TYPE (read-only): {{ commit_type }}
SCOPE (read-only): {{ scope }}
{% if user_context %}
USER CONTEXT (CRITICAL - must be incorporated into summary): {{ user_context }}
{% endif %}

DETAIL POINTS (basis for summary):
{{ details }}

DIFF STAT (supporting context):

```
{{ stat }}
```

═══════════════════════════════════════════════════════════════════════════════
YOUR TASK: Generate ONLY the description part
═══════════════════════════════════════════════════════════════════════════════

Output ONLY the text after "type(scope): " in: {{ commit_type }}({{ scope }}): <YOUR OUTPUT>
{% if user_context %}

⚠️  CRITICAL: The user-provided context above MUST be incorporated into the summary.
This is the most important part - ensure the summary reflects the user's context.
{% endif %}

REQUIREMENTS:

1. Maximum {{ chars }} characters
2. First word MUST be past-tense verb from ALLOWED LIST:
   added, fixed, updated, refactored, removed, replaced, improved, implemented,
   migrated, renamed, moved, merged, split, extracted, restructured, reorganized,
   consolidated, simplified, optimized, documented, tested, changed, introduced,
   deprecated, deleted, corrected, enhanced, reverted
3. Start lowercase
4. NO trailing period (conventional commits style)
5. Focus on primary change (single concept if scope specific)
6. NO leading adjectives before verb

FORBIDDEN PATTERNS:

- DO NOT repeat commit type "{{ commit_type }}" in summary
  If type="refactor", use: restructured, reorganized, migrated, simplified,
  consolidated, extracted (NOT "refactored")
- NO filler words: "comprehensive", "improved", "enhanced", "various", "several"
- NO "and" conjunctions cramming multiple unrelated concepts
- NO meta phrases: "this change", "this commit"

GOOD EXAMPLES (showing type in parens for clarity):

- (feat) "added TLS support with mutual authentication"
- (refactor) "migrated HTTP transport to unified builder API"
- (fix) "corrected race condition in connection pool"
- (perf) "optimized batch processing to reduce allocations"
- (build) "updated serde to 1.0.200 for security fix"

BAD EXAMPLES:

- (refactor) "refactor TLS configuration" ❌ (repeats type)
- (feat) "add comprehensive support for..." ❌ (filler + present tense)
- (chore) "update deps and improve build" ❌ (multiple concepts)
- (fix) "Fixed issue with parser" ❌ (capitalized)

CHECKLIST BEFORE RESPONDING:
✓ Summary ≤{{ chars }} chars
✓ Starts lowercase
✓ First word is past-tense verb from allowed list
✓ Does NOT repeat type "{{ commit_type }}"
✓ NO trailing period
✓ NO filler words
✓ Single focused concept
✓ Aligns with detail points
✓ Specific (names subsystem/artifact when relevant)
