"""Telegram bot interface."""

import asyncio
import logging
from io import BytesIO
from typing import Any, Callable, Optional

from aiogram import Bot, Dispatcher, F
from aiogram.client.default import DefaultBotProperties
from aiogram.enums import ParseMode, ChatAction
from aiogram.filters import Command, CommandStart
from aiogram.types import Message, CallbackQuery, InlineKeyboardMarkup, InlineKeyboardButton, MessageReactionUpdated

from lethe.config import Settings, get_settings
from lethe.conversation import ConversationManager
from lethe.models import MODEL_CATALOG, get_available_providers, provider_for_model, _PROVIDER_LABELS
from lethe.reaction_transport import send_message_reaction
from lethe.telegram.stickers import StickerProcessor
from lethe.transcription import TranscriptionError, transcribe_audio

logger = logging.getLogger(__name__)


class TelegramBot:
    """Async Telegram bot with interruptible conversation processing."""

    def __init__(
        self,
        settings: Optional[Settings] = None,
        conversation_manager: Optional[ConversationManager] = None,
        process_callback: Optional[Callable] = None,
        heartbeat_callback: Optional[Callable] = None,
    ):
        self.settings = settings or get_settings()
        self.conversation_manager = conversation_manager
        self.process_callback = process_callback
        self.actor_system = None  # Set after ActorSystem.setup()
        self.agent = None  # Set after agent init — needed for /model, /aux
        self.heartbeat_callback = heartbeat_callback

        self.bot = Bot(
            token=self.settings.telegram_bot_token,
            default=DefaultBotProperties(parse_mode=ParseMode.MARKDOWN),
        )
        self.dp = Dispatcher()
        self._running = False
        self._typing_tasks: dict[int, asyncio.Task] = {}
        self._sticker_tasks: dict[int, asyncio.Task] = {}
        self._last_message_id: Optional[int] = None
        self._last_chat_id: Optional[int] = None
        self._bot_user_id: Optional[int] = None
        self._sticker_processor: Optional[StickerProcessor] = None

        self._setup_handlers()

    def _setup_handlers(self):
        """Set up message handlers."""

        @self.dp.message(CommandStart())
        async def handle_start(message: Message):
            if not self._is_authorized(message.from_user.id):
                await message.answer("Unauthorized.")
                return

            await message.answer(
                "Hello! I'm Lethe, your autonomous assistant.\n\n"
                "Send me any message and I'll help you.\n\n"
                "Commands:\n"
                "/status - Check status\n"
                "/stop - Cancel current processing\n"
                "/heartbeat - Force a check-in\n"
                "/model - Switch main LLM model\n"
                "/aux - Switch auxiliary model"
            )

        @self.dp.message(Command("status"))
        async def handle_status(message: Message):
            if not self._is_authorized(message.from_user.id):
                return

            chat_id = message.chat.id
            is_processing = self.conversation_manager.is_processing(chat_id) if self.conversation_manager else False
            is_debouncing = self.conversation_manager.is_debouncing(chat_id) if self.conversation_manager else False
            pending = self.conversation_manager.get_pending_count(chat_id) if self.conversation_manager else 0

            status = "idle"
            if is_processing:
                status = "processing"
            elif is_debouncing:
                status = "waiting for more input"

            lines = [
                f"Status: {status}",
                f"Pending messages: {pending}",
            ]
            
            # Actor system info
            if self.actor_system and hasattr(self.actor_system, 'registry'):
                from lethe.actor import ActorState
                actors = self.actor_system.registry.all_actors
                
                # Separate system actors (cortex, brainstem, dmn) from user-spawned
                system_names = {"cortex", "brainstem", "dmn"}
                active = [a for a in actors if a.state in (ActorState.RUNNING, ActorState.INITIALIZING, ActorState.WAITING)]
                terminated = [a for a in actors if a.state == ActorState.TERMINATED and a.name not in system_names]
                
                # DMN status: sleeping (between rounds) or running
                dmn_active = any(a.name == "dmn" and a.state == ActorState.RUNNING for a in actors)
                dmn_status = "🟢 running" if dmn_active else "💤 sleeping (wakes on heartbeat)"
                
                # Subagents (non-system)
                subagents = [a for a in active if a.name not in system_names]
                brainstem_active = any(a.name == "brainstem" and a.state == ActorState.RUNNING for a in actors)
                
                lines.append(f"\nCortex: 🟢 active")
                lines.append(f"Brainstem: {'🟢 online' if brainstem_active else '🟡 starting'}")
                lines.append(f"DMN: {dmn_status}")
                
                if subagents:
                    lines.append(f"\nSubagents ({len(subagents)} active):")
                    for a in subagents:
                        state_emoji = {"running": "🟢", "initializing": "🟡", "waiting": "🔵"}.get(a.state.value, "⚪")
                        goals_short = a.goals[:60] + "..." if len(a.goals) > 60 else a.goals
                        lines.append(f"  {state_emoji} {a.name}: {goals_short}")
                
                if terminated:
                    lines.append(f"\nRecent ({len(terminated)}):")
                    for a in terminated[:5]:
                        goals_short = a.goals[:50] + "..." if len(a.goals) > 50 else a.goals
                        lines.append(f"  ⚫ {a.name}: {goals_short}")
                    if len(terminated) > 5:
                        lines.append(f"  ... +{len(terminated) - 5} more")

            await message.answer("\n".join(lines))

        @self.dp.message(Command("stop"))
        async def handle_stop(message: Message):
            if not self._is_authorized(message.from_user.id):
                return

            if self.conversation_manager:
                cancelled = await self.conversation_manager.cancel(message.chat.id)
                if cancelled:
                    await message.answer("Processing cancelled.")
                else:
                    await message.answer("Nothing to cancel.")

        @self.dp.message(Command("heartbeat"))
        async def handle_heartbeat(message: Message):
            if not self._is_authorized(message.from_user.id):
                return

            if self.heartbeat_callback:
                await message.answer("Triggering heartbeat...")
                await self.heartbeat_callback()
            else:
                await message.answer("Heartbeat not configured.")
        


        @self.dp.message(Command("model"))
        async def handle_model(message: Message):
            if not self._is_authorized(message.from_user.id):
                return
            await self._show_model_picker(message, "main")

        @self.dp.message(Command("aux"))
        async def handle_aux(message: Message):
            if not self._is_authorized(message.from_user.id):
                return
            await self._show_model_picker(message, "aux")

        @self.dp.callback_query(lambda c: c.data and (c.data.startswith("main:") or c.data.startswith("aux:") or c.data == "noop"))
        async def handle_model_callback(callback: CallbackQuery):
            if not callback.from_user or not self._is_authorized(callback.from_user.id):
                await callback.answer("Unauthorized")
                return
            await self._handle_model_selection(callback)

        @self.dp.message(F.text)
        async def handle_message(message: Message):
            if not self._is_authorized(message.from_user.id):
                await message.answer("Unauthorized.")
                return

            if not self.conversation_manager or not self.process_callback:
                await message.answer("Bot not fully initialized.")
                return

            self._remember_last_message(message)

            await self.conversation_manager.add_message(
                chat_id=message.chat.id,
                user_id=message.from_user.id,
                content=message.text,
                metadata=self._build_message_metadata(message),
                process_callback=self.process_callback,
            )

        @self.dp.message(F.photo)
        async def handle_photo(message: Message):
            """Handle photo messages with optional caption."""
            if not self._is_authorized(message.from_user.id):
                await message.answer("Unauthorized.")
                return

            if not self.conversation_manager or not self.process_callback:
                await message.answer("Bot not fully initialized.")
                return

            # Get the largest photo (last in the list)
            photo = message.photo[-1]
            
            # Download photo to memory and convert to base64
            import base64
            from io import BytesIO
            
            try:
                file = await self.bot.get_file(photo.file_id)
                bio = BytesIO()
                await self.bot.download_file(file.file_path, bio)
                bio.seek(0)
                image_data = base64.b64encode(bio.read()).decode('utf-8')
                
                # Determine mime type from file extension
                ext = file.file_path.split('.')[-1].lower() if file.file_path else 'jpg'
                mime_map = {'jpg': 'image/jpeg', 'jpeg': 'image/jpeg', 'png': 'image/png', 
                            'gif': 'image/gif', 'webp': 'image/webp'}
                mime_type = mime_map.get(ext, 'image/jpeg')
                
                # Build multimodal content (image-only if no caption provided).
                caption = (message.caption or "").strip()
                multimodal_content = []
                if caption:
                    multimodal_content.append({"type": "text", "text": caption})
                multimodal_content.append(
                    {"type": "image_url", "image_url": {"url": f"data:{mime_type};base64,{image_data}"}}
                )

                caption_preview = caption[:50] if caption else "(no caption)"
                logger.info(
                    f"Received photo ({photo.width}x{photo.height}) with caption: {caption_preview}..."
                )
                
                self._remember_last_message(message)

                await self.conversation_manager.add_message(
                    chat_id=message.chat.id,
                    user_id=message.from_user.id,
                    content=multimodal_content,  # Pass as list for multimodal
                    metadata=self._build_message_metadata(
                        message,
                        is_photo=True,
                        photo_size=f"{photo.width}x{photo.height}",
                    ),
                    process_callback=self.process_callback,
                )
            except Exception as e:
                logger.error(f"Failed to process photo: {e}")
                await message.answer(f"Failed to process photo: {e}")

        @self.dp.message(F.sticker)
        async def handle_sticker(message: Message):
            """Handle Telegram sticker messages with async normalization."""
            if not self._is_authorized(message.from_user.id):
                await message.answer("Unauthorized.")
                return

            if not self.conversation_manager or not self.process_callback:
                await message.answer("Bot not fully initialized.")
                return

            self._remember_last_message(message)

            task = asyncio.create_task(self._process_sticker_message(message))
            self._sticker_tasks[message.message_id] = task

            def _cleanup(_task: asyncio.Task):
                self._sticker_tasks.pop(message.message_id, None)

            task.add_done_callback(_cleanup)

        @self.dp.message(F.voice | F.audio)
        async def handle_audio_message(message: Message):
            """Handle Telegram voice/audio messages by transcribing them."""
            if not self._is_authorized(message.from_user.id):
                await message.answer("Unauthorized.")
                return

            if not self.conversation_manager or not self.process_callback:
                await message.answer("Bot not fully initialized.")
                return

            self._remember_last_message(message)

            if not self.settings.telegram_transcription_enabled:
                await message.answer("Voice transcription is disabled.")
                return

            media = message.voice or message.audio
            if not media:
                return

            is_voice = message.voice is not None
            file_name = getattr(media, "file_name", None) or f"telegram_{'voice' if is_voice else 'audio'}_{media.file_id}.ogg"
            mime_type = getattr(media, "mime_type", None) or ("audio/ogg" if is_voice else None)

            try:
                file = await self.bot.get_file(media.file_id)
                bio = BytesIO()
                await self.bot.download_file(file.file_path, bio)
                audio_bytes = bio.getvalue()
                transcript = await transcribe_audio(
                    audio_bytes,
                    filename=file_name,
                    mime_type=mime_type,
                    settings=self.settings,
                )

                caption = (message.caption or "").strip()
                media_label = "voice message" if is_voice else "audio message"
                content = f"[Transcribed {media_label}: {transcript}]"
                if caption:
                    content = f"{content}\nCaption: {caption}"

                await self.conversation_manager.add_message(
                    chat_id=message.chat.id,
                    user_id=message.from_user.id,
                    content=content,
                    metadata=self._build_message_metadata(
                        message,
                        is_voice=is_voice,
                        is_audio=not is_voice,
                        file_name=file_name,
                        mime_type=mime_type,
                        duration=getattr(media, "duration", None),
                        file_size=getattr(media, "file_size", None),
                        transcription_provider=self.settings.transcription_provider or "auto",
                        transcription_model=self.settings.transcription_model or "default",
                    ),
                    process_callback=self.process_callback,
                )
            except TranscriptionError as e:
                logger.warning("Failed to transcribe Telegram audio: %s", e)
                await message.answer(f"Failed to transcribe audio: {e}")
            except Exception as e:
                logger.exception("Failed to process Telegram audio")
                await message.answer(f"Failed to process audio: {e}")

        @self.dp.message(F.document)
        async def handle_document(message: Message):
            """Handle document/file messages - save to workspace/Downloads."""
            if not self._is_authorized(message.from_user.id):
                await message.answer("Unauthorized.")
                return

            if not self.conversation_manager or not self.process_callback:
                await message.answer("Bot not fully initialized.")
                return

            document = message.document
            file_name = document.file_name or f"file_{document.file_id}"

            self._remember_last_message(message)

            # Create Downloads directory in workspace
            downloads_dir = self.settings.workspace_dir / "Downloads"
            downloads_dir.mkdir(parents=True, exist_ok=True)
            
            # Download file
            try:
                file = await self.bot.get_file(document.file_id)
                file_path = downloads_dir / file_name
                
                await self.bot.download_file(file.file_path, file_path)
                
                logger.info(f"Received file: {file_name} ({document.file_size} bytes) -> {file_path}")
                
                # Build message with file info
                caption = message.caption or ""
                file_info = f"[Received file: {file_path}]"
                if caption:
                    content = f"{file_info}\n{caption}"
                else:
                    content = file_info
                
                await self.conversation_manager.add_message(
                    chat_id=message.chat.id,
                    user_id=message.from_user.id,
                    content=content,
                    metadata=self._build_message_metadata(
                        message,
                        is_document=True,
                        file_name=file_name,
                        file_path=str(file_path),
                        file_size=document.file_size,
                        mime_type=document.mime_type,
                    ),
                    process_callback=self.process_callback,
                )
            except Exception as e:
                logger.error(f"Failed to process document: {e}")
                await message.answer(f"Failed to download file: {e}")

        @self.dp.message_reaction()
        async def handle_message_reaction(update: MessageReactionUpdated):
            await self._process_reaction_update(update)

    def _get_sticker_processor(self) -> Optional[StickerProcessor]:
        if self._sticker_processor is not None:
            return self._sticker_processor
        if not self.agent:
            return None
        self._sticker_processor = StickerProcessor(
            settings=self.settings,
            bot=self.bot,
            llm_client_provider=lambda: self.agent.llm,
        )
        return self._sticker_processor

    async def _process_sticker_message(self, message: Message):
        processor = self._get_sticker_processor()
        sticker = message.sticker

        if not sticker:
            await message.answer("Failed to process sticker: missing payload")
            return

        try:
            if processor is None:
                logger.warning("Sticker processor unavailable; falling back to metadata-only context")
                content = self._format_sticker_fallback(sticker)
                metadata = self._build_sticker_metadata(message, None, None, "sticker processor unavailable")
            else:
                context = await processor.process(message)
                content = context.to_message_content(include_preview=processor._can_use_preview_description())
                metadata = self._build_sticker_metadata(message, context.local_path, context.preview_path, context.error)

            await self.conversation_manager.add_message(
                chat_id=message.chat.id,
                user_id=message.from_user.id,
                content=content,
                metadata=metadata,
                process_callback=self.process_callback,
            )
        except Exception as e:
            logger.exception("Failed to process Telegram sticker")
            await message.answer(f"Failed to process sticker: {e}")

    def _remember_last_message(self, message: Message) -> None:
        self._last_message_id = message.message_id
        self._last_chat_id = message.chat.id

    def _build_message_metadata(self, message: Message, **extra: Any) -> dict:
        metadata = {
            "username": message.from_user.username if message.from_user else None,
            "first_name": message.from_user.first_name if message.from_user else None,
            "message_id": message.message_id,
        }
        metadata.update(extra)
        return metadata

    def _build_sticker_metadata(self, message: Message, local_path: Optional[object], preview_path: Optional[object], error: Optional[str]) -> dict:
        sticker = message.sticker
        assert sticker is not None
        return self._build_message_metadata(
            message,
            is_sticker=True,
            file_id=sticker.file_id,
            file_unique_id=getattr(sticker, "file_unique_id", None),
            emoji=getattr(sticker, "emoji", None),
            set_name=getattr(sticker, "set_name", None),
            is_animated=bool(getattr(sticker, "is_animated", False)),
            is_video=bool(getattr(sticker, "is_video", False)),
            width=getattr(sticker, "width", None),
            height=getattr(sticker, "height", None),
            file_size=getattr(sticker, "file_size", None),
            file_path=str(local_path) if local_path else None,
            preview_path=str(preview_path) if preview_path else None,
            error=error,
        )

    def _format_sticker_fallback(self, sticker) -> str:
        parts = ["[Sticker received:"]
        if getattr(sticker, "emoji", None):
            parts.append(f' emoji="{sticker.emoji}"')
        if getattr(sticker, "set_name", None):
            parts.append(f' set="{sticker.set_name}"')
        parts.append(" visual description unavailable]")
        return "".join(parts)

    def _reaction_user(self, update: MessageReactionUpdated) -> Optional[Any]:
        return getattr(update, "user", None)

    def _reaction_values(self, reactions: Optional[list[Any]]) -> list[str]:
        values = []
        for reaction in reactions or []:
            emoji = getattr(reaction, "emoji", None)
            if emoji:
                values.append(emoji)
                continue
            custom_emoji_id = getattr(reaction, "custom_emoji_id", None)
            if custom_emoji_id:
                values.append(f"custom:{custom_emoji_id}")
                continue
            values.append(reaction.__class__.__name__)
        return values

    def _build_reaction_event(self, update: MessageReactionUpdated) -> tuple[str, dict]:
        user = self._reaction_user(update)
        old_values = self._reaction_values(getattr(update, "old_reaction", None))
        new_values = self._reaction_values(getattr(update, "new_reaction", None))
        action = "updated"
        if new_values and not old_values:
            action = "added"
        elif old_values and not new_values:
            action = "removed"
        actor_label = getattr(user, "username", None) or getattr(user, "first_name", None) or "unknown actor"
        message_id = getattr(update, "message_id", None)
        reactions_text = ", ".join(new_values or old_values or ["none"])
        content = f"[Telegram reaction {action}: {actor_label} -> message {message_id} ({reactions_text})]"
        metadata = {
            "message_id": message_id,
            "chat_id": update.chat.id,
            "reaction_update_type": "message_reaction",
            "reaction_action": action,
            "reaction_user_id": user.id if user else None,
            "reaction_user_username": getattr(user, "username", None),
            "reaction_user_name": getattr(user, "first_name", None),
            "reaction_old": old_values,
            "reaction_new": new_values,
        }
        return content, metadata

    async def _process_reaction_update(self, update: MessageReactionUpdated):
        user = self._reaction_user(update)
        if not user:
            logger.debug("Ignoring reaction update without user for message %s", getattr(update, "message_id", None))
            return
        if not self._is_authorized(user.id):
            logger.debug("Ignoring unauthorized reaction update from %s", user.id)
            return
        if self._bot_user_id and user.id == self._bot_user_id:
            logger.debug("Ignoring self-authored reaction update for message %s", getattr(update, "message_id", None))
            return
        if not self.conversation_manager or not self.process_callback:
            return

        content, metadata = self._build_reaction_event(update)
        await self.conversation_manager.add_message(
            chat_id=update.chat.id,
            user_id=user.id,
            content=content,
            metadata=metadata,
            process_callback=self.process_callback,
        )

    def _get_current_auth(self) -> str:
        """Return current auth type: 'sub' if OAuth is active, 'API' otherwise."""
        llm = self.agent.llm
        if llm._force_oauth is True:
            return "sub"
        if llm._force_oauth is False:
            return "API"
        # Auto: check if OAuth is initialized and active for current provider
        if llm._oauth and llm.config.provider == llm._oauth_provider:
            return "sub"
        return "API"

    def _build_model_buttons(self, kind: str, current: str) -> list[list[InlineKeyboardButton]]:
        """Build inline keyboard buttons for all available providers."""
        provider_infos = get_available_providers()
        if not provider_infos:
            provider_infos = [{"provider": self.agent.llm.config.provider, "label": self.agent.llm.config.provider, "auth": "API"}]

        current_auth = self._get_current_auth()

        buttons = []
        for info in provider_infos:
            provider = info["provider"]
            auth = info.get("auth", "API")
            catalog = MODEL_CATALOG.get(provider, {})
            models = catalog.get(kind, [])
            if not models:
                continue
            buttons.append([InlineKeyboardButton(text=f"── {info['label']} ──", callback_data="noop")])
            for name, model_id, pricing in models:
                # Only mark active if both model AND auth type match
                is_active = model_id == current and auth == current_auth
                marker = "✅ " if is_active else ""
                suffix = "" if auth == "sub" else f" ({pricing})"
                btn_text = f"{marker}{name}{suffix}"
                cb_data = f"{kind}:{auth}:{model_id}"
                if len(cb_data) > 64:
                    cb_data = cb_data[:64]
                buttons.append([InlineKeyboardButton(text=btn_text, callback_data=cb_data)])
        return buttons

    async def _show_model_picker(self, message: Message, kind: str):
        """Show inline keyboard with model options for main or aux model."""
        if not self.agent:
            await message.answer("Agent not initialized yet.")
            return

        current = self.agent.llm.config.model if kind == "main" else self.agent.llm.config.model_aux
        label = "Main model" if kind == "main" else "Aux model"

        buttons = self._build_model_buttons(kind, current)
        if not buttons:
            await message.answer("No models available.")
            return

        keyboard = InlineKeyboardMarkup(inline_keyboard=buttons)
        await message.answer(f"{label}: `{current}`\n\nSelect new model:", reply_markup=keyboard, parse_mode="Markdown")

    async def _handle_model_selection(self, callback: CallbackQuery):
        """Handle model/aux selection from inline keyboard.

        Callback data format: "{kind}:{auth}:{model_id}"
        e.g. "main:sub:claude-opus-4-6" or "aux:API:openrouter/google/gemini-3-flash-preview"
        """
        if not self.agent:
            await callback.answer("Agent not initialized.")
            return

        data = callback.data or ""
        if data == "noop":
            await callback.answer()
            return

        # Parse: kind:auth:model_id
        parts = data.split(":", 2)
        if len(parts) < 3:
            # Legacy format without auth: kind:model_id
            if len(parts) == 2:
                kind, model_id = parts
                auth = "API"
            else:
                await callback.answer("Unknown selection.")
                return
        else:
            kind, auth, model_id = parts

        if kind not in ("main", "aux"):
            await callback.answer("Unknown selection.")
            return

        old_model = self.agent.llm.config.model if kind == "main" else self.agent.llm.config.model_aux
        new_provider = provider_for_model(model_id) or self.agent.llm.config.provider
        force_oauth = True if auth == "sub" else False

        if kind == "main":
            changed = await self.agent.reconfigure_models(
                provider=new_provider,
                model=model_id,
                force_oauth=force_oauth,
            )
        else:
            changed = await self.agent.reconfigure_models(
                provider=new_provider,
                model_aux=model_id,
                force_oauth=force_oauth,
            )

        label = "Main model" if kind == "main" else "Aux model"
        if "provider" in changed:
            logger.info(
                "Provider changed: %s → %s",
                changed["provider"]["old"],
                changed["provider"]["new"],
            )
        logger.info(f"{label} changed: {old_model} → {model_id}")

        await callback.answer(f"Switched to {model_id}")
        # Update the message to reflect new selection
        try:
            buttons = self._build_model_buttons(kind, model_id)
            keyboard = InlineKeyboardMarkup(inline_keyboard=buttons)
            await callback.message.edit_text(
                f"{label}: `{model_id}`\n\n✅ Switched from `{old_model}`",
                reply_markup=keyboard,
                parse_mode="Markdown",
            )
        except Exception:
            pass  # Message edit can fail if unchanged

    def _is_authorized(self, user_id: int) -> bool:
        """Check if user is authorized."""
        allowed = self.settings.allowed_user_ids
        return not allowed or user_id in allowed

    async def send_message(self, chat_id: int, text: str, parse_mode: str = "Markdown"):
        """Send a message, splitting on --- for natural pauses."""
        # Skip empty messages (some models return empty responses)
        if not text or not text.strip():
            logger.warning("Skipping empty message to Telegram")
            return
            
        MAX_LENGTH = 4000  # Telegram limit is 4096
        
        # Split on --- for natural message breaks (human-like texting)
        # Each segment becomes a separate message with a pause
        segments = [s.strip() for s in text.split("---") if s.strip()]
        
        for i, segment in enumerate(segments):
            # Further split if segment is too long
            if len(segment) <= MAX_LENGTH:
                chunks = [segment]
            else:
                chunks = []
                current = ""
                for line in segment.split("\n"):
                    if len(current) + len(line) + 1 > MAX_LENGTH:
                        if current:
                            chunks.append(current)
                        current = line
                    else:
                        current = f"{current}\n{line}" if current else line
                if current:
                    chunks.append(current)
            
            for chunk in chunks:
                try:
                    await self.bot.send_message(chat_id, chunk, parse_mode=parse_mode)
                except Exception:
                    # Fallback to no parsing if markdown fails
                    await self.bot.send_message(chat_id, chunk, parse_mode=None)
                await asyncio.sleep(0.1)

            # Human-like pause: think time + typing time + jitter
            if i < len(segments) - 1:
                import random
                think = random.uniform(1.5, 3.0)
                typing = len(segment) * 0.03  # ~33 chars/sec phone typing
                pause = min(think + typing, 10.0)  # cap at 10s
                pause *= random.uniform(0.8, 1.3)  # ±jitter
                await asyncio.sleep(pause)

    async def send_photo(self, chat_id: int, photo_path: str, caption: str = ""):
        """Send a photo to chat."""
        from aiogram.types import FSInputFile
        try:
            photo = FSInputFile(photo_path)
            await self.bot.send_photo(chat_id, photo, caption=caption[:1024] if caption else None)
        except Exception as e:
            logger.error(f"Failed to send photo: {e}")
            await self.send_message(chat_id, f"[Image: {photo_path}]")
    
    async def react_to_message(self, chat_id: int, message_id: int, emoji: str = "👍"):
        """React to a message with an emoji."""
        try:
            await send_message_reaction(self.bot, chat_id, message_id, emoji)
            logger.info(f"Reacted to message {message_id} with {emoji}")
        except Exception as e:
            logger.warning(f"Failed to react to message: {e}")
    
    async def react_to_last_message(self, emoji: str = "👍"):
        """React to the last received message."""
        if self._last_chat_id and self._last_message_id:
            await self.react_to_message(self._last_chat_id, self._last_message_id, emoji)
        else:
            logger.warning("No last message to react to")

    async def start_typing(self, chat_id: int):
        """Start showing typing indicator."""
        if chat_id in self._typing_tasks:
            return

        async def typing_loop():
            while True:
                try:
                    await self.bot.send_chat_action(chat_id, ChatAction.TYPING)
                    await asyncio.sleep(4)
                except asyncio.CancelledError:
                    break
                except Exception:
                    break

        self._typing_tasks[chat_id] = asyncio.create_task(typing_loop())

    async def stop_typing(self, chat_id: int):
        """Stop showing typing indicator."""
        task = self._typing_tasks.pop(chat_id, None)
        if task:
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass

    async def start(self):
        """Start the bot."""
        self._running = True
        logger.info("Starting Telegram bot...")
        try:
            me = await self.bot.get_me()
        except Exception as e:
            logger.warning("Failed to resolve Telegram bot identity for reaction filtering: %s", e)
        else:
            self._bot_user_id = me.id
        # handle_signals=False lets us handle SIGTERM ourselves
        await self.dp.start_polling(self.bot, handle_signals=False)

    async def stop(self):
        """Stop the bot."""
        self._running = False
        # Cancel all typing tasks
        for task in self._typing_tasks.values():
            task.cancel()
        self._typing_tasks.clear()
        await self.dp.stop_polling()
        await self.bot.session.close()
        logger.info("Telegram bot stopped")
