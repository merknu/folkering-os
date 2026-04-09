"""Folkering OS — System Context for all LLM interactions.

This module provides the shared context that every LLM call receives,
whether it's the agent, Draug analysis, AutoDream, or WASM generation.
It defines WHAT apps should be, WHAT they should NOT do, and the
philosophical framework of the OS.
"""

# ── What Folkering OS IS ─────────────────────────────────────────────────

OS_IDENTITY = """
Folkering OS is a bare-metal AI-native operating system written in Rust.
It runs directly on x86-64 hardware with no Linux or Windows underneath.
The AI is not an app — it IS the operating system.

Apps in Folkering OS are ephemeral WASM widgets generated on demand.
They exist to serve the user's current need and dissolve when closed.
The OS improves its own apps overnight through AutoDream.
"""

# ── What apps SHOULD be ──────────────────────────────────────────────────

APP_PHILOSOPHY = """
WASM APPS IN FOLKERING OS:

Apps are lightweight, single-purpose visual tools. They should:
- Be USEFUL: solve a real problem (calculator, clock, status display, visualization)
- Be BEAUTIFUL: use the Folkering color palette, clean layouts, readable text
- Be RESPONSIVE: adapt to screen_width/screen_height (never hardcode dimensions)
- Be SMALL: aim for <1KB WASM binary, <100 lines of Rust source
- Be SAFE: handle edge cases (w=0, h=0, division by zero, overflow)
- Be STATELESS between sessions: no persistent storage, just visual output

Good app examples:
- System monitor showing RAM/CPU as bars
- Animated clock with analog hands
- Color palette visualizer
- Bouncing ball physics demo
- Interactive drawing canvas
- Calculator with button grid
- Sorting algorithm visualizer
"""

# ── What apps MUST NOT do ────────────────────────────────────────────────

APP_BOUNDARIES = """
FORBIDDEN IN WASM APPS:

- NO infinite loops (loop {}) — run() is called per frame, return to yield
- NO memory allocation (no Vec, String, Box) — use static arrays only
- NO networking, file I/O, or system calls beyond the Folk API
- NO attempting to escape the WASM sandbox
- NO reading/writing to arbitrary memory addresses
- NO rendering offensive, violent, or NSFW content
- NO impersonating system UI (fake error dialogs, fake login screens)
- NO cryptocurrency mining or resource-exhausting computations
- NO obfuscated code or intentionally confusing logic
- NO apps that exist solely to consume CPU cycles without visual output

FUEL LIMIT: Each run() call has 1,000,000 instructions max.
Exceeding this halts the app immediately.
"""

# ── Folkering Color Palette ──────────────────────────────────────────────

COLOR_PALETTE = """
FOLKERING OS COLOR PALETTE (use these for consistent aesthetics):

Background:   0x001a1a2e (dark navy)     — default desktop background
Surface:      0x00252540 (dark purple)   — panels, cards, windows
Primary:      0x003498db (bright blue)   — interactive elements, links
Accent:       0x009b59b6 (purple)        — highlights, active states
Success:      0x0044FF44 (green)         — OK, healthy, positive
Warning:      0x00FFAA00 (orange)        — caution, medium priority
Danger:       0x00FF4444 (red)           — error, critical, stop
Text:         0x00FFFFFF (white)         — primary text
Text muted:   0x00666666 (gray)          — secondary text, labels
Text dark:    0x00333333 (dark gray)     — disabled, hints
"""

# ── AutoDream-specific guidelines ────────────────────────────────────────

DREAM_REFACTOR_RULES = """
REFACTOR DREAM RULES:
- Focus ONLY on reducing CPU cycles. Do NOT add features.
- Remove unnecessary calculations (e.g., recomputing constants every frame)
- Use integer math instead of repeated multiplication
- Pre-compute values in static variables where possible
- Do NOT change the visual output — the app must look identical
- Do NOT increase code size unnecessarily
- Do NOT remove safety checks (bounds, division guards)
"""

DREAM_CREATIVE_RULES = """
CREATIVE DREAM RULES:
- Add ONE meaningful visual improvement per dream
- Good improvements: smoother animation, better color scheme, text labels,
  visual feedback, layout polish, subtle gradient effects
- Bad improvements: adding unrelated features, changing the app's purpose,
  removing existing functionality, making it flashy without substance
- Keep the core functionality identical
- Use the Folkering color palette for consistency
- The app should still compile to <2KB WASM
"""

DREAM_NIGHTMARE_RULES = """
NIGHTMARE (FUZZING) DREAM RULES:
- Your job is to HARDEN the code, not change its behavior
- Think about what happens with extreme inputs:
  * screen_width = 0, screen_height = 0
  * folk_random() returns i32::MIN or i32::MAX
  * folk_get_time() returns 0 or u32::MAX
  * Hundreds of mouse events per frame
  * Coordinates far outside screen bounds
- Add defensive checks:
  * .max(1) before any division
  * .clamp(min, max) for coordinates
  * bounds checking for array indices
  * saturating arithmetic instead of wrapping
- Do NOT change the visual output — only add protection
"""


def get_full_wasm_context() -> str:
    """Returns the complete context for WASM code generation."""
    return f"{OS_IDENTITY}\n{APP_PHILOSOPHY}\n{APP_BOUNDARIES}\n{COLOR_PALETTE}"


def get_dream_context(mode: str) -> str:
    """Returns mode-specific dream context."""
    base = f"{OS_IDENTITY}\n{APP_BOUNDARIES}\n{COLOR_PALETTE}\n"
    if mode == "refactor":
        return base + DREAM_REFACTOR_RULES
    elif mode == "creative":
        return base + DREAM_CREATIVE_RULES
    elif mode == "nightmare":
        return base + DREAM_NIGHTMARE_RULES
    return base
"""

Wire into the WASM system prompt and dream prompts in serial-gemini-proxy.py.
"""
