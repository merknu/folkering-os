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
GEMINI_URL = f"https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent?key={API_KEY}"
PORT = 8080  # Default; override with command line arg


class GeminiHandler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length)

        try:
            data = json.loads(body)
            prompt = data.get("prompt", "")
        except json.JSONDecodeError:
            prompt = body.decode("utf-8", errors="replace")

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

    def log_message(self, format, *args):
        pass  # Suppress default logging


if __name__ == "__main__":
    server = http.server.HTTPServer(("0.0.0.0", PORT), GeminiHandler)
    print(f"[PROXY] Gemini proxy listening on 0.0.0.0:{PORT}")
    print(f"[PROXY] From QEMU guest: http://10.0.2.2:{PORT}/generate")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[PROXY] Shutting down")
