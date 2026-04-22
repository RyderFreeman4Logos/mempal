#!/bin/sh

prime_output="$(mempal prime 2>/dev/null || true)"
if [ -n "$prime_output" ]; then
  printf '%s\n' "$prime_output"
fi
