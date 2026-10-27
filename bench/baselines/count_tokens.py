#!/usr/bin/env python3
"""Count tokens on stdin with tiktoken. Usage: count_tokens.py [encoding] < file  (default o200k_base)."""
import sys
import tiktoken

enc = tiktoken.get_encoding(sys.argv[1] if len(sys.argv) > 1 else "o200k_base")
data = sys.stdin.buffer.read().decode("utf-8", errors="replace")
print(len(enc.encode(data, disallowed_special=())))
