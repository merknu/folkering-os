"""LLM Router — Multi-tier hybrid model routing with auto-escalation.

Tiers: FAST (local Ollama) → MEDIUM (Gemini Lite) → HEAVY (Gemini Flash) → ULTRA (Gemini Pro)
Each request is routed to the cheapest tier that can handle it.
On failure, automatically escalates to the next tier.
"""

import json
import time
import urllib.request
import ssl


# ── State ────────────────────────────────────────────────────────────────

_ultra_calls = []  # Timestamps of ultra-tier calls (session-scoped)
_ULTRA_MAX_PER_SESSION = 3
_ULTRA_COOLDOWN_S = 300  # 5 minutes


def _ultra_allowed():
    """Check if ultra tier is within rate limits."""
    now = time.time()
    _ultra_calls[:] = [t for t in _ultra_calls if now - t < 3600]
    if len(_ultra_calls) >= _ULTRA_MAX_PER_SESSION:
        return False
    if _ultra_calls and now - _ultra_calls[-1] < _ULTRA_COOLDOWN_S:
        return False
    return True


def _ultra_record():
    """Record an ultra-tier API call."""
    _ultra_calls.append(time.time())


# ── Provider Backends ────────────────────────────────────────────────────

def call_gemini(prompt, model, base_url, api_key):
    """Google Gemini API."""
    url = f"{base_url}/models/{model}:generateContent?key={api_key}"
    body = json.dumps({"contents": [{"parts": [{"text": prompt}]}]}).encode()
    req = urllib.request.Request(url, data=body,
        headers={"Content-Type": "application/json"}, method="POST")
    ctx = ssl.create_default_context()
    with urllib.request.urlopen(req, context=ctx, timeout=30) as resp:
        result = json.loads(resp.read())
        return result["candidates"][0]["content"]["parts"][0]["text"]


def call_openai(prompt, model, base_url, api_key):
    """OpenAI-compatible API (OpenAI, LM Studio, Ollama compat, llama.cpp)."""
    url = f"{base_url}/chat/completions"
    body = json.dumps({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 4096,
        "temperature": 0.7,
    }).encode()
    headers = {"Content-Type": "application/json"}
    if api_key:
        headers["Authorization"] = f"Bearer {api_key}"
    req = urllib.request.Request(url, data=body, headers=headers, method="POST")
    kwargs = {"timeout": 45}
    if url.startswith("https"):
        kwargs["context"] = ssl.create_default_context()
    with urllib.request.urlopen(req, **kwargs) as resp:
        result = json.loads(resp.read())
        return result["choices"][0]["message"]["content"]


def call_local(prompt, model, base_url):
    """Local Ollama API — uses native /api/chat for thinking support."""
    base = base_url.rstrip("/v1").rstrip("/")
    url = f"{base}/api/chat"
    body = json.dumps({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": False,
    }).encode()
    req = urllib.request.Request(url, data=body,
        headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=45) as resp:
        result = json.loads(resp.read())
        msg = result.get("message", {})
        content = msg.get("content", "")
        thinking = msg.get("thinking", "")
        if thinking:
            return f"<think>\n{thinking}\n</think>\n{content}"
        return content


def call_claude(prompt, model, base_url, api_key):
    """Anthropic Claude API."""
    url = f"{base_url}/messages"
    body = json.dumps({
        "model": model,
        "max_tokens": 4096,
        "messages": [{"role": "user", "content": prompt}],
    }).encode()
    req = urllib.request.Request(url, data=body, headers={
        "Content-Type": "application/json",
        "x-api-key": api_key,
        "anthropic-version": "2023-06-01",
    }, method="POST")
    ctx = ssl.create_default_context()
    with urllib.request.urlopen(req, context=ctx, timeout=30) as resp:
        result = json.loads(resp.read())
        return result["content"][0]["text"]


# ── Dispatcher ───────────────────────────────────────────────────────────

def dispatch(provider, url, model, prompt, api_key=""):
    """Route to the correct provider backend."""
    if provider == "gemini":
        return call_gemini(prompt, model, url, api_key)
    elif provider == "local":
        return call_local(prompt, model, url)
    elif provider == "openai":
        return call_openai(prompt, model, url, api_key)
    elif provider == "claude":
        return call_claude(prompt, model, url, api_key)
    else:
        return f"Error: unknown provider '{provider}'"


def call_llm(prompt, tier_chain, api_key, fast_config, tier="fast", model_override=""):
    """Call LLM using the hybrid router with auto-escalation on failure.

    Args:
        prompt: The text prompt
        tier_chain: List of (tier_name, get_config_fn) tuples
        api_key: Cloud API key
        fast_config: (provider, model, url) for FAST fallback
        tier: Starting tier name
        model_override: Override model for first attempt
    """
    tier_names = [t[0] for t in tier_chain]
    start_idx = tier_names.index(tier) if tier in tier_names else 0

    for i in range(start_idx, len(tier_chain)):
        tier_name, get_config = tier_chain[i]

        if tier_name == "ultra" and not _ultra_allowed():
            return "Error: ultra tier rate-limited (max 3/session, 5min cooldown)"

        provider, model, url = get_config()

        if model_override and i == start_idx:
            model = model_override

        if provider != "local" and not api_key:
            if i == start_idx:
                print(f"[ROUTER] No API key for {tier_name}/{provider}, falling back to FAST")
            provider, model, url = fast_config

        print(f"[ROUTER] tier={tier_name} -> {provider}/{model}")
        try:
            result = dispatch(provider, url, model, prompt, api_key)
            if result and not result.startswith("Error:"):
                if tier_name == "ultra":
                    _ultra_record()
                return result
            raise ValueError(result)
        except Exception as e:
            if i < len(tier_chain) - 1:
                print(f"[ROUTER] {tier_name} failed ({e}), escalating...")
            else:
                return f"Error: all tiers exhausted ({e})"

    return "Error: no tiers available"


def route_for_task(msg_type, prompt=""):
    """Decide which tier to use based on task type and prompt content."""
    if "draug" in prompt.lower() or "background daemon" in prompt.lower():
        return "fast"
    if msg_type == "wasm_gen_request" or "generate_wasm" in prompt.lower():
        return "medium"
    if msg_type == "chat_request" and ("tool" in prompt.lower() or "agent" in prompt.lower()):
        return "medium"
    if len(prompt) > 4000:
        return "medium"
    return "fast"
