#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Adapt kemu's x86 pt_regs syscall entry points to l4h's six arguments."""
import sys

seen = set()
with open(sys.argv[1]) as table:
    for line in table:
        fields = line.split()
        if len(fields) < 4 or not fields[0].isdigit():
            continue
        nr, abi, _, symbol = fields[:4]
        if abi not in ("common", "64") or symbol in seen:
            continue
        seen.add(symbol)
        print(f"KEMU_SYSCALL({nr}, {symbol})")
