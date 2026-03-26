#!/usr/bin/env python3
"""Serial Gemini Proxy — communicates with Folkering OS via COM2 (TCP socket).

QEMU exposes COM2 as TCP server on port 4567.
This proxy connects, reads @@GEMINI_REQ@@{json}@@END@@ from COM2,
calls Gemini API, and writes @@GEMINI_RESP@@{text}@@END@@ back.

Usage: python serial-gemini-proxy.py
"""

import socket
import json
import urllib.request
import ssl
import time
import sys
import threading

API_KEY = "AIzaSyBTJNGrHPMvPn31zLsOOhXUhi35AH5OdTA"
# Model selection: lite for simple/fast queries, flash for complex tasks
GEMINI_LITE = f"https://generativelanguage.googleapis.com/v1beta/models/gemini-3.1-flash-lite-preview:generateContent?key={API_KEY}"
GEMINI_FLASH = f"https://generativelanguage.googleapis.com/v1beta/models/gemini-3-flash-preview:generateContent?key={API_KEY}"
GEMINI_URL = GEMINI_FLASH  # Default to Gemini 3 Flash
COM2_HOST = "127.0.0.1"
COM2_PORT = 4567

REQ_START = b"@@GEMINI_REQ@@"
REQ_END = b"@@END@@"
RESP_START = b"@@GEMINI_RESP@@"
RESP_END = b"@@END@@"


def call_gemini(prompt: str) -> str:
    """Call Gemini API and return response text."""
    body = json.dumps({
        "contents": [{"parts": [{"text": prompt}]}]
    }).encode()

    req = urllib.request.Request(
        GEMINI_URL, data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )

    try:
        ctx = ssl.create_default_context()
        with urllib.request.urlopen(req, context=ctx, timeout=30) as resp:
            result = json.loads(resp.read())
            return result["candidates"][0]["content"]["parts"][0]["text"]
    except Exception as e:
        return f"Error: {e}"


def handle_serial(sock: socket.socket):
    """Read from COM2, process requests, write responses."""
    buf = b""
    print("[SERIAL-PROXY] Connected to COM2, listening for requests...")

    while True:
        try:
            data = sock.recv(4096)
            if not data:
                print("[SERIAL-PROXY] COM2 disconnected")
                break

            buf += data

            # Look for complete request: @@GEMINI_REQ@@{...}@@END@@
            while REQ_START in buf and REQ_END in buf:
                start = buf.index(REQ_START) + len(REQ_START)
                end = buf.index(REQ_END)

                if start > end:
                    # Malformed — skip
                    buf = buf[end + len(REQ_END):]
                    continue

                payload = buf[start:end]
                buf = buf[end + len(REQ_END):]

                print(f"[SERIAL-PROXY] Request: {payload[:100]}...")

                # Parse JSON
                try:
                    data = json.loads(payload)
                    prompt = data.get("prompt", payload.decode("utf-8", errors="replace"))
                except json.JSONDecodeError:
                    prompt = payload.decode("utf-8", errors="replace")

                # Call Gemini
                print(f"[SERIAL-PROXY] Calling Gemini...")
                response_text = call_gemini(prompt)
                print(f"[SERIAL-PROXY] Response: {len(response_text)} chars")

                # Send response back via COM2
                response = RESP_START + response_text.encode("utf-8", errors="replace") + RESP_END + b"\n"
                sock.sendall(response)
                print(f"[SERIAL-PROXY] Sent {len(response)} bytes back to OS")

        except socket.timeout:
            continue
        except Exception as e:
            print(f"[SERIAL-PROXY] Error: {e}")
            break


def main():
    print(f"[SERIAL-PROXY] Connecting to QEMU COM2 at {COM2_HOST}:{COM2_PORT}...")

    while True:
        try:
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.connect((COM2_HOST, COM2_PORT))
            sock.settimeout(1.0)
            handle_serial(sock)
        except ConnectionRefusedError:
            print("[SERIAL-PROXY] COM2 not available yet, retrying in 2s...")
            time.sleep(2)
        except Exception as e:
            print(f"[SERIAL-PROXY] Connection error: {e}, retrying in 2s...")
            time.sleep(2)
        finally:
            try:
                sock.close()
            except:
                pass


if __name__ == "__main__":
    main()
