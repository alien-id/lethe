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
