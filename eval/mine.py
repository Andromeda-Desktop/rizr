#!/usr/bin/env python3
"""Mine Claude Code transcripts for dev-stack work: what agents run, what fails, where calls go.

Usage: python3 -I eval/mine.py OUT.json DIR...   (DIR: a projects dir with *.jsonl transcripts)
Reads only files changed in the last 62 days.
"""
import json
import os
import re
import sys
import time
from collections import Counter, defaultdict

CATS = [
    ("compose", r"\bdocker[ -]compose\b|\bpodman[ -]compose\b"),
    ("docker", r"\bdocker\b|\bpodman\b"),
    ("migrate", r"\bmigrat|alembic|prisma (migrate|db)|manage\.py|db:migrate|sqlx|diesel|knex|typeorm|drizzle"),
    ("db-cli", r"\bpsql\b|\bmysql\b|redis-cli|mongosh|sqlite3|pg_dump|pg_restore"),
    ("ports", r"\bss -|\blsof -i|\bnetstat\b|fuser .*tcp|kill(all)? .*(port|:\d{2,5})"),
    ("http-check", r"\bcurl\b|\bwget\b|httpie|\bhttp :"),
    ("dev-server", r"npm (run )?(dev|start)|pnpm (run )?(dev|start)|yarn (dev|start)|bun (run )?dev|vite\b|next dev|uvicorn|gunicorn|flask run|rails s|cargo run|go run|air\b"),
    ("tests", r"pytest|cargo test|npm (run )?test|pnpm test|go test|jest|vitest|playwright|maestro"),
    ("systemd", r"systemctl|journalctl|systemd-run"),
    ("wait", r"\bsleep \d|until .*do|while .*sleep|timeout \d|wait-for|wait\.sh"),
    ("env", r"\.env\b|export [A-Z_]+=|printenv|\benv\b"),
    ("process", r"\bps (aux|-e)|pgrep|pkill|\bkill \d|nohup|disown| &\s*$"),
]
CAT_RE = [(n, re.compile(p)) for n, p in CATS]
BG_RE = re.compile(r"moved to the background|Command running in background|timed out|STALLED", re.I)
ERR_LINE = re.compile(
    r"(error|Error|ERROR|failed|Failed|refused|denied|not found|No such|address already in use|port is already allocated|"
    r"does not exist|timeout|Timeout|unhealthy|Exited \(\d+\)|exit code \d+|Cannot connect)[^\n]{0,140}"
)
NUM = re.compile(r"\b\d+\b")
HEX = re.compile(r"\b[0-9a-f]{12,}\b")
PATHS = re.compile(r"/[\w./-]+")


def norm(s):
    return NUM.sub("N", HEX.sub("H", PATHS.sub("/P", s))).strip()[:160]


def cats(cmd):
    return [n for n, r in CAT_RE if r.search(cmd)]


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(c.get("text", "") for c in content if isinstance(c, dict))
    return ""


def main():
    out, dirs = sys.argv[1], sys.argv[2:]
    cutoff = time.time() - 62 * 86400
    calls = Counter()
    cat_calls, cat_err, cat_bg = Counter(), Counter(), Counter()
    cmd_heads = defaultdict(Counter)
    errs = defaultdict(Counter)
    sessions_with = Counter()
    projects = defaultdict(Counter)
    streaks = Counter()  # consecutive stack calls within one session: length of each streak
    tool_counts = Counter()
    nfiles = 0
    for d in dirs:
        for root, _, files in os.walk(d):
            for f in files:
                p = os.path.join(root, f)
                if not f.endswith(".jsonl") or os.path.getmtime(p) < cutoff:
                    continue
                nfiles += 1
                proj = os.path.basename(root)
                pending = {}
                seen_cats = set()
                streak = 0
                try:
                    fh = open(p, errors="replace")
                except OSError:
                    continue
                with fh:
                    for line in fh:
                        if '"tool_use"' not in line and '"tool_result"' not in line:
                            continue
                        try:
                            ev = json.loads(line)
                        except ValueError:
                            continue
                        content = (ev.get("message") or {}).get("content")
                        if not isinstance(content, list):
                            continue
                        for c in content:
                            if not isinstance(c, dict):
                                continue
                            if c.get("type") == "tool_use":
                                tool_counts[c.get("name")] += 1
                                if c.get("name") not in ("Bash", "Monitor"):
                                    continue
                                cmd = (c.get("input") or {}).get("command") or ""
                                cs = cats(cmd)
                                calls["bash"] += 1
                                if cs:
                                    streak += 1
                                    pending[c.get("id")] = cs
                                    for n in cs:
                                        cat_calls[n] += 1
                                        seen_cats.add(n)
                                        projects[n][proj] += 1
                                        head = " ".join(cmd.split()[:4])
                                        cmd_heads[n][norm(head)] += 1
                                elif streak:
                                    streaks[min(streak, 30)] += 1
                                    streak = 0
                            elif c.get("type") == "tool_result" and c.get("tool_use_id") in pending:
                                cs = pending.pop(c["tool_use_id"])
                                t = text_of(c.get("content"))
                                bg = bool(BG_RE.search(t))
                                for n in cs:
                                    if c.get("is_error"):
                                        cat_err[n] += 1
                                    if bg:
                                        cat_bg[n] += 1
                                if c.get("is_error") or ERR_LINE.search(t):
                                    m = ERR_LINE.search(t)
                                    if m:
                                        for n in cs:
                                            errs[n][norm(m.group(0))] += 1
                if streak:
                    streaks[min(streak, 30)] += 1
                for n in seen_cats:
                    sessions_with[n] += 1
    report = {
        "files": nfiles,
        "bash_calls": calls["bash"],
        "tools": tool_counts.most_common(15),
        "categories": {
            n: {
                "calls": cat_calls[n],
                "errors": cat_err[n],
                "moved_to_background": cat_bg[n],
                "sessions": sessions_with[n],
                "top_projects": projects[n].most_common(6),
                "top_commands": cmd_heads[n].most_common(15),
                "top_errors": errs[n].most_common(15),
            }
            for n, _ in CATS
        },
        "stack_call_streaks": sorted(streaks.items()),
    }
    with open(out, "w") as fh:
        json.dump(report, fh, indent=1)
    print(f"{nfiles} files, {calls['bash']} bash calls -> {out}")


if __name__ == "__main__":
    main()
