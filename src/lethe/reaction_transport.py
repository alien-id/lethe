from __future__ import annotations

from typing import Any


async def send_message_reaction(bot: Any, chat_id: int, message_id: int, emoji: str) -> None:
    from aiogram.types import ReactionTypeEmoji

    await bot.set_message_reaction(
        chat_id=chat_id,
        message_id=message_id,
        reaction=[ReactionTypeEmoji(emoji=emoji)],
    )
