"""Tools for the Lethe agent.

Tools are just Python functions. Schemas are auto-generated from type hints and docstrings.
"""

import inspect
import re
from typing import Callable, Any, get_type_hints, Optional

# Import all tool functions
from lethe.tools.cli import (
    bash,
    bash_output,
    get_terminal_screen,
    send_terminal_input,
    kill_bash,
    get_environment_info,
    check_command_exists,
)

from lethe.tools.filesystem import (
    read_file,
    write_file,
    edit_file,
    list_directory,
    glob_search,
    grep_search,
)

from lethe.tools.web_search import (
    web_search,
    fetch_webpage,
    is_available as web_search_available,
)

from lethe.tools.notes import (
    note_search,
    note_create,
    note_list,
    set_store as set_note_store,
)

from lethe.tools.browser_agent import (
    browser_open_async as browser_open,
    browser_snapshot_async as browser_snapshot,
    browser_click_async as browser_click,
    browser_fill_async as browser_fill,
)

# Internal telegram context (not tools - used by main.py)
from lethe.tools.telegram_tools import (
    set_telegram_context,
    set_last_message_id,
    clear_telegram_context,
)

# Agent tools
from lethe.tools.telegram_tools import (
    telegram_react_async as telegram_react,
    telegram_send_message_async as telegram_send_message,
    telegram_send_file_async as telegram_send_file,
)


def _python_type_to_json(py_type) -> str:
    """Convert Python type to JSON schema type."""
    if py_type is None or py_type == type(None):
        return "string"
    
    type_name = getattr(py_type, "__name__", str(py_type))
    
    mapping = {
        "str": "string",
        "int": "integer",
        "float": "number",
        "bool": "boolean",
        "list": "array",
        "dict": "object",
    }
    return mapping.get(type_name, "string")


def _parse_docstring(docstring: str) -> tuple[str, dict[str, str]]:
    """Parse Google-style docstring into description and param descriptions."""
    if not docstring:
        return "", {}
    
    lines = docstring.strip().split("\n")
    description_lines = []
    param_descriptions = {}
    
    in_args = False
    current_param = None
    
    for line in lines:
        stripped = line.strip()
        
        if stripped.lower().startswith("args:"):
            in_args = True
            continue
        elif stripped.lower().startswith("returns:"):
            in_args = False
            continue
        
        if in_args:
            # Check for param line: "param_name: description" or "param_name (type): description"
            match = re.match(r"(\w+)(?:\s*\([^)]*\))?\s*:\s*(.+)", stripped)
            if match:
                current_param = match.group(1)
                param_descriptions[current_param] = match.group(2)
            elif current_param and stripped:
                # Continuation of previous param description
                param_descriptions[current_param] += " " + stripped
        elif not in_args and stripped:
            description_lines.append(stripped)
    
    return " ".join(description_lines), param_descriptions


def function_to_schema(func: Callable) -> dict:
    """Generate OpenAI function schema from a Python function."""
    sig = inspect.signature(func)
    
    try:
        hints = get_type_hints(func)
    except Exception:
        hints = {}
    
    description, param_docs = _parse_docstring(func.__doc__ or "")
    
    properties = {}
    required = []
    
    for name, param in sig.parameters.items():
        if name in ("self", "cls"):
            continue
        
        param_type = hints.get(name, str)
        json_type = _python_type_to_json(param_type)
        
        prop = {"type": json_type}
        if name in param_docs:
            prop["description"] = param_docs[name]
        
        properties[name] = prop
        
        # Required if no default value
        if param.default is inspect.Parameter.empty:
            required.append(name)
    
    return {
        "name": func.__name__,
        "description": description or f"Execute {func.__name__}",
        "parameters": {
            "type": "object",
            "properties": properties,
            "required": required,
        }
    }


# Core tools — always registered with full schemas (~12 tools, under Gemma 4's recommended limit)
_CORE_TOOLS = [
    # CLI & files
    (bash, None),
    (read_file, None),
    (write_file, None),
    (edit_file, None),
    (list_directory, None),
    (grep_search, None),
    # Web
    (web_search, None),
    (fetch_webpage, None),
    # Notes
    (note_search, None),
    (note_create, None),
    # Telegram
    (telegram_send_message, "telegram_send_message"),
    (telegram_send_file, "telegram_send_file"),
    (telegram_react, "telegram_react"),
]

# Extended tools — available on demand via request_tool()
_EXTENDED_TOOLS = {
    "browser_open": (browser_open, "browser_open"),
    "browser_snapshot": (browser_snapshot, "browser_snapshot"),
    "browser_click": (browser_click, "browser_click"),
    "browser_fill": (browser_fill, "browser_fill"),
    "note_list": (note_list, None),
}

# LLM client reference — set by agent at init time for dynamic tool registration
_llm_client = None


def set_llm_client(client):
    """Set the LLM client for dynamic tool registration."""
    global _llm_client
    _llm_client = client


def _is_tool(func):
    func._is_tool = True
    return func


@_is_tool
def request_tool(name: str) -> str:
    """Request an extended tool to be made available for this conversation.

    Core tools (bash, read/write/edit_file, list_directory, grep_search,
    web_search, note_search, note_create, telegram_send_message,
    telegram_send_file, telegram_react, spawn_actor, send_message,
    discover_actors, memory_read, memory_update, request_tool) are always available.

    Extended tools that can be requested:
    - browser_open, browser_snapshot, browser_click, browser_fill — Browser automation
    - fetch_webpage — Fetch full page content
    - archival_search, archival_insert, conversation_search — Legacy memory search
    - memory_append — Append to memory blocks
    - kill_actor, ping_actor, terminate — Actor management
    - note_list — List all notes
    - view_image — View image in LLM context

    Args:
        name: Name of the tool to request

    Returns:
        Confirmation that the tool is now available, or error if not found
    """
    if name not in _EXTENDED_TOOLS:
        available = ", ".join(sorted(_EXTENDED_TOOLS.keys()))
        return f"Unknown tool: {name}. Available extended tools: {available}"

    if not _llm_client:
        return f"Error: tool registration not available"

    func, name_override = _EXTENDED_TOOLS[name]
    schema = function_to_schema(func)
    if name_override:
        schema["name"] = name_override

    # Check if already registered
    if _llm_client.get_tool(name):
        return f"Tool '{name}' is already available. You can use it now."

    _llm_client.add_tool(func, schema)
    desc = schema.get("description", "")[:100]
    return f"Tool '{name}' is now available. {desc}"


def get_core_tools() -> list[tuple[Callable, dict]]:
    """Get core tools (always registered) as (function, schema) tuples."""
    result = []
    for func, name_override in _CORE_TOOLS:
        schema = function_to_schema(func)
        if name_override:
            schema["name"] = name_override
        result.append((func, schema))
    # Add request_tool itself
    result.append((request_tool, function_to_schema(request_tool)))
    return result


def get_all_tools() -> list[tuple[Callable, dict]]:
    """Get ALL tools (core + extended) as (function, schema) tuples.

    Used for subagents or contexts where tool count doesn't matter.
    """
    tools = _CORE_TOOLS + list(_EXTENDED_TOOLS.values())
    result = []
    for func, name_override in tools:
        schema = function_to_schema(func)
        if name_override:
            schema["name"] = name_override
        result.append((func, schema))
    result.append((request_tool, function_to_schema(request_tool)))
    return result


def get_tool_by_name(name: str) -> Optional[Callable]:
    """Get a tool function by name."""
    tools = {
        "bash": bash,
        "bash_output": bash_output,
        "kill_bash": kill_bash,
        "read_file": read_file,
        "write_file": write_file,
        "edit_file": edit_file,
        "list_directory": list_directory,
        "glob_search": glob_search,
        "grep_search": grep_search,
        # Browser
        "browser_open": browser_open,
        "browser_snapshot": browser_snapshot,
        "browser_click": browser_click,
        "browser_fill": browser_fill,
        # Web search
        "web_search": web_search,
        "fetch_webpage": fetch_webpage,
        # Telegram
        "telegram_react": telegram_react,
        "telegram_send_message": telegram_send_message,
        "telegram_send_file": telegram_send_file,
    }
    return tools.get(name)


__all__ = [
    "function_to_schema",
    "get_all_tools",
    "get_tool_by_name",
    # CLI
    "bash",
    "bash_output",
    "get_terminal_screen",
    "send_terminal_input",
    "kill_bash",
    "get_environment_info",
    "check_command_exists",
    # Files
    "read_file",
    "write_file",
    "edit_file",
    "list_directory",
    "glob_search",
    "grep_search",
    # Browser
    "browser_open",
    "browser_snapshot",
    "browser_click",
    "browser_fill",
    # Web search
    "web_search",
    "fetch_webpage",
    "web_search_available",
    # Telegram (tools)
    "telegram_react",
    "telegram_send_message", 
    "telegram_send_file",
    # Telegram (internal - for main.py)
    "set_telegram_context",
    "set_last_message_id",
    "clear_telegram_context",
]
