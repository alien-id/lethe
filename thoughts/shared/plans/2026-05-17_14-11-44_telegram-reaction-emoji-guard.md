---
date: 2026-05-17T14:11:44+0300
author: Volodymyr Epifanov
commit: de4a2db
branch: feature/reactions
repository: lethe-reactions
topic: "Telegram reaction emoji guard"
tags: [plan, telegram, reactions, guard, tests]
status: ready
parent: "thoughts/shared/designs/2026-05-17_13-39-59_telegram-reaction-emoji-guard.md"
last_updated: 2026-05-17T14:11:44+0300
last_updated_by: Volodymyr Epifanov
---

# Telegram reaction emoji guard Implementation Plan

## Overview
This plan implements the Telegram-only one-channel guard from the design artifact: buffered reactions, strict emoji-only classification, and final-turn random selection between a Telegram reaction and an emoji-only reply. The code changes stay scoped to Telegram turns only and preserve ordinary text replies.

## Desired End State
- Mixed Telegram turns choose exactly one visible expressive channel: either a reaction or an emoji-only final reply.
- Ordinary text replies stay unchanged.
- The random choice is testable by injecting the RNG.
- Reaction transport, persistence, and API/proactive behavior stay unchanged.

## What We're NOT Doing
- API/proactive guard behavior.
- Reaction transport or persistence changes.
- Schema, storage, or migration work.
- Custom or paid reaction support.
- A typed conversation/event refactor.

## Phase Dependencies
- Phases are sequential; there are no parallel phases in this plan.
- Phase 2 depends on Phase 1's `src/lethe/telegram_turn_guard.py` contract.
- Phase 3 depends on Phase 2's queued-reaction tool contract.

## Phase 1: Guard foundation

### Overview
Add the per-turn guard helper and its unit tests. This phase establishes the buffered reaction state and strict emoji-only classifier that later phases consume.

### Changes Required:

#### 1. `src/lethe/telegram_turn_guard.py`
**File**: `src/lethe/telegram_turn_guard.py`
**Changes**: NEW — root helper for per-turn buffered reactions and emoji-only classification.

```python
from __future__ import annotations

import random
import unicodedata
from contextvars import ContextVar
from dataclasses import dataclass, field
from typing import Any, Callable, Optional


_turn_guard: ContextVar[Optional["TelegramTurnGuard"]] = ContextVar(
    "telegram_turn_guard",
    default=None,
)


@dataclass(frozen=True)
class PendingReaction:
    bot: Any
    chat_id: int
    message_id: int
    emoji: str


@dataclass
class TelegramTurnGuard:
    rng: Callable[[], float] = random.random
    pending_reactions: list[PendingReaction] = field(default_factory=list)

    def queue_pending_reaction(self, bot: Any, chat_id: int, message_id: int, emoji: str) -> None:
        self.pending_reactions.append(
            PendingReaction(
                bot=bot,
                chat_id=chat_id,
                message_id=message_id,
                emoji=emoji,
            )
        )

    def has_pending_reactions(self) -> bool:
        return bool(self.pending_reactions)

    def drain_pending_reactions(self) -> list[PendingReaction]:
        pending = list(self.pending_reactions)
        self.pending_reactions.clear()
        return pending

    def choose_visible_channel(self) -> str:
        return "reaction" if self.rng() < 0.5 else "reply"


def start_telegram_turn_guard(rng: Callable[[], float] = random.random) -> TelegramTurnGuard:
    guard = TelegramTurnGuard(rng=rng)
    _turn_guard.set(guard)
    return guard


def get_telegram_turn_guard() -> Optional[TelegramTurnGuard]:
    return _turn_guard.get()


def clear_telegram_turn_guard() -> None:
    _turn_guard.set(None)


def queue_pending_reaction(bot: Any, chat_id: int, message_id: int, emoji: str) -> bool:
    guard = get_telegram_turn_guard()
    if guard is None:
        return False
    guard.queue_pending_reaction(bot=bot, chat_id=chat_id, message_id=message_id, emoji=emoji)
    return True


def _is_emoji_base_char(ch: str) -> bool:
    code = ord(ch)
    return (
        0x1F1E6 <= code <= 0x1F1FF
        or 0x1F300 <= code <= 0x1FAFF
        or 0x2600 <= code <= 0x27BF
    )


def is_emoji_only_reply(text: str) -> bool:
    if not text:
        return False

    stripped = unicodedata.normalize("NFC", text).strip()
    if not stripped:
        return False

    saw_emoji = False
    for ch in stripped:
        if ch.isspace():
            continue

        code = ord(ch)
        if ch in {"\u200d", "\ufe0f"}:
            continue
        if 0x1F3FB <= code <= 0x1F3FF:
            continue
        if _is_emoji_base_char(ch):
            saw_emoji = True
            continue
        return False

    return saw_emoji
```

#### 2. `tests/test_telegram_turn_guard.py`
**File**: `tests/test_telegram_turn_guard.py`
**Changes**: NEW — unit tests for emoji-only classification and guard buffering/draining behavior.

```python
from __future__ import annotations

from types import SimpleNamespace

import pytest

from lethe.telegram_turn_guard import (
    PendingReaction,
    clear_telegram_turn_guard,
    get_telegram_turn_guard,
    is_emoji_only_reply,
    queue_pending_reaction,
    start_telegram_turn_guard,
)


class TestEmojiOnlyReply:
    @pytest.mark.parametrize(
        "text",
        ["👍", "❤️", "🔥🔥", "👨‍👩‍👧‍👦", "👍🏻", "🇺🇦"],
    )
    def test_accepts_pure_emoji(self, text):
        assert is_emoji_only_reply(text)

    @pytest.mark.parametrize(
        "text",
        ["", "   ", "👍!", "thanks 👍", "<3", "👍 and more", "reply ❤️"],
    )
    def test_rejects_text_or_punctuation(self, text):
        assert not is_emoji_only_reply(text)


class TestTelegramTurnGuard:
    def teardown_method(self):
        clear_telegram_turn_guard()

    def test_start_and_choose_visible_channel(self):
        guard = start_telegram_turn_guard(rng=lambda: 0.25)

        assert get_telegram_turn_guard() is guard
        assert guard.choose_visible_channel() == "reaction"

        clear_telegram_turn_guard()
        reply_guard = start_telegram_turn_guard(rng=lambda: 0.75)
        assert reply_guard.choose_visible_channel() == "reply"

    def test_queue_and_drain_reactions(self):
        guard = start_telegram_turn_guard(rng=lambda: 0.75)
        bot = SimpleNamespace(name="bot")

        assert queue_pending_reaction(bot, 99, 42, "🔥") is True
        assert queue_pending_reaction(bot, 99, 43, "👍") is True
        assert guard.has_pending_reactions() is True

        pending = guard.drain_pending_reactions()

        assert pending == [
            PendingReaction(bot=bot, chat_id=99, message_id=42, emoji="🔥"),
            PendingReaction(bot=bot, chat_id=99, message_id=43, emoji="👍"),
        ]
        assert guard.has_pending_reactions() is False

    def test_queue_without_guard_returns_false(self):
        clear_telegram_turn_guard()
        bot = SimpleNamespace(name="bot")

        assert queue_pending_reaction(bot, 99, 42, "🔥") is False
        assert get_telegram_turn_guard() is None
```

### Success Criteria:

#### Automated Verification:
- [x] `PYTHONPATH=src pytest tests/test_telegram_turn_guard.py -q` passes.
- [x] Guard unit tests cover pure emoji acceptance, text/punctuation rejection, queue/drain behavior, and no-guard fallback.

#### Manual Verification:
- [ ] The helper can be instantiated with a deterministic RNG and returns stable channel choices.
- [ ] The emoji-only classifier rejects mixed prose and punctuation.

---

## Phase 2: Reaction tool buffering

### Overview
Update `telegram_react_async()` to queue reactions when the Telegram turn guard is active, while preserving the direct transport fallback when no guard is present. Extend tool tests to cover both guarded and unguarded behavior.

### Changes Required:

#### 1. `src/lethe/tools/telegram_tools.py`
**File**: `src/lethe/tools/telegram_tools.py`
**Changes**: MODIFY — queue reactions into the active guard and preserve direct reaction transport otherwise.

```python
from lethe.telegram_turn_guard import queue_pending_reaction


async def telegram_react_async(emoji: str = "👍", message_id: int = 0) -> str:
    """React to the user's last message with an emoji.

    Args:
        emoji: Emoji to react with (e.g., "👍", "❤️", "😂", "🔥", "👀")
        message_id: Optional message ID to react to. Use 0 to fall back to the
            last tracked inbound message.

    Returns:
        JSON with success status
    """
    bot = _current_bot.get()
    chat_id = _current_chat_id.get()
    target_message_id = message_id or _last_message_id.get()

    if not bot or not chat_id or not target_message_id:
        raise RuntimeError("Telegram context not set or no message to react to.")

    if queue_pending_reaction(bot, chat_id, target_message_id, emoji):
        return json.dumps({
            "success": True,
            "queued": True,
            "emoji": emoji,
            "message_id": target_message_id,
        })

    await send_message_reaction(bot, chat_id, target_message_id, emoji)

    return json.dumps({
        "success": True,
        "emoji": emoji,
        "message_id": target_message_id,
    })
```

#### 2. `tests/test_tools.py`
**File**: `tests/test_tools.py`
**Changes**: MODIFY — add regression coverage for guarded reaction buffering without breaking explicit/fallback behavior.

```python
    @pytest.mark.asyncio
    async def test_telegram_react_queues_when_guard_active(self):
        from lethe.tools.telegram_tools import (
            clear_telegram_context,
            set_last_message_id,
            set_telegram_context,
            telegram_react_async,
        )
        from lethe.telegram_turn_guard import clear_telegram_turn_guard, start_telegram_turn_guard

        bot = DummyTelegramBot()
        start_telegram_turn_guard(rng=lambda: 0.25)
        set_telegram_context(bot, 99)
        set_last_message_id(42)

        try:
            payload = json.loads(await telegram_react_async("🔥", message_id=77))
        finally:
            clear_telegram_context()
            clear_telegram_turn_guard()

        assert payload["success"] is True
        assert payload["queued"] is True
        assert payload["message_id"] == 77
        assert bot.calls == []
```

### Success Criteria:

#### Automated Verification:
- [ ] `PYTHONPATH=src pytest tests/test_tools.py -k telegram_react -q` passes.
- [ ] Guarded `telegram_react_async()` returns queued payloads and does not call direct reaction transport.
- [ ] Unguarded `telegram_react_async()` still performs the direct Telegram reaction call.

#### Manual Verification:
- [ ] A guarded reaction request reports `queued: true` in its JSON payload.
- [ ] The direct transport path still works when no guard is active.

---

## Phase 3: Telegram finalization wiring

### Overview
Seed and clear the guard around Telegram turns, then flush the chosen visible channel after `agent.chat()`. Add an integration test that proves mixed turns produce at most one visible side effect.

### Changes Required:

#### 1. `src/lethe/main.py`
**File**: `src/lethe/main.py`
**Changes**: MODIFY — start the guard at turn start, finalize it after `agent.chat()`, and clear it in cleanup paths.

```python
from typing import Callable, Optional

from lethe.reaction_transport import send_message_reaction
from lethe.telegram_turn_guard import (
    clear_telegram_turn_guard,
    get_telegram_turn_guard,
    is_emoji_only_reply,
    start_telegram_turn_guard,
)


async def _send_guarded_telegram_final_response(
    telegram_bot: TelegramBot,
    chat_id: int,
    response: str,
    mark_user_visible_activity: Callable[[str], None],
) -> None:
    guard = get_telegram_turn_guard()
    pending_reactions = guard.drain_pending_reactions() if guard else []

    if guard and is_emoji_only_reply(response):
        if pending_reactions:
            if guard.choose_visible_channel() == "reaction":
                pending = pending_reactions[0]
                await send_message_reaction(
                    pending.bot,
                    pending.chat_id,
                    pending.message_id,
                    pending.emoji,
                )
                mark_user_visible_activity("assistant reaction response")
            elif response and response.strip():
                await telegram_bot.send_message(chat_id, response)
                mark_user_visible_activity("assistant final response")
            return

    for pending in pending_reactions:
        await send_message_reaction(
            pending.bot,
            pending.chat_id,
            pending.message_id,
            pending.emoji,
        )
        mark_user_visible_activity("assistant reaction response")

    if response and response.strip():
        await telegram_bot.send_message(chat_id, response)
        mark_user_visible_activity("assistant final response")


# inside run() -> process_message(...)
        from lethe.tools import set_telegram_context, set_last_message_id, clear_telegram_context
        set_telegram_context(telegram_bot.bot, chat_id)
        if metadata.get("message_id"):
            set_last_message_id(metadata["message_id"])
        start_telegram_turn_guard()

        await telegram_bot.start_typing(chat_id)

        try:
            # Callback for intermediate messages (reasoning/thinking)
            async def on_intermediate(content: str):
                """Send intermediate updates while agent is working."""
                if not content or len(content) < 10:
                    return
                # Check for interrupt before sending
                if interrupt_check():
                    return
                # Send thinking/reasoning as-is (no emoji prefix)
                await telegram_bot.send_message(chat_id, content)
                mark_user_visible_activity("intermediate assistant update")

            # Callback for image attachments (screenshots, etc.)
            async def on_image(image_path: str):
                """Send image to user."""
                if interrupt_check():
                    return
                await telegram_bot.send_photo(chat_id, image_path)
                mark_user_visible_activity("assistant image update")

            # Get response from agent
            response = await agent.chat(message, on_message=on_intermediate, on_image=on_image)

            # Check for interrupt
            if interrupt_check():
                logger.info("Processing interrupted")
                return

            # Send response
            logger.info(f"Sending response ({len(response)} chars): {response[:80]}...")
            await _send_guarded_telegram_final_response(
                telegram_bot,
                chat_id,
                response,
                mark_user_visible_activity,
            )

        except Exception as e:
            logger.exception(f"Error processing message: {e}")
            await telegram_bot.send_message(chat_id, f"Error: {e}")
            mark_user_visible_activity("assistant error response")
        finally:
            clear_telegram_turn_guard()
            await telegram_bot.stop_typing(chat_id)
            clear_telegram_context()
```

#### 2. `tests/test_telegram_guard_integration.py`
**File**: `tests/test_telegram_guard_integration.py`
**Changes**: NEW — mixed-turn integration regression tests for exactly one visible side effect.

```python
from __future__ import annotations

import json
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock

import pytest

pytest.importorskip("aiogram")

from lethe.main import _send_guarded_telegram_final_response
from lethe.telegram_turn_guard import clear_telegram_turn_guard, start_telegram_turn_guard
from lethe.tools.telegram_tools import (
    clear_telegram_context,
    set_last_message_id,
    set_telegram_context,
    telegram_react_async,
)


class DummyTelegramBot:
    def __init__(self):
        self.send_message = AsyncMock(return_value=SimpleNamespace(message_id=1))
        self.set_message_reaction = AsyncMock()


class TestGuardedTelegramFinalization:
    def teardown_method(self):
        clear_telegram_context()
        clear_telegram_turn_guard()

    @pytest.mark.asyncio
    async def test_emoji_reply_prefers_reaction_channel(self, monkeypatch):
        bot = DummyTelegramBot()
        reaction_send = AsyncMock()
        monkeypatch.setattr("lethe.main.send_message_reaction", reaction_send)
        start_telegram_turn_guard(rng=lambda: 0.1)
        marker = Mock()
        set_telegram_context(bot, 99)
        set_last_message_id(42)

        try:
            payload = json.loads(await telegram_react_async("🔥", message_id=77))
            await _send_guarded_telegram_final_response(bot, 99, "👍", marker)
        finally:
            clear_telegram_context()
            clear_telegram_turn_guard()

        assert payload["queued"] is True
        reaction_send.assert_awaited_once()
        bot.send_message.assert_not_awaited()
        marker.assert_called_once_with("assistant reaction response")

    @pytest.mark.asyncio
    async def test_emoji_reply_prefers_text_channel(self, monkeypatch):
        bot = DummyTelegramBot()
        reaction_send = AsyncMock()
        monkeypatch.setattr("lethe.main.send_message_reaction", reaction_send)
        start_telegram_turn_guard(rng=lambda: 0.9)
        marker = Mock()
        set_telegram_context(bot, 99)
        set_last_message_id(42)

        try:
            payload = json.loads(await telegram_react_async("🔥", message_id=77))
            await _send_guarded_telegram_final_response(bot, 99, "👍", marker)
        finally:
            clear_telegram_context()
            clear_telegram_turn_guard()

        assert payload["queued"] is True
        reaction_send.assert_not_awaited()
        bot.send_message.assert_awaited_once_with(99, "👍")
        marker.assert_called_once_with("assistant final response")

    @pytest.mark.asyncio
    async def test_text_reply_flushes_pending_reactions_then_text(self, monkeypatch):
        bot = DummyTelegramBot()
        reaction_send = AsyncMock()
        monkeypatch.setattr("lethe.main.send_message_reaction", reaction_send)
        start_telegram_turn_guard(rng=lambda: 0.1)
        marker = Mock()
        set_telegram_context(bot, 99)
        set_last_message_id(42)

        try:
            await telegram_react_async("🔥", message_id=77)
            await telegram_react_async("👍", message_id=78)
            await _send_guarded_telegram_final_response(bot, 99, "Thanks for the update.", marker)
        finally:
            clear_telegram_context()
            clear_telegram_turn_guard()

        assert reaction_send.await_count == 2
        bot.send_message.assert_awaited_once_with(99, "Thanks for the update.")
        assert marker.call_count == 3
```

### Success Criteria:

#### Automated Verification:
- [ ] `PYTHONPATH=src pytest tests/test_telegram_guard_integration.py -q` passes.
- [ ] Mixed-turn tests prove only one visible side effect occurs for emoji-only replies.
- [ ] Interrupt/error paths clear the guard without flushing pending reactions.

#### Manual Verification:
- [ ] In Telegram, a mixed turn yields either a reaction or an emoji-only reply, not both.
- [ ] Ordinary text replies still appear normally.

## Testing Strategy

### Automated:
- `PYTHONPATH=src pytest tests/test_telegram_turn_guard.py tests/test_tools.py tests/test_telegram_guard_integration.py -q`
- `PYTHONPATH=src pytest tests/test_tools.py -k telegram_react -q`
- `PYTHONPATH=src pytest tests/test_telegram_turn_guard.py -q`
- `PYTHONPATH=src pytest tests/test_telegram_guard_integration.py -q`

### Manual Testing Steps:
1. Trigger a Telegram turn where Lethe both reacts and would otherwise emit an emoji-only final reply; confirm only one visible outcome appears.
2. Trigger a normal text reply; confirm it still sends normally.
3. Verify a guarded reaction request returns `queued: true` and is later flushed by finalization.

## Performance Considerations
- No new network calls; buffered reactions only delay an existing Telegram call.
- ContextVar lookup and a conservative classifier are negligible.
- The random choice is only evaluated for emoji-only mixed turns, not every message.
- Existing Telegram send chunking and jitter remain unchanged.

## Migration Notes
None. This is a runtime-only change with no schema or storage migration.

## References
- Design: `thoughts/shared/designs/2026-05-17_13-39-59_telegram-reaction-emoji-guard.md`
- Research: `thoughts/shared/research/2026-05-17_13-28-21_telegram-reaction-emoji-guard.md`
- Discover: `thoughts/shared/discover/2026-05-17_13-13-49_telegram-reaction-emoji-guard.md`
- Related design: `thoughts/shared/designs/2026-05-16_18-12-45_telegram-message-reaction-support.md`
- Related plan: `thoughts/shared/plans/2026-05-16_22-43-58_telegram-reaction-pr-polish.md`
