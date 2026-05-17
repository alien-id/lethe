---
date: 2026-05-17T14:26:19+0300
author: Volodymyr Epifanov
commit: de4a2db
branch: feature/reactions
repository: lethe-reactions
topic: "Telegram reaction emoji guard Implementation Strategy"
tags: [implementation, telegram, reactions, guard, tests]
status: complete
last_updated: 2026-05-17T14:26:19+0300
last_updated_by: Volodymyr Epifanov
type: implementation_strategy
---

# Handoff: Telegram reaction emoji guard

## Task(s)
- **Implementation plan**: `thoughts/shared/plans/2026-05-17_14-11-44_telegram-reaction-emoji-guard.md`
  - **Phase 1 complete**: guard foundation landed.
  - **Phase 2 planned**: reaction tool buffering.
  - **Phase 3 planned**: Telegram finalization wiring.
- Current session stopped after Phase 1 because the user requested a handoff before continuing implementation.

## Critical References
- `thoughts/shared/designs/2026-05-17_13-39-59_telegram-reaction-emoji-guard.md`
- `thoughts/shared/research/2026-05-17_13-28-21_telegram-reaction-emoji-guard.md`
- `thoughts/shared/plans/2026-05-17_14-11-44_telegram-reaction-emoji-guard.md`

## Recent changes
- `src/lethe/telegram_turn_guard.py:1-105` — new root helper with `PendingReaction`, `TelegramTurnGuard`, `start_telegram_turn_guard()`, `queue_pending_reaction()`, and `is_emoji_only_reply()`.
- `tests/test_telegram_turn_guard.py:1-68` — new unit coverage for emoji-only classification, deterministic channel choice, queue/drain behavior, and no-guard fallback.
- `thoughts/shared/plans/2026-05-17_14-11-44_telegram-reaction-emoji-guard.md:33-240` — plan updated with sequential phase dependencies and Phase 1 checkboxes marked complete.

## Learnings
- The turn guard can stay fully internal: `ContextVar`-based state is enough for per-turn buffering without widening the agent API.
- The emoji-only classifier in `src/lethe/telegram_turn_guard.py` is intentionally strict: any non-emoji text, punctuation, or prose causes `False`.
- The current repo already has the right seam for this work: Telegram reaction handling lives in `src/lethe/tools/telegram_tools.py`, while final Telegram text emission is in `src/lethe/main.py`.
- Local verification passed for Phase 1: `PYTHONPATH=src pytest tests/test_telegram_turn_guard.py -q` → 16 passed.

## Artifacts
- `thoughts/shared/designs/2026-05-17_13-39-59_telegram-reaction-emoji-guard.md`
- `thoughts/shared/research/2026-05-17_13-28-21_telegram-reaction-emoji-guard.md`
- `thoughts/shared/discover/2026-05-17_13-13-49_telegram-reaction-emoji-guard.md`
- `thoughts/shared/plans/2026-05-17_14-11-44_telegram-reaction-emoji-guard.md`
- `src/lethe/telegram_turn_guard.py`
- `tests/test_telegram_turn_guard.py`

## Action Items & Next Steps
1. Resume implementation at **Phase 2** of `thoughts/shared/plans/2026-05-17_14-11-44_telegram-reaction-emoji-guard.md`.
2. Update `src/lethe/tools/telegram_tools.py` to queue reactions when the turn guard is active.
3. Extend `tests/test_tools.py` with the guarded-buffering regression.
4. Implement **Phase 3** in `src/lethe/main.py` and add the mixed-turn integration test.
5. Run the full plan verification suite from the plan once all phases are done.
6. Only use a remote box if you need to validate Telegram/aiogram behavior end-to-end; Phase 1 already passed locally, so no remote box is required for the work completed so far.

## Other Notes
- The handoff is intentionally narrow: it captures only the paused implementation state, not the broader design discussion.
- If the next agent needs a quick orientation, start with the plan doc, then the design, then the new helper and tests.
- The user specifically asked about remote tests: for the completed Phase 1 work, local tests are sufficient. Remote testing is mainly useful later for Telegram integration/manual verification in Phases 2–3.
