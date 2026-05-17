from __future__ import annotations

from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

pytest.importorskip("aiogram")

from lethe.telegram import TelegramBot


class DummyReactionBot:
    def __init__(self):
        self.calls = []

    async def set_message_reaction(self, chat_id: int, message_id: int, reaction: list, **kwargs):
        self.calls.append((chat_id, message_id, reaction))


class TestTelegramReactionHelpers:
    def _make_bot(self, tmp_path, allowed_user_ids=None):
        settings = SimpleNamespace(
            telegram_bot_token="123456:ABCDEF",
            allowed_user_ids=allowed_user_ids or [],
            workspace_dir=tmp_path,
            telegram_transcription_enabled=False,
        )
        conversation_manager = SimpleNamespace(add_message=AsyncMock())
        bot = TelegramBot(
            settings=settings,
            conversation_manager=conversation_manager,
            process_callback=AsyncMock(),
        )
        return bot, conversation_manager

    def _reaction_update(self, actor_id=11, message_id=42, emoji="👍", include_user=True):
        user = None
        if include_user:
            user = SimpleNamespace(id=actor_id, username="alice", first_name="Alice")
        return SimpleNamespace(
            chat=SimpleNamespace(id=99),
            message_id=message_id,
            user=user,
            actor_chat=None,
            old_reaction=[],
            new_reaction=[SimpleNamespace(emoji=emoji)],
        )

    @pytest.mark.asyncio
    async def test_react_to_message_uses_shared_transport(self, tmp_path):
        bot, _ = self._make_bot(tmp_path)
        recorder = DummyReactionBot()
        bot.bot = recorder

        await bot.react_to_message(chat_id=99, message_id=77, emoji="🔥")

        assert recorder.calls[0][0] == 99
        assert recorder.calls[0][1] == 77
        assert getattr(recorder.calls[0][2][0], "emoji", None) == "🔥"

    def test_build_message_metadata_includes_message_id(self, tmp_path):
        bot, _ = self._make_bot(tmp_path)
        message = SimpleNamespace(
            message_id=11,
            chat=SimpleNamespace(id=22),
            from_user=SimpleNamespace(username="alice", first_name="Alice"),
        )

        metadata = bot._build_message_metadata(message, is_photo=True)

        assert metadata["message_id"] == 11
        assert metadata["is_photo"] is True

    def test_build_reaction_event_serializes_actor_and_message(self, tmp_path):
        bot, _ = self._make_bot(tmp_path)

        content, metadata = bot._build_reaction_event(self._reaction_update())

        assert content.startswith("[Telegram reaction added:")
        assert "message 42" in content
        assert metadata["message_id"] == 42
        assert metadata["reaction_new"] == ["👍"]

    @pytest.mark.asyncio
    async def test_process_reaction_update_enqueues_synthetic_event(self, tmp_path):
        bot, conversation_manager = self._make_bot(tmp_path, allowed_user_ids=[11])

        await bot._process_reaction_update(self._reaction_update(actor_id=11))

        conversation_manager.add_message.assert_awaited_once()
        kwargs = conversation_manager.add_message.await_args.kwargs
        assert kwargs["chat_id"] == 99
        assert kwargs["user_id"] == 11
        assert "Telegram reaction" in kwargs["content"]
        assert kwargs["metadata"]["message_id"] == 42
        assert kwargs["metadata"]["reaction_new"] == ["👍"]

    @pytest.mark.asyncio
    async def test_process_reaction_update_ignores_unauthorized_users(self, tmp_path):
        bot, conversation_manager = self._make_bot(tmp_path, allowed_user_ids=[1])

        await bot._process_reaction_update(self._reaction_update(actor_id=11))

        conversation_manager.add_message.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_process_reaction_update_ignores_missing_user(self, tmp_path):
        bot, conversation_manager = self._make_bot(tmp_path)

        await bot._process_reaction_update(self._reaction_update(include_user=False))

        conversation_manager.add_message.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_process_reaction_update_ignores_self_reactions(self, tmp_path):
        bot, conversation_manager = self._make_bot(tmp_path, allowed_user_ids=[7])
        bot._bot_user_id = 7

        await bot._process_reaction_update(self._reaction_update(actor_id=7))

        conversation_manager.add_message.assert_not_awaited()
