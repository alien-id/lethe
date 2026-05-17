"""Main entry point for Lethe."""

import asyncio
import logging
import os
import signal
import sys
from typing import Callable, Optional

# Load .env file before anything else
from dotenv import load_dotenv
load_dotenv()

from rich.console import Console
from rich.logging import RichHandler

from lethe.agent import Agent
from lethe.config import get_settings
from lethe.conversation import ConversationManager
from lethe.telegram import TelegramBot
from lethe.heartbeat import Heartbeat
from lethe import console as lethe_console
from lethe.reaction_transport import send_message_reaction
from lethe.telegram_turn_guard import (
    clear_telegram_turn_guard,
    get_telegram_turn_guard,
    is_emoji_only_reply,
    start_telegram_turn_guard,
)

console = Console()


def setup_logging(verbose: bool = False):
    """Configure logging with rich output."""
    level = logging.DEBUG if verbose else logging.INFO

    logging.basicConfig(
        level=level,
        format="%(message)s",
        datefmt="[%X]",
        handlers=[RichHandler(rich_tracebacks=True, console=console)],
    )

    # Reduce noise from libraries
    logging.getLogger("aiogram").setLevel(logging.WARNING)
    logging.getLogger("httpx").setLevel(logging.WARNING)
    logging.getLogger("httpcore").setLevel(logging.WARNING)
    logging.getLogger("onnxruntime").setLevel(logging.WARNING)


async def _send_guarded_telegram_final_response(
    telegram_bot: TelegramBot,
    chat_id: int,
    response: str,
    mark_user_visible_activity: Callable[[str], None],
) -> None:
    guard = get_telegram_turn_guard()
    pending_reactions = guard.drain_pending_reactions() if guard else []

    if guard and is_emoji_only_reply(response) and pending_reactions:
        if guard.choose_visible_channel() == "reaction":
            pending = pending_reactions[0]
            await send_message_reaction(
                pending.bot,
                pending.chat_id,
                pending.message_id,
                pending.emoji,
            )
            mark_user_visible_activity("assistant reaction response")
        else:
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


async def run():
    """Run the Lethe application."""
    logger = logging.getLogger(__name__)

    try:
        settings = get_settings()
    except Exception as e:
        console.print(f"[red]Configuration error:[/red] {e}")
        console.print("\nMake sure you have a .env file with TELEGRAM_BOT_TOKEN set.")
        console.print("Also ensure OPENROUTER_API_KEY is set in your environment.")
        sys.exit(1)

    console.print("[bold blue]Lethe[/bold blue] - Autonomous AI Assistant")
    console.print(f"Model: {settings.llm_model}")
    console.print(f"Memory: {settings.memory_dir}")
    console.print()

    # Initialize agent (tools auto-loaded)
    console.print("[dim]Initializing agent...[/dim]")
    agent = Agent(settings)
    await agent.initialize()  # Async init: load history with summarization
    agent.refresh_memory_context()
    
    # Initialize actor system (subagent support)
    actor_system = None
    if os.environ.get("ACTORS_ENABLED", "true").lower() == "true":
        from lethe.actor.integration import ActorSystem
        actor_system = ActorSystem(agent, settings=settings)
        await actor_system.setup()
        console.print("[cyan]Actor system[/cyan] initialized (brainstem + cortex + DMN)")
    
    stats = agent.get_stats()
    console.print(f"[green]Agent ready[/green] - {stats['memory_blocks']} blocks, {stats['archival_memories']} memories")

    # Initialize console (mind state visualization) if enabled
    console_enabled = os.environ.get("LETHE_CONSOLE", "false").lower() == "true"
    console_port = int(os.environ.get("LETHE_CONSOLE_PORT", 8777))
    console_host = os.environ.get("LETHE_CONSOLE_HOST", "127.0.0.1")

    if console_enabled:
        from lethe.console.ui import run_console
        await run_console(port=console_port, host=console_host)
        console.print(f"[cyan]Console[/cyan] running at http://{console_host}:{console_port}")
        
        # Initialize console state with current data
        lethe_console.update_stats(stats['total_messages'], stats['archival_memories'])
        
        # Load identity
        identity_block = agent.memory.blocks.get("identity")
        lethe_console.update_identity(identity_block.get("value", "") if identity_block else "")
        
        # Load all memory blocks
        all_blocks = agent.memory.blocks.list_blocks()
        lethe_console.update_memory_blocks(all_blocks)
        
        # Load recent messages from context
        lethe_console.update_messages(agent.llm.context.messages)
        
        # Load summary if available
        if agent.llm.context.summary:
            lethe_console.update_summary(agent.llm.context.summary)
        
        # Capture initial context (what would be sent to LLM)
        initial_context = agent.llm.context.build_messages()
        token_estimate = agent.llm.context.count_tokens(str(initial_context))
        lethe_console.update_context(initial_context, token_estimate)
        
        # Model info
        lethe_console.update_model_info(settings.llm_model, settings.llm_model_aux)
        
        # Hook into agent for state updates
        agent.set_console_hooks(
            on_context_build=lambda ctx, tokens: lethe_console.update_context(ctx, tokens),
            on_status_change=lambda status, tool: lethe_console.update_status(status, tool),
            on_memory_change=lambda blocks: lethe_console.update_memory_blocks(blocks),
            on_token_usage=None,
        )

    # Initialize conversation manager
    conversation_manager = ConversationManager(debounce_seconds=settings.debounce_seconds)
    logger.info(f"Conversation manager initialized (debounce: {settings.debounce_seconds}s)")
    heartbeat: Optional[Heartbeat] = None
    fallback_chat_id: Optional[int] = settings.allowed_user_ids[0] if settings.allowed_user_ids else None
    last_user_chat_id: Optional[int] = None

    def get_target_chat_id() -> Optional[int]:
        return last_user_chat_id or fallback_chat_id

    def mark_user_visible_activity(reason: str) -> None:
        """Reset synthetic idle state after real user-visible activity."""
        removed = agent.llm.clear_idle_markers()
        if removed:
            logger.info("Cleared %d idle marker(s) after %s", removed, reason)
        if heartbeat:
            heartbeat.reset_idle_timer(reason)

    # Message processing callback
    async def process_message(chat_id: int, user_id: int, message: str, metadata: dict, interrupt_check):
        """Process a message from Telegram."""
        from lethe.tools import set_telegram_context, set_last_message_id, clear_telegram_context
        nonlocal last_user_chat_id
        
        logger.info(f"Processing message from {user_id}: {message[:50]}...")
        last_user_chat_id = chat_id
        mark_user_visible_activity("incoming user message")
        
        # Set telegram context for tools (reactions, sending messages)
        set_telegram_context(telegram_bot.bot, chat_id)
        if metadata.get("message_id"):
            set_last_message_id(metadata["message_id"])
        start_telegram_turn_guard()
        
        # Start typing indicator
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

    # Initialize Telegram bot
    telegram_bot = TelegramBot(
        settings,
        conversation_manager=conversation_manager,
        process_callback=process_message,
    )
    telegram_bot.agent = agent  # For /model, /aux commands
    # heartbeat_callback will be set below after Heartbeat is created

    # Initialize heartbeat
    heartbeat_interval = int(os.environ.get("HEARTBEAT_INTERVAL", 60 * 60))  # Default 1 hour
    heartbeat_enabled = os.environ.get("HEARTBEAT_ENABLED", "true").lower() == "true"
    
    async def heartbeat_process(message: str) -> str:
        """Process heartbeat — triggers background rounds if actor system is active."""
        if actor_system:
            await actor_system.brainstem_heartbeat(message)
            result = await actor_system.background_round()
            return result or "ok"
        return await agent.heartbeat(message)
    
    async def heartbeat_full_context(message: str) -> str:
        """Full context heartbeat — triggers supervision + background rounds."""
        if actor_system:
            await actor_system.brainstem_heartbeat(message)
            result = await actor_system.background_round()
            return result or "ok"
        return await agent.chat(message, use_hippocampus=False)
    
    # --- Proactive message rate limiter (hard enforcement) ---
    _proactive_sends: list[float] = []  # timestamps of proactive messages sent
    _proactive_max = settings.proactive_max_per_day
    _proactive_cooldown = settings.proactive_cooldown_minutes * 60  # seconds

    def _proactive_allowed() -> bool:
        """Check if a proactive message is allowed right now."""
        import time
        now = time.time()
        # Prune old entries (older than 24h)
        while _proactive_sends and (now - _proactive_sends[0]) > 86400:
            _proactive_sends.pop(0)
        # Check daily budget
        if _proactive_max > 0 and len(_proactive_sends) >= _proactive_max:
            logger.info("Proactive message blocked: daily limit (%d/%d)", len(_proactive_sends), _proactive_max)
            return False
        # Check cooldown
        if _proactive_sends and (now - _proactive_sends[-1]) < _proactive_cooldown:
            remaining = int(_proactive_cooldown - (now - _proactive_sends[-1]))
            logger.info("Proactive message blocked: cooldown (%d seconds remaining)", remaining)
            return False
        return True

    def _proactive_record():
        """Record that a proactive message was sent."""
        import time
        _proactive_sends.append(time.time())

    async def heartbeat_send(response: str):
        """Send heartbeat response to user (rate-limited)."""
        target_chat_id = get_target_chat_id()
        if not target_chat_id:
            logger.info("Heartbeat message suppressed: no active chat target")
            return
        if not _proactive_allowed():
            logger.info("Heartbeat message suppressed by rate limiter")
            return
        await telegram_bot.send_message(target_chat_id, response)
        _proactive_record()
        mark_user_visible_activity("proactive outbound message")
    
    async def heartbeat_summarize(prompt: str) -> str:
        """Summarize/evaluate heartbeat response before sending (uses aux model)."""
        return await agent.llm.complete(prompt, use_aux=True)

    async def heartbeat_idle(minutes_passed: int):
        """Record idle passage-of-time as a single user-role timeline block."""
        agent.llm.note_idle_interval(minutes_passed)

    async def get_active_reminders() -> str:
        """Get active reminders as formatted string."""
        from lethe.todos import TodoManager
        todo_manager = TodoManager(settings.db_path)
        todos = await todo_manager.list(status="pending")
        
        if not todos:
            return ""
        
        lines = []
        for todo in todos[:10]:  # Limit to 10
            priority = todo.get("priority", "normal")
            due = todo.get("due_at", "")
            due_str = f" (due: {due})" if due else ""
            lines.append(f"- [{priority}] {todo['title']}{due_str}")
        
        return "\n".join(lines)

    heartbeat = Heartbeat(
        process_callback=heartbeat_process,
        send_callback=heartbeat_send,
        summarize_callback=heartbeat_summarize,
        full_context_callback=heartbeat_full_context,
        get_reminders_callback=get_active_reminders,
        idle_callback=heartbeat_idle,
        interval=heartbeat_interval,
        enabled=heartbeat_enabled,
    )
    
    # Set heartbeat trigger on telegram bot for /heartbeat command
    telegram_bot.heartbeat_callback = heartbeat.trigger
    
    # Wire actor system into telegram bot for /status command
    if actor_system:
        telegram_bot.actor_system = actor_system
    
    async def run_cortex_turn(synthetic_message: str):
        """Trigger a full cortex LLM turn with a synthetic system message.

        Used when a subagent finishes so the cortex can process the result
        and respond to the user proactively.
        """
        target_chat_id = get_target_chat_id()
        if not target_chat_id:
            logger.warning("run_cortex_turn: no chat_id configured")
            return
        # No rate limiting — subagent results are responses to user-initiated tasks.
        from lethe.tools import set_telegram_context, clear_telegram_context
        set_telegram_context(telegram_bot.bot, target_chat_id)
        try:
            await telegram_bot.start_typing(target_chat_id)
            response = await agent.chat(synthetic_message)
            if response and response.strip():
                await telegram_bot.send_message(target_chat_id, response)
                mark_user_visible_activity("cortex subagent followup")
        except Exception as e:
            logger.exception("run_cortex_turn failed: %s", e)
        finally:
            await telegram_bot.stop_typing(target_chat_id)
            clear_telegram_context()

    # Wire DMN callbacks (send_to_user, get_reminders)
    if actor_system:
        actor_system.set_callbacks(
            send_to_user=heartbeat_send,
            get_reminders=get_active_reminders,
            run_cortex_turn=run_cortex_turn,
        )

    # Console monitoring pump for dynamic runtime subsystems.
    console_monitor_task = None
    if console_enabled:
        async def monitor_console_state():
            while True:
                try:
                    stats = agent.get_stats()
                    lethe_console.update_stats(stats['total_messages'], stats['archival_memories'])
                    lethe_console.update_messages(agent.llm.context.messages)
                    lethe_console.update_summary(agent.llm.context.summary or "")
                    lethe_console.update_hippocampus(agent.hippocampus.get_stats())
                    lethe_console.update_hippocampus_context(agent.hippocampus.get_context_view())
                    if actor_system:
                        lethe_console.update_actor_status(actor_system.status)
                        if actor_system.brainstem:
                            lethe_console.update_stem_context(actor_system.brainstem.get_context_view())
                        if actor_system.dmn:
                            lethe_console.update_dmn_context(actor_system.dmn.get_context_view())
                        # Amygdala removed: salience stats now in hippocampus context view
                except asyncio.CancelledError:
                    raise
                except Exception as e:
                    logger.warning(f"Console monitor update failed: {e}")
                await asyncio.sleep(2.0)

        console_monitor_task = asyncio.create_task(
            monitor_console_state(),
            name="console-monitor",
        )

    # Set up shutdown handling
    shutdown_event = asyncio.Event()

    def signal_handler():
        logger.info("Received shutdown signal...")
        shutdown_event.set()
        # Force exit after 3 seconds using a thread (not event loop)
        # This ensures exit even if event loop is blocked
        import threading
        def force_exit():
            import time
            time.sleep(3)
            logger.warning("Graceful shutdown timed out, forcing exit")
            os._exit(0)
        threading.Thread(target=force_exit, daemon=True).start()

    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, signal_handler)

    # Start services
    console.print("[green]Starting services...[/green]")

    bot_task = asyncio.create_task(telegram_bot.start())
    heartbeat_task = asyncio.create_task(heartbeat.start())
    asyncio.create_task(agent.run_startup_curator())

    if stats['total_messages'] == 0 and fallback_chat_id:
        async def onboarding():
            await asyncio.sleep(3)
            try:
                response = await agent.chat(
                    "You are Lethe. This is your very first conversation with your principal. "
                    "Introduce yourself as Lethe and ask them to tell you about themselves "
                    "so you can remember who they are. Two to three sentences."
                )
                await telegram_bot.send_message(fallback_chat_id, response)
                logger.info("Onboarding message sent")
            except Exception as e:
                logger.warning("Onboarding failed: %s", e)
        asyncio.create_task(onboarding())

    try:
        await shutdown_event.wait()
    except asyncio.CancelledError:
        pass
    finally:
        console.print("\n[yellow]Shutting down...[/yellow]")
        
        # Shutdown with timeout to avoid hanging on native threads
        try:
            async with asyncio.timeout(5):
                if console_monitor_task:
                    console_monitor_task.cancel()
                    try:
                        await console_monitor_task
                    except asyncio.CancelledError:
                        pass
                if actor_system:
                    await actor_system.shutdown()
                await heartbeat.stop()
                await telegram_bot.stop()
                await agent.close()
        except asyncio.TimeoutError:
            logger.warning("Shutdown timed out, forcing exit")
            os._exit(0)  # Force exit - LanceDB/OpenBLAS threads don't respect Python shutdown
        
        bot_task.cancel()
        heartbeat_task.cancel()
        try:
            await bot_task
        except asyncio.CancelledError:
            pass
        try:
            await heartbeat_task
        except asyncio.CancelledError:
            pass
        
        console.print("[green]Shutdown complete.[/green]")


async def run_api(port: int = 8080):
    """Run Lethe in HTTP API mode for gateway architecture."""
    logger = logging.getLogger(__name__)

    try:
        settings = get_settings()
    except Exception as e:
        console.print(f"[red]Configuration error:[/red] {e}")
        sys.exit(1)

    console.print("[bold blue]Lethe[/bold blue] - API Mode")
    console.print(f"Model: {settings.llm_model}")
    console.print(f"Memory: {settings.memory_dir}")
    console.print()

    if not os.environ.get("LETHE_API_TOKEN", "").strip():
        console.print("[red]LETHE_API_TOKEN must be set in API mode.[/red]")
        sys.exit(1)

    # Initialize agent
    console.print("[dim]Initializing agent...[/dim]")
    agent = Agent(settings)
    await agent.initialize()
    agent.refresh_memory_context()

    # Initialize actor system
    actor_system = None
    if os.environ.get("ACTORS_ENABLED", "true").lower() == "true":
        from lethe.actor.integration import ActorSystem
        actor_system = ActorSystem(agent, settings=settings)
        await actor_system.setup()
        console.print("[cyan]Actor system[/cyan] initialized")

    stats = agent.get_stats()
    console.print(f"[green]Agent ready[/green] - {stats['memory_blocks']} blocks, {stats['archival_memories']} memories")

    # Initialize conversation manager
    conversation_manager = ConversationManager(debounce_seconds=settings.debounce_seconds)

    # Set up the API module globals
    from lethe import api as api_module
    api_module._agent = agent
    api_module._conversation_manager = conversation_manager
    api_module._actor_system = actor_system
    api_module._settings = settings

    # Initialize heartbeat with proactive messages going to /events SSE
    heartbeat_interval = int(os.environ.get("HEARTBEAT_INTERVAL", 60 * 60))  # Default 1 hour
    heartbeat_enabled = os.environ.get("HEARTBEAT_ENABLED", "true").lower() == "true"

    async def heartbeat_process(message: str) -> str:
        if actor_system:
            await actor_system.brainstem_heartbeat(message)
            result = await actor_system.background_round()
            return result or "ok"
        return await agent.heartbeat(message)

    async def heartbeat_full_context(message: str) -> str:
        if actor_system:
            await actor_system.brainstem_heartbeat(message)
            result = await actor_system.background_round()
            return result or "ok"
        return await agent.chat(message, use_hippocampus=False)

    async def heartbeat_send(response: str):
        await api_module.send_proactive(response)

    async def heartbeat_summarize(prompt: str) -> str:
        return await agent.llm.complete(prompt, use_aux=True)

    async def heartbeat_idle(minutes_passed: int):
        agent.llm.note_idle_interval(minutes_passed)

    async def get_active_reminders() -> str:
        from lethe.todos import TodoManager
        todo_manager = TodoManager(settings.db_path)
        todos = await todo_manager.list(status="pending")
        if not todos:
            return ""
        lines = []
        for todo in todos[:10]:
            priority = todo.get("priority", "normal")
            due = todo.get("due_at", "")
            due_str = f" (due: {due})" if due else ""
            lines.append(f"- [{priority}] {todo['title']}{due_str}")
        return "\n".join(lines)

    heartbeat = Heartbeat(
        process_callback=heartbeat_process,
        send_callback=heartbeat_send,
        summarize_callback=heartbeat_summarize,
        full_context_callback=heartbeat_full_context,
        get_reminders_callback=get_active_reminders,
        idle_callback=heartbeat_idle,
        interval=heartbeat_interval,
        enabled=heartbeat_enabled,
    )
    api_module._heartbeat = heartbeat

    # Wire actor system callbacks
    if actor_system:
        async def run_cortex_turn(synthetic_message: str):
            from lethe.tools import set_telegram_context, clear_telegram_context
            from lethe.proxy_bot import ProxyBot
            proxy = ProxyBot(api_module._proactive_queue)
            set_telegram_context(proxy, 0)
            try:
                response = await agent.chat(synthetic_message)
                if response and response.strip():
                    await api_module.send_proactive(response)
            except Exception as e:
                logger.exception("run_cortex_turn failed: %s", e)
            finally:
                clear_telegram_context()

        actor_system.set_callbacks(
            send_to_user=heartbeat_send,
            get_reminders=get_active_reminders,
            run_cortex_turn=run_cortex_turn,
        )

    # Set up shutdown handling
    shutdown_event = asyncio.Event()

    def signal_handler():
        logger.info("Received shutdown signal...")
        shutdown_event.set()

    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, signal_handler)

    # Start uvicorn
    import uvicorn
    config = uvicorn.Config(
        api_module.app,
        host=os.environ.get("LETHE_API_HOST", "127.0.0.1"),
        port=port,
        log_level="info",
    )
    server = uvicorn.Server(config)

    console.print(f"[green]API server starting on port {port}[/green]")

    heartbeat_task = asyncio.create_task(heartbeat.start())
    server_task = asyncio.create_task(server.serve())

    try:
        await shutdown_event.wait()
    except asyncio.CancelledError:
        pass
    finally:
        console.print("\n[yellow]Shutting down...[/yellow]")
        server.should_exit = True
        try:
            async with asyncio.timeout(5):
                if actor_system:
                    await actor_system.shutdown()
                await heartbeat.stop()
                await agent.close()
        except asyncio.TimeoutError:
            logger.warning("Shutdown timed out, forcing exit")
            os._exit(0)

        heartbeat_task.cancel()
        server_task.cancel()
        for t in (heartbeat_task, server_task):
            try:
                await t
            except asyncio.CancelledError:
                pass

        console.print("[green]Shutdown complete.[/green]")


def main():
    """CLI entry point."""
    import argparse

    parser = argparse.ArgumentParser(description="Lethe - Autonomous AI Assistant")
    parser.add_argument("-v", "--verbose", action="store_true", help="Enable verbose logging")
    parser.add_argument("--api", action="store_true", help="Run in HTTP API mode (for gateway)")
    parser.add_argument("--api-port", type=int, default=8080, help="HTTP API port (default: 8080)")

    subparsers = parser.add_subparsers(dest="command")
    oauth_parser = subparsers.add_parser(
        "oauth-login",
        help="Login with OAuth (anthropic or openai)",
    )
    oauth_parser.add_argument(
        "provider",
        nargs="?",
        choices=["anthropic", "openai"],
        default="anthropic",
        help="OAuth provider (default: anthropic)",
    )

    args = parser.parse_args()

    # Handle subcommands
    if args.command == "oauth-login":
        from lethe.tools.oauth_login import run_oauth_login
        run_oauth_login(args.provider)
        return

    setup_logging(verbose=args.verbose)

    # Check for API mode (CLI flag or env var)
    api_mode = args.api or os.environ.get("LETHE_MODE", "").lower() == "api"

    try:
        if api_mode:
            asyncio.run(run_api(port=args.api_port))
        else:
            asyncio.run(run())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
