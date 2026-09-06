# Orchestrate model guidance

Reviewed 2026-09-06. These are product routing instructions, not benchmark rankings.

The shared basis of the Fable and Astra collaboration prompts is understanding the
user's intended outcome, completing necessary details within scope, independent
judgment, concise communication, and a concrete brief for execution models.
The main thread receives workflow instructions and other peers' descriptions;
only a consulted peer receives its own self-concept as instructions.

The [official Astra guide](https://developers.openai.com/api/docs/guides/latest-model?model=gpt-6-astra)
informs the emphasis on broad technical synthesis, sustained work, useful tool
access, scope-aware initiative, and verification that stops once sufficient.
The prompt turns these into reviewable behavior rather than promising that any
particular audit or optimization will succeed.

The [Fable 5.1 prompting guide](https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/prompting-claude-fable-5-1)
supports pairing initiative with scope control, preserving intent through long
tasks, and direct communication. Fable's emphasis on intent, coherent design, and
tradeoffs is a routing lens; these capabilities are not exclusive to Fable.
The [model overview](https://platform.claude.com/docs/en/models/fable-5-1/overview)
identifies `claude-fable-5-1`.

The [Opus 5 overview](https://platform.claude.com/docs/en/models/opus-5/overview)
and [prompting guide](https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/prompting-claude-opus-5)
inform its execution description: substantial coding and review, with checks
proportionate to the task instead of repeated mandatory self-verification.
The [Anthropic effort guide](https://platform.claude.com/docs/en/build-with-claude/effort)
lists API efforts low, medium, high, xhigh, and max. tcode uses the CLI capability
catalog, which can additionally expose CLI-specific ultracode/ultrathink modes.

Keeping one entry per provider/model, limiting collaborators to medium/high,
and recommending Sol medium for routine work through max for difficult problems
are deliberate product choices. Live provider capabilities constrain the actual
choices; descriptions guide selection rather than pinning a model to one effort.

The user's experience and supplied Astra anecdote suggested audits of redundant
code, performance investigations, improvements to verification environments, and
reconsidering stuck approaches. These are consultation topics and hypotheses to
verify, not established comparative scores or authorization to merge, deploy, or
discard existing work. Unsupported ratings and latency multipliers were removed
from the bundled descriptions.
