"""Tests for Lethe tools."""

import asyncio
import json
import os
import tempfile
from pathlib import Path

import pytest


# ============================================================================
# Filesystem Tools Tests
# ============================================================================

class TestReadFile:
    """Tests for read_file tool."""
    
    def test_read_existing_file(self):
        """Should read an existing file with line numbers."""
        from lethe.tools.filesystem import read_file
        
        with tempfile.NamedTemporaryFile(mode='w', suffix='.txt', delete=False) as f:
            f.write("line 1\nline 2\nline 3\n")
            f.flush()
            
            result = read_file(f.name)
            
            assert "line 1" in result
            assert "line 2" in result
            assert "line 3" in result
            # Check line numbers
            assert "1\t" in result or "     1\t" in result
            
            os.unlink(f.name)
    
    def test_read_nonexistent_file(self):
        """Should return error for nonexistent file."""
        from lethe.tools.filesystem import read_file
        
        result = read_file("/nonexistent/path/file.txt")
        assert "Error" in result
        assert "not found" in result.lower()
    
    def test_read_with_offset_and_limit(self):
        """Should respect offset and limit parameters."""
        from lethe.tools.filesystem import read_file
        
        with tempfile.NamedTemporaryFile(mode='w', suffix='.txt', delete=False) as f:
            for i in range(10):
                f.write(f"line {i}\n")
            f.flush()
            
            # offset is 1-indexed, so offset=3 starts at line 3 (which contains "line 2")
            result = read_file(f.name, offset=3, limit=3)
            
            # Should have lines 3-5 (1-indexed offset of 3, limit of 3)
            assert "line 2" in result
            assert "line 3" in result
            assert "line 4" in result
            # Should not have line 0, 1, or 5+
            assert "line 0" not in result
            assert "line 1" not in result
            assert "line 5" not in result
            
            os.unlink(f.name)
    
    def test_read_directory_returns_error(self):
        """Should return error when trying to read a directory."""
        from lethe.tools.filesystem import read_file
        
        with tempfile.TemporaryDirectory() as tmpdir:
            result = read_file(tmpdir)
            assert "Error" in result
            assert "Not a file" in result


class TestWriteFile:
    """Tests for write_file tool."""
    
    def test_write_new_file(self):
        """Should create a new file with content."""
        from lethe.tools.filesystem import write_file, read_file
        
        with tempfile.TemporaryDirectory() as tmpdir:
            filepath = os.path.join(tmpdir, "test.txt")
            
            result = write_file(filepath, "Hello, World!")
            
            assert "Successfully" in result
            assert os.path.exists(filepath)
            
            # Verify content
            read_result = read_file(filepath)
            assert "Hello, World!" in read_result
    
    def test_write_creates_parent_directories(self):
        """Should create parent directories if they don't exist."""
        from lethe.tools.filesystem import write_file
        
        with tempfile.TemporaryDirectory() as tmpdir:
            filepath = os.path.join(tmpdir, "nested", "dir", "test.txt")
            
            result = write_file(filepath, "content")
            
            assert "Successfully" in result
            assert os.path.exists(filepath)
    
    def test_write_overwrites_existing(self):
        """Should overwrite existing file."""
        from lethe.tools.filesystem import write_file, read_file
        
        with tempfile.NamedTemporaryFile(mode='w', suffix='.txt', delete=False) as f:
            f.write("original content")
            f.flush()
            
            write_file(f.name, "new content")
            
            result = read_file(f.name)
            assert "new content" in result
            assert "original content" not in result
            
            os.unlink(f.name)


class TestEditFile:
    """Tests for edit_file tool."""
    
    def test_edit_single_occurrence(self):
        """Should replace single occurrence."""
        from lethe.tools.filesystem import edit_file, read_file
        
        with tempfile.NamedTemporaryFile(mode='w', suffix='.txt', delete=False) as f:
            f.write("Hello, World!")
            f.flush()
            
            result = edit_file(f.name, "World", "Python")
            
            assert "Successfully" in result
            
            content = read_file(f.name)
            assert "Python" in content
            assert "World" not in content
            
            os.unlink(f.name)
    
    def test_edit_fails_on_multiple_occurrences(self):
        """Should fail when string appears multiple times without replace_all."""
        from lethe.tools.filesystem import edit_file
        
        with tempfile.NamedTemporaryFile(mode='w', suffix='.txt', delete=False) as f:
            f.write("foo bar foo")
            f.flush()
            
            result = edit_file(f.name, "foo", "baz")
            
            assert "Error" in result
            assert "multiple times" in result.lower()
            
            os.unlink(f.name)
    
    def test_edit_replace_all(self):
        """Should replace all occurrences when replace_all=True."""
        from lethe.tools.filesystem import edit_file, read_file
        
        with tempfile.NamedTemporaryFile(mode='w', suffix='.txt', delete=False) as f:
            f.write("foo bar foo")
            f.flush()
            
            result = edit_file(f.name, "foo", "baz", replace_all=True)
            
            assert "Successfully" in result
            assert "2 occurrence" in result
            
            content = read_file(f.name)
            assert "baz bar baz" in content
            
            os.unlink(f.name)
    
    def test_edit_string_not_found(self):
        """Should return error when string not found."""
        from lethe.tools.filesystem import edit_file
        
        with tempfile.NamedTemporaryFile(mode='w', suffix='.txt', delete=False) as f:
            f.write("Hello, World!")
            f.flush()
            
            result = edit_file(f.name, "nonexistent", "replacement")
            
            assert "Error" in result
            assert "not found" in result.lower()
            
            os.unlink(f.name)


class TestListDirectory:
    """Tests for list_directory tool."""
    
    def test_list_directory(self):
        """Should list directory contents."""
        from lethe.tools.filesystem import list_directory
        
        with tempfile.TemporaryDirectory() as tmpdir:
            # Create some files and dirs
            Path(tmpdir, "file1.txt").touch()
            Path(tmpdir, "file2.txt").touch()
            Path(tmpdir, "subdir").mkdir()
            
            result = list_directory(tmpdir)
            
            assert "file1.txt" in result
            assert "file2.txt" in result
            assert "subdir" in result
            assert "[DIR]" in result
            assert "[FILE]" in result
    
    def test_list_hidden_files(self):
        """Should respect show_hidden parameter."""
        from lethe.tools.filesystem import list_directory
        
        with tempfile.TemporaryDirectory() as tmpdir:
            Path(tmpdir, ".hidden").touch()
            Path(tmpdir, "visible").touch()
            
            # Without hidden
            result = list_directory(tmpdir, show_hidden=False)
            assert ".hidden" not in result
            assert "visible" in result
            
            # With hidden
            result = list_directory(tmpdir, show_hidden=True)
            assert ".hidden" in result
            assert "visible" in result
    
    def test_list_nonexistent_directory(self):
        """Should return error for nonexistent directory."""
        from lethe.tools.filesystem import list_directory
        
        result = list_directory("/nonexistent/path")
        assert "Error" in result


class TestGlobSearch:
    """Tests for glob_search tool."""
    
    def test_glob_pattern(self):
        """Should find files matching pattern."""
        from lethe.tools.filesystem import glob_search
        
        with tempfile.TemporaryDirectory() as tmpdir:
            Path(tmpdir, "file1.py").touch()
            Path(tmpdir, "file2.py").touch()
            Path(tmpdir, "file3.txt").touch()
            
            result = glob_search("*.py", tmpdir)
            
            assert "file1.py" in result
            assert "file2.py" in result
            assert "file3.txt" not in result
            assert "Found 2 files" in result
    
    def test_glob_recursive(self):
        """Should support recursive patterns."""
        from lethe.tools.filesystem import glob_search
        
        with tempfile.TemporaryDirectory() as tmpdir:
            subdir = Path(tmpdir, "subdir")
            subdir.mkdir()
            Path(tmpdir, "root.py").touch()
            Path(subdir, "nested.py").touch()
            
            result = glob_search("**/*.py", tmpdir)
            
            assert "root.py" in result
            assert "nested.py" in result
    
    def test_glob_no_matches(self):
        """Should report when no files match."""
        from lethe.tools.filesystem import glob_search
        
        with tempfile.TemporaryDirectory() as tmpdir:
            result = glob_search("*.xyz", tmpdir)
            assert "No files matching" in result


class TestGrepSearch:
    """Tests for grep_search tool."""
    
    def test_grep_pattern(self):
        """Should find lines matching pattern."""
        from lethe.tools.filesystem import grep_search
        
        with tempfile.TemporaryDirectory() as tmpdir:
            Path(tmpdir, "test.txt").write_text("hello world\nfoo bar\nhello again\n")
            
            result = grep_search("hello", tmpdir)
            
            assert "hello world" in result
            assert "hello again" in result
            assert "foo bar" not in result
            assert "Found 2 matches" in result
    
    def test_grep_regex(self):
        """Should support regex patterns."""
        from lethe.tools.filesystem import grep_search
        
        with tempfile.TemporaryDirectory() as tmpdir:
            Path(tmpdir, "test.txt").write_text("test123\ntest456\nother\n")
            
            result = grep_search(r"test\d+", tmpdir)
            
            assert "test123" in result
            assert "test456" in result
            assert "other" not in result
    
    def test_grep_file_pattern(self):
        """Should filter by file pattern."""
        from lethe.tools.filesystem import grep_search
        
        with tempfile.TemporaryDirectory() as tmpdir:
            Path(tmpdir, "code.py").write_text("hello\n")
            Path(tmpdir, "text.txt").write_text("hello\n")
            
            result = grep_search("hello", tmpdir, file_pattern="*.py")
            
            assert "code.py" in result
            assert "text.txt" not in result


# ============================================================================
# CLI Tools Tests
# ============================================================================

class TestBash:
    """Tests for bash tool."""
    
    def test_simple_command(self):
        """Should execute simple command."""
        from lethe.tools.cli import bash
        
        result = bash("echo hello")
        assert "hello" in result
    
    def test_command_with_exit_code(self):
        """Should report non-zero exit codes."""
        from lethe.tools.cli import bash
        
        result = bash("exit 1")
        assert "Exit code: 1" in result
    
    def test_command_timeout(self):
        """Should timeout long-running commands."""
        from lethe.tools.cli import bash
        
        result = bash("sleep 10", timeout=1)
        assert "timed out" in result.lower()
    
    def test_background_command(self):
        """Should run command in background."""
        from lethe.tools.cli import bash
        
        result = bash("sleep 1", run_in_background=True)
        assert "background" in result.lower()
        assert "bash_" in result
    
    def test_list_background_processes(self):
        """Should list background processes with /bg."""
        from lethe.tools.cli import bash
        
        # Start a background process
        bash("sleep 5", run_in_background=True)
        
        # List processes
        result = bash("/bg")
        assert "sleep" in result or "no background processes" in result.lower()
    
    def test_capture_stderr(self):
        """Should capture stderr output."""
        from lethe.tools.cli import bash
        
        result = bash("echo error >&2")
        assert "error" in result


class TestBashOutput:
    """Tests for bash_output tool."""
    
    def test_get_output(self):
        """Should retrieve output from background process."""
        from lethe.tools.cli import bash, bash_output
        import time
        
        # Start background command
        start_result = bash("echo hello; sleep 0.5; echo world", run_in_background=True)
        
        # Extract shell_id
        shell_id = start_result.split(":")[-1].strip()
        
        # Wait a bit for output
        time.sleep(1)
        
        result = bash_output(shell_id)
        assert "hello" in result or "no output" in result.lower()
    
    def test_nonexistent_shell(self):
        """Should return error for nonexistent shell."""
        from lethe.tools.cli import bash_output
        
        result = bash_output("nonexistent_shell")
        assert "No background process" in result


class TestKillBash:
    """Tests for kill_bash tool."""
    
    def test_kill_process(self):
        """Should kill a background process."""
        from lethe.tools.cli import bash, kill_bash
        
        # Start a long-running background command
        start_result = bash("sleep 100", run_in_background=True)
        shell_id = start_result.split(":")[-1].strip()
        
        # Kill it
        result = kill_bash(shell_id)
        assert "Killed" in result
    
    def test_kill_nonexistent(self):
        """Should return error for nonexistent shell."""
        from lethe.tools.cli import kill_bash
        
        result = kill_bash("nonexistent_shell")
        assert "No background process" in result


class TestGetEnvironmentInfo:
    """Tests for get_environment_info tool."""
    
    def test_get_info(self):
        """Should return environment information."""
        from lethe.tools.cli import get_environment_info
        
        result = get_environment_info()
        
        assert "user:" in result.lower()
        assert "pwd:" in result.lower()
        assert "home:" in result.lower()


class TestCheckCommandExists:
    """Tests for check_command_exists tool."""
    
    def test_existing_command(self):
        """Should find existing command."""
        from lethe.tools.cli import check_command_exists
        
        result = check_command_exists("ls")
        assert "available" in result.lower()
    
    def test_nonexistent_command(self):
        """Should report when command not found."""
        from lethe.tools.cli import check_command_exists
        
        result = check_command_exists("nonexistent_command_xyz")
        assert "not found" in result.lower()


# ============================================================================
# Schema Generation Tests
# ============================================================================

class TestFunctionToSchema:
    """Tests for function_to_schema."""
    
    def test_basic_function(self):
        """Should generate schema from basic function."""
        from lethe.tools import function_to_schema
        
        def test_func(name: str, count: int = 1) -> str:
            """Test function description.
            
            Args:
                name: The name parameter
                count: The count parameter
            """
            return ""
        
        schema = function_to_schema(test_func)
        
        assert schema["name"] == "test_func"
        assert "Test function description" in schema["description"]
        assert schema["parameters"]["properties"]["name"]["type"] == "string"
        assert schema["parameters"]["properties"]["count"]["type"] == "integer"
        assert "name" in schema["parameters"]["required"]
        assert "count" not in schema["parameters"]["required"]  # Has default
    
    def test_parameter_descriptions(self):
        """Should extract parameter descriptions from docstring."""
        from lethe.tools import function_to_schema
        
        def func(path: str) -> str:
            """Function.
            
            Args:
                path: The file path to read
            """
            return ""
        
        schema = function_to_schema(func)
        assert "file path" in schema["parameters"]["properties"]["path"]["description"].lower()


class TestGetAllTools:
    """Tests for get_all_tools."""
    
    def test_returns_tools(self):
        """Should return list of tool tuples."""
        from lethe.tools import get_all_tools
        
        tools = get_all_tools()
        
        assert isinstance(tools, list)
        assert len(tools) > 0
        
        # Each tool should be (callable, schema dict)
        for func, schema in tools:
            assert callable(func)
            assert isinstance(schema, dict)
            assert "name" in schema
            assert "parameters" in schema
    
    def test_tool_names(self):
        """Should include expected tool names."""
        from lethe.tools import get_all_tools
        
        tools = get_all_tools()
        names = [schema["name"] for _, schema in tools]
        
        # Check some expected tools
        assert "bash" in names
        assert "read_file" in names
        assert "write_file" in names
        assert "list_directory" in names
        assert "browser_open" in names


class TestGetToolByName:
    """Tests for get_tool_by_name."""
    
    def test_get_existing_tool(self):
        """Should return tool function by name."""
        from lethe.tools import get_tool_by_name
        
        tool = get_tool_by_name("bash")
        assert tool is not None
        assert callable(tool)
    
    def test_get_nonexistent_tool(self):
        """Should return None for nonexistent tool."""
        from lethe.tools import get_tool_by_name
        
        tool = get_tool_by_name("nonexistent_tool")
        assert tool is None


# ============================================================================
# Browser Tools Tests (Async)
# ============================================================================

def _check_agent_browser():
    """Check if agent-browser is available."""
    import shutil
    if not shutil.which("agent-browser"):
        pytest.skip("agent-browser not installed")


class TestBrowserToolsAsync:
    """Tests for browser tools (async).
    
    These tests require agent-browser CLI to be installed.
    Run: npm install -g agent-browser
    """
    
    @pytest.mark.asyncio
    async def test_browser_open_valid_url(self):
        """Should open a valid URL."""
        _check_agent_browser()
        from lethe.tools import browser_open
        import json
        
        result = await browser_open("https://example.com")
        
        # Should return JSON
        data = json.loads(result)
        assert data.get("status") == "OK" or "error" in data.get("status", "").lower()
    
    @pytest.mark.asyncio
    async def test_browser_open_invalid_url(self):
        """Should handle invalid URL gracefully."""
        _check_agent_browser()
        from lethe.tools import browser_open
        import json
        
        result = await browser_open("not-a-valid-url")
        data = json.loads(result)
        
        # Should return error status, not crash
        assert isinstance(data, dict)
    
    @pytest.mark.asyncio
    async def test_browser_snapshot(self):
        """Should get accessibility snapshot."""
        _check_agent_browser()
        from lethe.tools import browser_open, browser_snapshot
        import json
        
        # First open a page
        await browser_open("https://example.com")
        
        # Then get snapshot
        result = await browser_snapshot()
        
        # Should return JSON or accessibility tree
        assert isinstance(result, (str, dict))
        if isinstance(result, str):
            # Try to parse as JSON
            try:
                data = json.loads(result)
                assert isinstance(data, dict)
            except json.JSONDecodeError:
                # Might be raw accessibility tree text
                assert len(result) > 0
    
    @pytest.mark.asyncio
    async def test_browser_click_invalid_ref(self):
        """Should handle invalid ref gracefully."""
        _check_agent_browser()
        from lethe.tools import browser_click
        import json
        
        result = await browser_click("@invalid_ref_12345")
        
        # Should return error, not crash
        if isinstance(result, str):
            try:
                data = json.loads(result)
                assert "error" in str(data).lower() or "not found" in str(data).lower()
            except json.JSONDecodeError:
                # Raw error message
                assert len(result) > 0
    
    @pytest.mark.asyncio
    async def test_browser_fill_invalid_ref(self):
        """Should handle invalid ref gracefully."""
        _check_agent_browser()
        from lethe.tools import browser_fill
        import json
        
        result = await browser_fill("@invalid_ref_12345", "test text")
        
        # Should return error, not crash
        assert isinstance(result, (str, dict))


# ============================================================================
# Web Search Tests
# ============================================================================

def _check_exa_api():
    """Check if Exa API is available."""
    from lethe.tools import web_search_available
    if not web_search_available():
        pytest.skip("EXA_API_KEY not configured")


class TestWebSearch:
    """Tests for web search tools."""
    
    def test_web_search_basic(self):
        """Should search the web and return synthesized results."""
        _check_exa_api()
        from lethe.tools import web_search

        result = web_search("Python programming language")
        assert isinstance(result, str)
        assert len(result) > 50
        assert "Raw results:" in result

    def test_web_search_with_category(self):
        """Should filter by category."""
        _check_exa_api()
        from lethe.tools import web_search

        result = web_search("machine learning", num_results=5, category="research paper")
        assert isinstance(result, str)
        assert len(result) > 50
    
    def test_web_search_no_api_key(self):
        """Should return error when API key not set."""
        import json
        import os
        import sys
        
        # Save original state
        original_key = os.environ.get("EXA_API_KEY")
        
        # Remove key and reload module
        if "EXA_API_KEY" in os.environ:
            del os.environ["EXA_API_KEY"]
        
        # Remove cached module to force reload
        if "lethe.tools.web_search" in sys.modules:
            del sys.modules["lethe.tools.web_search"]
        
        # Import fresh
        from lethe.tools.web_search import web_search as ws_func
        
        result = ws_func("test query")
        data = json.loads(result)
        
        assert data.get("status") == "error"
        assert "EXA_API_KEY" in data.get("message", "")
        
        # Restore key
        if original_key:
            os.environ["EXA_API_KEY"] = original_key
        # Clear module so future tests get correct state
        if "lethe.tools.web_search" in sys.modules:
            del sys.modules["lethe.tools.web_search"]

    def test_web_search_reads_api_key_at_runtime(self, monkeypatch):
        """Should pick up EXA_API_KEY changes without reloading the module."""
        import json
        import sys
        import types
        from lethe.tools.web_search import web_search

        captured = {}

        class DummyResponse:
            text = ""

            def raise_for_status(self):
                return None

            def json(self):
                return {"results": []}

        class DummyClient:
            def __init__(self, timeout=30.0):
                self.timeout = timeout

            def __enter__(self):
                return self

            def __exit__(self, exc_type, exc, tb):
                return False

            def post(self, url, headers=None, json=None):
                captured["url"] = url
                captured["api_key"] = (headers or {}).get("x-api-key")
                return DummyResponse()

        dummy_httpx = types.SimpleNamespace(Client=DummyClient, HTTPStatusError=Exception)
        monkeypatch.setitem(sys.modules, "httpx", dummy_httpx)
        monkeypatch.setenv("EXA_API_KEY", "runtime-test-key")

        result = web_search("runtime key test")
        data = json.loads(result)

        assert data.get("status") == "OK"
        assert captured.get("api_key") == "runtime-test-key"
    
    def test_fetch_webpage(self):
        """Should fetch webpage content."""
        _check_exa_api()
        from lethe.tools import fetch_webpage
        import json
        
        result = fetch_webpage("https://example.com")
        data = json.loads(result)
        
        # Might succeed or fail depending on Exa's handling
        assert isinstance(data, dict)
        assert "status" in data


class DummyTelegramBot:
    def __init__(self):
        self.calls = []

    async def set_message_reaction(self, chat_id, message_id, reaction, **kwargs):
        self.calls.append((chat_id, message_id, reaction))


class TestTelegramReactTool:
    def test_telegram_react_is_core_tool(self):
        from lethe.tools import get_core_tools

        names = [schema["name"] for _, schema in get_core_tools()]
        assert "telegram_react" in names

    def test_telegram_react_schema_includes_optional_message_id(self):
        from lethe.tools import function_to_schema
        from lethe.tools.telegram_tools import telegram_react_async

        schema = function_to_schema(telegram_react_async)
        assert schema["parameters"]["properties"]["message_id"]["type"] == "integer"
        assert "message_id" not in schema["parameters"]["required"]

    def test_request_tool_does_not_advertise_telegram_react(self):
        from lethe.tools import request_tool

        assert "Unknown tool: telegram_react" in request_tool("telegram_react")

    @pytest.mark.asyncio
    async def test_telegram_react_uses_explicit_message_id(self):
        from lethe.tools.telegram_tools import (
            clear_telegram_context,
            set_last_message_id,
            set_telegram_context,
            telegram_react_async,
        )

        bot = DummyTelegramBot()
        set_telegram_context(bot, 99)
        set_last_message_id(42)

        try:
            payload = json.loads(await telegram_react_async("🔥", message_id=77))
        finally:
            clear_telegram_context()

        assert payload["emoji"] == "🔥"
        assert payload["message_id"] == 77
        assert bot.calls[0][0] == 99
        assert bot.calls[0][1] == 77
        assert getattr(bot.calls[0][2][0], "emoji", None) == "🔥"

    @pytest.mark.asyncio
    async def test_telegram_react_falls_back_to_last_message_id(self):
        from lethe.tools.telegram_tools import (
            clear_telegram_context,
            set_last_message_id,
            set_telegram_context,
            telegram_react_async,
        )

        bot = DummyTelegramBot()
        set_telegram_context(bot, 99)
        set_last_message_id(42)

        try:
            payload = json.loads(await telegram_react_async("👍"))
        finally:
            clear_telegram_context()

        assert payload["message_id"] == 42
        assert bot.calls[0][1] == 42

    @pytest.mark.asyncio
    async def test_telegram_react_queues_when_guard_active(self):
        from lethe.telegram_turn_guard import clear_telegram_turn_guard, start_telegram_turn_guard
        from lethe.tools.telegram_tools import (
            clear_telegram_context,
            set_last_message_id,
            set_telegram_context,
            telegram_react_async,
        )

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


# ============================================================================
# Integration Tests
# ============================================================================

class TestToolIntegration:
    """Integration tests for tools working together."""
    
    def test_write_and_read(self):
        """Should write file then read it back."""
        from lethe.tools.filesystem import write_file, read_file
        
        with tempfile.TemporaryDirectory() as tmpdir:
            filepath = os.path.join(tmpdir, "test.txt")
            content = "Line 1\nLine 2\nLine 3"
            
            write_file(filepath, content)
            result = read_file(filepath)
            
            assert "Line 1" in result
            assert "Line 2" in result
            assert "Line 3" in result
    
    def test_write_edit_read(self):
        """Should write, edit, then read file."""
        from lethe.tools.filesystem import write_file, edit_file, read_file
        
        with tempfile.TemporaryDirectory() as tmpdir:
            filepath = os.path.join(tmpdir, "test.txt")
            
            write_file(filepath, "Hello, World!")
            edit_file(filepath, "World", "Python")
            result = read_file(filepath)
            
            assert "Hello, Python!" in result
    
    def test_bash_and_file_operations(self):
        """Should use bash to verify file operations."""
        from lethe.tools.filesystem import write_file
        from lethe.tools.cli import bash
        
        with tempfile.TemporaryDirectory() as tmpdir:
            filepath = os.path.join(tmpdir, "test.txt")
            
            write_file(filepath, "test content")
            
            # Use bash to verify
            result = bash(f"cat {filepath}")
            assert "test content" in result


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
