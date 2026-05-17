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
