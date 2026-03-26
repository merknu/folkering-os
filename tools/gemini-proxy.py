#!/usr/bin/env python3
"""Gemini API Proxy — runs on Windows host, receives HTTP from Folkering OS.

Folkering OS sends: POST /generate HTTP/1.1 with JSON body {"prompt":"..."}
Proxy forwards to Gemini API via HTTPS, returns plain text response.

Usage: python gemini-proxy.py
Listens on 0.0.0.0:8080 (accessible from QEMU guest as 10.0.2.2:8080)
"""

import http.server
import json
import urllib.request
import urllib.error
import ssl
import sys

API_KEY = "AIzaSyBTJNGrHPMvPn31zLsOOhXUhi35AH5OdTA"
GEMINI_LITE = f"https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent?key={API_KEY}"
GEMINI_FLASH = f"https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent?key={API_KEY}"
GEMINI_URL = GEMINI_FLASH
PORT = 8080  # Default; override with command line arg


class GeminiHandler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        content_length = int(self.headers.get("Content-Length", 0))
        print(f"[PROXY] Receiving {content_length} bytes from {self.client_address}...")
        self.connection.settimeout(30)  # 30s timeout on read
        try:
            body = self.rfile.read(content_length)
        except Exception as e:
            print(f"[PROXY] Read error: {e}")
            self.send_response(500)
            self.end_headers()
            self.wfile.write(b"Read timeout")
            return
        print(f"[PROXY] Received {len(body)} bytes")

        try:
            data = json.loads(body)
            prompt = data.get("prompt", "")
        except json.JSONDecodeError:
            prompt = body.decode("utf-8", errors="replace")

        # Route: /generate_tool for WASM compilation
        if self.path == "/generate_tool":
            print(f"[PROXY] Tool request: {prompt[:80]}...")
            return self.do_POST_generate_tool(prompt)

        print(f"[PROXY] Prompt: {prompt[:80]}...")

        # Forward to Gemini
        gemini_body = json.dumps({
            "contents": [{"parts": [{"text": prompt}]}]
        }).encode()

        req = urllib.request.Request(
            GEMINI_URL,
            data=gemini_body,
            headers={"Content-Type": "application/json"},
            method="POST",
        )

        try:
            ctx = ssl.create_default_context()
            with urllib.request.urlopen(req, context=ctx, timeout=30) as resp:
                result = json.loads(resp.read())
                text = result["candidates"][0]["content"]["parts"][0]["text"]
                print(f"[PROXY] Response: {len(text)} chars")

                self.send_response(200)
                self.send_header("Content-Type", "text/plain")
                self.send_header("Content-Length", str(len(text.encode())))
                self.end_headers()
                self.wfile.write(text.encode())
        except Exception as e:
            error_msg = f"Error: {e}"
            print(f"[PROXY] {error_msg}")
            self.send_response(500)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(error_msg)))
            self.end_headers()
            self.wfile.write(error_msg.encode())

    def do_POST_generate_tool(self, prompt):
        """Generate Rust source via Gemini, compile to WASM, return binary."""
        import subprocess, tempfile, os

        system_prompt = """You are a code generator for Folkering OS (bare-metal Rust).
Generate a SINGLE Rust file that compiles to wasm32-unknown-unknown.
The file must:
- Use #![no_std] and #![no_main]
- Import host functions: extern "C" { fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32); fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32); }
- Export a #[no_mangle] pub extern "C" fn run() function
- Use core::panic_handler
- NO println!, NO std, NO allocation

Example:
```rust
#![no_std]
#![no_main]
extern "C" { fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32); }
#[panic_handler] fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }
#[no_mangle] pub extern "C" fn run() { unsafe { folk_draw_rect(10, 10, 100, 50, 0x00FF00); } }
```
Output ONLY the Rust code, no explanation."""

        gemini_body = json.dumps({
            "contents": [
                {"role": "user", "parts": [{"text": f"{system_prompt}\n\nGenerate: {prompt}"}]}
            ]
        }).encode()

        req = urllib.request.Request(
            GEMINI_URL, data=gemini_body,
            headers={"Content-Type": "application/json"}, method="POST",
        )

        try:
            ctx = ssl.create_default_context()
            with urllib.request.urlopen(req, context=ctx, timeout=60) as resp:
                result = json.loads(resp.read())
                source = result["candidates"][0]["content"]["parts"][0]["text"]

            # Strip markdown code blocks if present
            if "```rust" in source:
                source = source.split("```rust")[1].split("```")[0]
            elif "```" in source:
                source = source.split("```")[1].split("```")[0]

            print(f"[PROXY] Generated {len(source)} chars of Rust source")

            # Write to temp file and compile
            with tempfile.NamedTemporaryFile(suffix=".rs", delete=False, mode="w") as f:
                f.write(source)
                src_path = f.name

            out_path = src_path.replace(".rs", ".wasm")
            compile_cmd = [
                "rustc", "+nightly", "--target", "wasm32-unknown-unknown",
                "-O", "--crate-type", "cdylib",
                "-o", out_path, src_path
            ]

            print(f"[PROXY] Compiling: {' '.join(compile_cmd)}")
            result = subprocess.run(compile_cmd, capture_output=True, text=True, timeout=30)

            if result.returncode != 0:
                error = f"Compile error: {result.stderr[:500]}"
                print(f"[PROXY] {error}")
                self.send_response(500)
                self.send_header("Content-Type", "text/plain")
                self.end_headers()
                self.wfile.write(error.encode())
                return

            # Read compiled WASM binary
            with open(out_path, "rb") as f:
                wasm_binary = f.read()

            print(f"[PROXY] WASM compiled: {len(wasm_binary)} bytes")

            # Return with 4-byte length prefix
            self.send_response(200)
            self.send_header("Content-Type", "application/wasm")
            self.send_header("Content-Length", str(4 + len(wasm_binary)))
            self.end_headers()
            self.wfile.write(len(wasm_binary).to_bytes(4, "little"))
            self.wfile.write(wasm_binary)

            # Cleanup
            os.unlink(src_path)
            os.unlink(out_path)

        except Exception as e:
            error_msg = f"Tool generation error: {e}"
            print(f"[PROXY] {error_msg}")
            self.send_response(500)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(error_msg.encode())

    def log_message(self, format, *args):
        pass  # Suppress default logging


import threading

class ThreadedHTTPServer(http.server.HTTPServer):
    """Handle each request in a new thread to prevent guestfwd blocking."""
    def process_request(self, request, client_address):
        thread = threading.Thread(target=self.process_request_thread,
                                  args=(request, client_address))
        thread.daemon = True
        thread.start()

    def process_request_thread(self, request, client_address):
        try:
            self.finish_request(request, client_address)
        except Exception:
            self.handle_error(request, client_address)
        finally:
            self.shutdown_request(request)


if __name__ == "__main__":
    server = ThreadedHTTPServer(("0.0.0.0", PORT), GeminiHandler)
    print(f"[PROXY] Gemini proxy listening on 0.0.0.0:{PORT} (threaded)")
    print(f"[PROXY] From QEMU guest: http://10.0.2.2:{PORT}/generate")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[PROXY] Shutting down")
