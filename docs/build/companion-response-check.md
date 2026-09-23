# Companion response checks

## Constraints and intent
- Goal: owner answers reach an active coding session on its next turn without a manual check or Python parsing.
- Medium scope: Rust CLI lifecycle and local response journal; no hosted schema, API authority, Kanban, or UI changes.
- No repository CLAUDE.md, AGENTS.md, or provisioner exists in this checkout. Use surrounding Rust conventions and CI.
- Preserve source/plan ownership checks, immutable answer context, durable pending observations, explicit acknowledgments.
- No model calls, idle polling, or full-plan injection. No new answers means zero response-context tokens (existing checkpoint instruction is separate).
- Before-turn refresh is bounded, uses the locally known source plan and cursor; no all-plan discovery on every turn.
- Pending observations retain existing backpressure until handled; do not silently drop or acknowledge them.
- Inject newly observed answers compactly; repeated unhandled answers get a small reminder, not repeated full payloads.
- A resumed session must still be able to recover pending answers. Compact output must identify truncation and preserve a full retrieval path.
- Retrieval does not mean handled, execution, completion, or permission for external actions.
- Compact consume output carries question, answer, IDs, revision/fingerprint, and pending counts. Full mode remains available.
- Mechanical gate: targeted Companion tests; cargo test; cargo clippy --lib --bin wardwell -- -D warnings.
- No global formatter: baseline has unrelated formatting differences.

## Intent wiring
Active user turn -> existing Begin hook -> source identity + local plan -> bounded consume -> durable journal -> compact new-answer context -> agent handles answer -> explicit acknowledgment.
No known plan or no answers -> no response context.
Network failure -> concise truthful warning, existing work remains intact.

## Acceptance
1. An answer is surfaced on an ordinary next turn without asking to check Hank.
2. Empty checks produce no response context; duplicate delivery does not repeat the full answer.
3. Failed refresh never acknowledges or erases an answer; source mismatch is rejected.
4. Compact consume is usable directly, with safe full retrieval for omitted details.
5. Existing checkpoint and Kanban tests stay green.

## Pre-build lens
DDD/OOD and verifiability lens accepted the existing source-owned journal boundary. Biggest gate: no silent empty result on failed retrieval or truncated answers. Pending answer content stays unchanged in the journal; repeated turns may carry only a reminder with explicit retrieval instructions. This resolves repeat-token cost without acknowledging or losing data.

## Operator usage
The installed Begin hook checks the source's known plan before each active turn. It does not wake idle sessions. Manual retrieval needs no Python parsing:

```json
{"action":"consume","source_key":"<existing conversation source>","arguments":{"id":"<existing plan UUID>","compact":true}}
```

Pass that wrapper on stdin to `wardwell companion request`, or use the local MCP Companion tool. Omit `compact` for full immutable context when truncation is reported. Acknowledge only after handling the named observations successfully. Retrieval alone is not execution or completion.

## As-built wiring
Begin hook -> existing source/checkpoint -> locally known plan -> bounded consume -> durable journal -> compact response context. Repeated pending replies produce a small reminder; explicit manual reads and resume recovery retain access to the answer. Existing source ownership checks and pending-page backpressure remain in place. No UI, hosted schema, Kanban, or idle behavior changes.

The vault's legacy `/breadboard` skill name was unavailable; this document records intent and as-built wiring directly. Native app trust and successful operation inside an ordinary live Claude/Codex turn remain distinct from binary integration tests.

## Follow-up product requirements from owner
The owner must be able to assign work to recognizable initiatives (for example ADT or PCC under Corr). Agent publications must not create naming sprawl. Owner assignments must survive source revisions; handoff/session provenance remains distinct from organization. This is accepted product direction, not implemented by this binary slice.
A corrected successful publication also needs explicit, safe retirement of rejected predecessor requests; meaningful unpublished changes must never be silently discarded. Elation's blocked predecessor is an observed case, distinct from ADT's pending changed work.

## Validation
- Principle lens: complete before implementation; durable pending state and visible failures are the gate.
- Mechanical: 464 tests pass, including two real CLI turn-response integration tests and existing Kanban suites; strict lib/bin Clippy passes.
- Conventions and adversarial review: completed. Reproduced and fixed dropped owner free text, raw resume formatting, missing size bounds, and journal writer contention. Targeted tests pass after fixes.
- Compact presentation limit: three observations, 512-byte text fields, explicit truncation/full retrieval instruction. No acknowledgments from presentation.
- Native next-turn activation remains to be observed after installation; binary integration evidence is not a claim that all existing client processes reloaded.
