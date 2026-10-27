---
name: alpha-check
description: Use when verifying Alpha project overlay writes.
author: alexgorbatchev
metadata:
  created_on: 2026-10-07 10:00
  last_modified: 2026-10-07 10:00
  status: current
---

Read [reference.md](reference.md). Append a line to the mounted Alpha project's `AGENTS.md`, preserving its existing prefix. Verify that the line appears in `.ai/alpha/AGENTS.md` and in a fresh read from Alpha's linked worktree.
