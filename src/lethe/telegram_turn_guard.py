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
