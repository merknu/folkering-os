"""Context Manager — 3-Tier Compaction for Agentic Sessions

Manages the conversation context window to prevent degradation.
Implements the architectural blueprint's compaction strategy:

Tier 1 (MicroCompact): Per-turn trimming, zero API calls
  - Strip repeated tool outputs
  - Collapse duplicate error messages
  - Truncate large code blocks

Tier 2 (AutoCompact): At 75% context usage, summarize with LLM
  - Reserve 20% buffer for the summarization itself
  - Circuit breaker: max 3 retries

Tier 3 (FullCompact): At 95%, nuclear reset
  - Keep only: system prompt + last user message + recent tool results
"""

# Token estimation: ~4 chars per token (rough but consistent)
CHARS_PER_TOKEN = 4


class ContextManager:
    """Manages conversation history with automatic compaction."""

    def __init__(self, max_tokens: int = 8192):
        self.max_tokens = max_tokens
        self.messages: list[dict] = []
        self.system_prompt: str = ""
        self.compact_count: int = 0

    def set_system_prompt(self, prompt: str):
        """Set the static system prompt (never compacted)."""
        self.system_prompt = prompt

    def add_message(self, role: str, content: str):
        """Add a message and run MicroCompact."""
        self.messages.append({"role": role, "content": content})
        self._micro_compact()

    def get_messages(self) -> list[dict]:
        """Get all messages including system prompt."""
        msgs = [{"role": "system", "content": self.system_prompt}]
        msgs.extend(self.messages)
        return msgs

    def get_prompt_text(self) -> str:
        """Get flattened prompt text for token counting."""
        parts = [self.system_prompt]
        for m in self.messages:
            parts.append(f"[{m['role']}]\n{m['content']}")
        return "\n\n".join(parts)

    def token_count(self) -> int:
        """Estimate token count."""
        return len(self.get_prompt_text()) // CHARS_PER_TOKEN

    def usage_pct(self) -> float:
        """Context window usage as percentage."""
        return self.token_count() / self.max_tokens * 100

    # ── Tier 1: MicroCompact ─────────────────────────────────────────

    def _micro_compact(self):
        """Per-turn trimming: no API calls, purely mechanical."""
        for i, msg in enumerate(self.messages):
            content = msg["content"]

            # Truncate very long tool results (keep first + last 500 chars)
            if msg["role"] == "tool_result" and len(content) > 2000:
                self.messages[i]["content"] = (
                    content[:800]
                    + "\n...[truncated]...\n"
                    + content[-500:]
                )

            # Collapse repeated errors
            if i > 0 and msg["role"] == "tool_result":
                prev = self.messages[i - 1]
                if prev["role"] == "tool_result" and prev["content"] == content:
                    self.messages[i]["content"] = "[same as previous]"

    # ── Tier 2: AutoCompact ──────────────────────────────────────────

    def needs_auto_compact(self) -> bool:
        """Check if we're approaching the context limit."""
        return self.usage_pct() > 75.0

    def auto_compact(self, summarize_fn) -> bool:
        """Summarize older messages using the LLM.

        Args:
            summarize_fn: callable(text) -> str that calls the LLM to summarize

        Returns:
            True if compaction succeeded, False if circuit breaker tripped
        """
        if self.compact_count >= 3:
            return False  # Circuit breaker

        # Keep the last 4 messages intact (recent context)
        keep_count = min(4, len(self.messages))
        old_messages = self.messages[:-keep_count] if keep_count < len(self.messages) else []
        recent_messages = self.messages[-keep_count:] if keep_count > 0 else []

        if not old_messages:
            return True  # Nothing to compact

        # Build text to summarize
        old_text = "\n".join(
            f"[{m['role']}] {m['content']}" for m in old_messages
        )

        summary_prompt = (
            "Summarize this conversation history into a concise summary. "
            "Preserve: key decisions, tool results, errors encountered, "
            "and the current task state. Be specific and technical.\n\n"
            f"{old_text}"
        )

        try:
            summary = summarize_fn(summary_prompt)
            # Replace old messages with summary
            self.messages = [
                {"role": "system", "content": f"[Previous context summary]\n{summary}"}
            ] + recent_messages
            self.compact_count += 1
            return True
        except Exception:
            self.compact_count += 1
            return False

    # ── Tier 3: FullCompact ──────────────────────────────────────────

    def needs_full_compact(self) -> bool:
        """Check if nuclear reset is needed."""
        return self.usage_pct() > 95.0

    def full_compact(self):
        """Nuclear option: keep only the essentials."""
        # Keep only the last user message and last tool result
        last_user = None
        last_tool = None
        for msg in reversed(self.messages):
            if msg["role"] == "user" and last_user is None:
                last_user = msg
            if msg["role"] == "tool_result" and last_tool is None:
                last_tool = msg

        self.messages = []
        if last_tool:
            self.messages.append(last_tool)
        if last_user:
            self.messages.append(last_user)
        self.compact_count = 0  # Reset circuit breaker


# ── Self-Test ────────────────────────────────────────────────────────

if __name__ == "__main__":
    print("=== Context Manager Self-Test ===")

    cm = ContextManager(max_tokens=200)  # Small window for testing
    cm.set_system_prompt("You are a helpful OS agent.")

    # Add messages
    cm.add_message("user", "List all files")
    cm.add_message("assistant", '{"tool": "list_files", "args": ""}')
    cm.add_message("tool_result", "file1.txt\nfile2.txt\nfile3.txt")
    cm.add_message("assistant", '{"answer": "I found 3 files."}')

    print(f"  Messages: {len(cm.messages)}")
    print(f"  Tokens: ~{cm.token_count()}")
    print(f"  Usage: {cm.usage_pct():.0f}%")

    # Test MicroCompact with consecutive duplicate
    cm.add_message("tool_result", "duplicate output")
    cm.add_message("tool_result", "duplicate output")
    assert cm.messages[-1]["content"] == "[same as previous]"
    print("[PASS] MicroCompact: duplicate detection")

    # Test large truncation
    cm.add_message("tool_result", "x" * 3000)
    assert len(cm.messages[-1]["content"]) < 2000
    assert "[truncated]" in cm.messages[-1]["content"]
    print("[PASS] MicroCompact: large output truncation")

    # Test AutoCompact threshold
    cm2 = ContextManager(max_tokens=50)
    cm2.set_system_prompt("system")
    cm2.add_message("user", "A" * 200)
    assert cm2.needs_auto_compact()
    print(f"[PASS] AutoCompact trigger at {cm2.usage_pct():.0f}%")

    # Test FullCompact
    cm2.full_compact()
    assert len(cm2.messages) == 1  # Just the user message
    print(f"[PASS] FullCompact: {len(cm2.messages)} messages remaining")

    print("\n=== All tests passed! ===")
