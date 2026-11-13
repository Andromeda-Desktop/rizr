#!/usr/bin/env python3
"""Deep mine of Claude Code transcripts: normalized commands, errors, recovery chains, repeats.

Usage: python3 -I eval/mine_deep.py OUT.json DIR...   (all counts are regex-based, approximate)
"""
import json, os, re, sys, shlex
from collections import Counter, defaultdict

SKIP = {"cd", "echo", "set", "export", "true", "source", ".", "unset", "printf", "exit", ":", "wait", "pushd", "popd", "test", "[", "[["}
WRAP = {"timeout", "nice", "ionice", "time", "env", "command", "exec", "stdbuf", "setsid", "flock", "sudo", "xargs", "nohup"}
SUB = {"git", "cargo", "just", "adb", "docker", "podman", "npm", "pnpm", "yarn", "meson", "ninja", "systemctl", "gh", "pacman", "gradle", "gradlew", "journalctl", "busctl", "gdbus", "pip", "uv", "go", "make", "rustup", "flatpak", "dbus-send", "loginctl", "maestro", "emulator", "qdbus"}
SECRET = re.compile(r"(sk-[A-Za-z0-9_-]{10,}|ghp_\w+|(?i:token|password|passwd|secret|api[_-]?key)=\S+|[\w.+-]+@[\w-]+\.[\w.]+|Bearer \S+)")


def mask(s):
    s = SECRET.sub("<S>", s)
    s = re.sub(r"\b[0-9a-f]{7,40}\b", "<H>", s)
    s = re.sub(r"(/[\w.@+~-]+){2,}", "<P>", s)
    s = re.sub(r"\d+", "N", s)
    return s


def split_cmd(c):
    out, cur, q = [], "", None
    i = 0
    while i < len(c):
        ch = c[i]
        if q:
            cur += ch
            if ch == q:
                q = None
        elif ch in "'\"":
            q = ch
            cur += ch
        elif ch in ";|&\n":
            out.append(cur)
            cur = ""
            while i + 1 < len(c) and c[i + 1] in ";|&":
                i += 1
        else:
            cur += ch
        i += 1
    out.append(cur)
    return [x.strip() for x in out if x.strip()]


def seg_key(seg):
    try:
        t = shlex.split(seg, posix=True)
    except ValueError:
        t = seg.split()
    while t:
        w = t[0]
        if re.match(r"^[A-Za-z_]\w*=", w):
            t = t[1:]
        elif w in WRAP:
            t = t[1:]
            while t and (t[0].startswith("-") or re.match(r"^[\d.]+[smh]?$", t[0]) or re.match(r"^[\w/.-]*\.lock$", t[0])):
                t = t[1:]
        else:
            break
    if not t:
        return None
    p = os.path.basename(t[0])
    if p in SKIP:
        return None
    if p.startswith("./") or "/" in t[0]:
        p = os.path.basename(t[0])
    if p in SUB:
        rest = [x for x in t[1:] if not x.startswith("-") and not re.match(r"^[A-Za-z_]\w*=", x)]
        if p == "adb" and "-s" in t[1:]:
            pass
        if rest:
            return p + " " + mask(rest[0])[:30]
    return mask(p)[:40]


def norm(cmd):
    keys = [k for k in (seg_key(s) for s in split_cmd(cmd)) if k]
    return (keys[0] if keys else "(shell-builtin)"), keys


def res_text(c):
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        return "".join(x.get("text", "") for x in c if isinstance(x, dict))
    return ""


def classify_err(txt, tool):
    t = txt[:600]
    pats = [
        ("hook_blocked", r"Blocked:|hook.*(block|denied)|PreToolUse.*(block|denied)"),
        ("perm_denied", r"permission.*(denied|to use)|was denied|doesn't want to proceed|not allowed|auto mode classifier|user rejected|rejected the"),
        ("edit_not_found", r"String to replace not found|old_string.*not found"),
        ("edit_not_unique", r"Found \d+ matches|not unique"),
        ("not_read_first", r"has not been read|must (first )?read|File has been modified"),
        ("file_missing", r"No such file|does not exist|File does not exist|ENOENT"),
        ("too_large", r"exceeds maximum|too large|too long|File content .* exceeds"),
        ("stalled", r"STALLED|exit 125|Exit code 125"),
        ("timeout", r"timed out|Timeout|exit code 124|Exit code 124"),
        ("cmd_not_found", r"command not found|not found in PATH"),
        ("interrupted", r"interrupted|Request interrupted"),
        ("exit_nonzero", r"Exit code \d+"),
    ]
    for n, p in pats:
        if re.search(p, t, re.I):
            return n
    return "other"


def first_line(txt):
    for ln in txt.splitlines():
        ln = ln.strip()
        if ln and not ln.startswith("Exit code"):
            return mask(ln)[:140]
    return mask(txt.strip()[:140])


def example(cmd, n=140):
    return mask(" ".join(cmd.split()))[:n] if False else SECRET.sub("<S>", " ".join(cmd.split()))[:n]


class Stat:
    __slots__ = ("calls", "err", "chars", "sess", "proj", "ex", "errex")

    def __init__(s):
        s.calls = s.err = s.chars = 0
        s.sess = set()
        s.proj = set()
        s.ex = []
        s.errex = []


def codex(root):
    import glob
    st = defaultdict(lambda: [0, 0, 0, set(), []])
    rep = Counter(); tot = Counter(); errs = Counter(); chains = defaultdict(lambda: [0, 0, 0])
    for f in glob.glob(os.path.join(root, "**", "*.jsonl"), recursive=True):
        tot["files"] += 1
        calls = []; idx = {}
        for line in open(f, errors="replace"):
            try:
                d = json.loads(line)
            except ValueError:
                continue
            p = d.get("payload") or {}
            t = p.get("type")
            if t in ("function_call", "custom_tool_call"):
                raw = p.get("arguments") if t == "function_call" else p.get("input")
                m = re.search(r'"cmd"\s*:\s*"((?:[^"\\]|\\.)*)"', raw or "")
                if m:
                    try:
                        cmd = json.loads('"' + m.group(1) + '"')
                    except ValueError:
                        cmd = m.group(1)
                else:
                    cmd = ""
                key = norm(cmd)[0] if cmd else "tool:" + str(p.get("name"))
                idx[p.get("call_id")] = len(calls)
                calls.append([key, cmd, None, 0])
                tot["calls"] += 1
            elif t in ("function_call_output", "custom_tool_call_output"):
                i = idx.get(p.get("call_id"))
                if i is None:
                    continue
                o = p.get("output")
                txt = o if isinstance(o, str) else res_text(o)
                m = re.search(r"(?:exited with code|exit code:?|Exit code:?)\s*(\d+)", txt)
                err = bool(m and m.group(1) != "0")
                calls[i][2] = err; calls[i][3] = len(txt)
                s = st[calls[i][0]]
                s[0] += 1; s[1] += err; s[2] += len(txt)
                if len(s[4]) < 2: s[4].append(example(calls[i][1], 120))
                tot["chars"] += len(txt); tot["errors"] += err
                if err: errs[first_line(txt)] += 1
        for n, (key, cmd, err, ch) in enumerate(calls):
            if err is None: continue
            for m_ in range(max(0, n - 12), n):
                if calls[m_][1] == cmd and cmd:
                    rep[key] += 1; break
            if err:
                c = chains[key]; c[0] += 1
                for m_ in range(n + 1, min(len(calls), n + 16)):
                    if calls[m_][0] == key and calls[m_][2] is False:
                        c[1] += m_ - n; break
                else:
                    c[2] += 1
    return {"totals": dict(tot),
            "commands": [{"key": k, "calls": v[0], "errors": v[1], "out_chars": v[2], "examples": v[4]} for k, v in sorted(st.items(), key=lambda kv: -kv[1][0])[:60]],
            "repeats": rep.most_common(20), "err_texts": errs.most_common(25),
            "chains": [{"key": k, "failed": v[0], "recovery_calls": v[1], "unrecovered": v[2]} for k, v in sorted(chains.items(), key=lambda kv: -kv[1][0])[:25]]}


def main(out, dirs):
    stats = defaultdict(Stat)          # (tool, key)
    errkinds = Counter()               # (tool, kind)
    errkind_ex = defaultdict(list)
    errtexts = Counter()
    hook_rules = Counter()
    hook_ex = {}
    chains = defaultdict(lambda: [0, 0, 0, []])   # key -> [n_chains, recovery_calls, unrecovered, ex]
    repeats = defaultdict(lambda: [0, 0, set(), []])  # key -> [repeat calls, chars, sessions, ex]
    read_rep = [0, 0, Counter()]
    seq_pairs = Counter()
    bigouts = []
    trunc = Counter()
    tot = Counter()
    subagent_types = Counter()
    sub_calls_per_agent = []
    waits = Counter()

    files = []
    for d in dirs:
        for r, _, fs in os.walk(d):
            for f in fs:
                if f.endswith(".jsonl"):
                    files.append(os.path.join(r, f))
    for fp in files:
        rel = os.path.relpath(fp, next(d for d in dirs if fp.startswith(d)))
        proj = rel.split(os.sep)[0]
        sess = fp
        is_sub = "/subagents/" in fp
        uses = {}      # id -> (tool, key, cmd, idx)
        calls = []     # ordered (id, tool, key, cmd)
        seen_ids = set()
        edits_since = {}
        reads = Counter()
        try:
            fh = open(fp, errors="replace")
        except OSError:
            continue
        tot["files"] += 1
        tot["subagent_files"] += is_sub
        ncalls_here = 0
        for line in fh:
            if '"tool_use' not in line and '"tool_result"' not in line:
                continue
            try:
                d = json.loads(line)
            except ValueError:
                continue
            if d.get("type") not in ("assistant", "user"):
                continue
            c = d.get("message", {}).get("content")
            if not isinstance(c, list):
                continue
            for b in c:
                if not isinstance(b, dict):
                    continue
                if b.get("type") == "tool_use" and d["type"] == "assistant":
                    i = b.get("id")
                    if i in seen_ids:
                        continue
                    seen_ids.add(i)
                    tool = b.get("name", "?")
                    inp = b.get("input") or {}
                    cmd = ""
                    if tool == "Bash":
                        cmd = inp.get("command", "")
                        key, _ = norm(cmd)
                    elif tool in ("Read", "Edit", "Write"):
                        cmd = inp.get("file_path", "")
                        key = tool
                        if tool == "Read":
                            fkey = (str(cmd), str(inp.get("offset")), str(inp.get("limit")))
                            reads[fkey] += 1
                            if reads[fkey] > 1 and not edits_since.get(cmd):
                                read_rep[0] += 1
                                read_rep[2][mask(cmd)] += 1
                            edits_since[cmd] = False
                        else:
                            edits_since[cmd] = True
                    elif tool == "Agent":
                        key = "Agent:" + str(inp.get("subagent_type", "general"))
                        subagent_types[key] += 1
                        cmd = str(inp.get("description", ""))
                    elif tool == "Monitor":
                        key = "Monitor"
                        cmd = inp.get("command", "")
                    elif tool in ("Grep", "Glob"):
                        key = tool
                        cmd = str(inp.get("pattern", ""))
                    else:
                        key = tool
                        cmd = str(inp)[:100]
                    uses[i] = (tool, key, cmd, len(calls))
                    calls.append([i, tool, key, cmd, None, 0])
                    ncalls_here += 1
                    tot["calls"] += 1
                elif b.get("type") == "tool_result":
                    i = b.get("tool_use_id")
                    if i not in uses:
                        continue
                    idx = uses[i][3]
                    if calls[idx][4] is not None:
                        continue
                    txt = res_text(b.get("content"))
                    err = bool(b.get("is_error")) is True or str(b.get("is_error")) == "True"
                    if not err and re.match(r"Exit code [1-9]", txt):
                        err = True
                    calls[idx][4] = err
                    calls[idx][5] = len(txt)
                    tool, key, cmd, _ = uses[i]
                    st = stats[(tool, key)]
                    st.calls += 1
                    st.chars += len(txt)
                    st.sess.add(sess)
                    st.proj.add(proj)
                    if len(st.ex) < 2:
                        st.ex.append(example(cmd))
                    tot["chars"] += len(txt)
                    if txt.startswith("<persisted-output>") or "Output too large" in txt[:200]:
                        trunc[(tool, key)] += 1
                    if "[output truncated" in txt or "truncated" in txt[-200:].lower():
                        trunc[("any", "truncated-tail")] += 1
                    if len(txt) > 15000:
                        bigouts.append((len(txt), tool, key))
                    if err:
                        st.err += 1
                        tot["errors"] += 1
                        kind = classify_err(txt, tool)
                        errkinds[(tool, kind)] += 1
                        et = first_line(txt)
                        errtexts[(tool, et)] += 1
                        if len(errkind_ex[(tool, kind)]) < 2:
                            errkind_ex[(tool, kind)].append(example(cmd, 100) + " => " + et)
                        if len(st.errex) < 2:
                            st.errex.append(et)
                        if kind == "hook_blocked":
                            m = re.search(r"Blocked:?\s*([^\n]{0,110})", txt)
                            r = mask(m.group(1)) if m else et
                            hook_rules[r] += 1
                            hook_ex.setdefault(r, example(cmd, 100))
                    elif "Blocked:" in txt[:200]:
                        hook_rules[mask(txt[:110])] += 1
        # per-session analysis
        if is_sub:
            sub_calls_per_agent.append(ncalls_here)
        prev = None
        run = 0
        for n, (i, tool, key, cmd, err, ch) in enumerate(calls):
            if err is None:
                continue
            if tool == "Bash":
                if prev and prev[0] == key and prev[1] == cmd and not prev[2] and not err is None:
                    pass
                # wait/poll loops
                if re.search(r"\bsleep\s+\d|until .*; do|while .*; do|tools/wait\.sh", cmd):
                    waits[key] += 1
            # repeated identical command (ignoring errors), consecutive-or-near within 12 calls
            if tool == "Bash" and cmd:
                for m in range(max(0, n - 12), n):
                    if calls[m][1] == "Bash" and calls[m][3] == cmd:
                        r = repeats[key]
                        r[0] += 1
                        r[1] += ch
                        r[2].add(sess)
                        if len(r[3]) < 2:
                            r[3].append(example(cmd, 120))
                        break
            if err:
                # recovery chain: calls until same key succeeds
                rec = None
                for m in range(n + 1, min(len(calls), n + 16)):
                    if calls[m][2] == key and calls[m][1] == tool and calls[m][4] is False:
                        rec = m - n
                        break
                c = chains[(tool, key)]
                c[0] += 1
                if rec:
                    c[1] += rec
                else:
                    c[2] += 1
                if len(c[3]) < 2:
                    c[3].append(example(cmd, 110) + " rec=" + str(rec))
            if n + 1 < len(calls):
                seq_pairs[(key, calls[n + 1][2])] += 1

    def sstat(k, s):
        return {"tool": k[0], "key": k[1], "calls": s.calls, "errors": s.err, "err_rate": round(s.err / max(1, s.calls), 3),
                "out_chars": s.chars, "sessions": len(s.sess), "projects": len(s.proj), "examples": s.ex, "err_examples": s.errex}

    res = {
        "totals": dict(tot),
        "by_tool": {},
        "commands": sorted((sstat(k, s) for k, s in stats.items()), key=lambda x: -x["calls"])[:400],
        "err_kinds": [{"tool": k[0], "kind": k[1], "n": v, "examples": errkind_ex[k]} for k, v in errkinds.most_common(60)],
        "err_texts": [{"tool": k[0], "text": k[1], "n": v} for k, v in errtexts.most_common(80)],
        "hook_rules": [{"rule": k, "n": v, "example": hook_ex.get(k, "")} for k, v in hook_rules.most_common(40)],
        "chains": sorted(({"tool": k[0], "key": k[1], "failed": v[0], "recovery_calls": v[1], "unrecovered": v[2], "examples": v[3]} for k, v in chains.items()), key=lambda x: -(x["recovery_calls"] + x["failed"]))[:80],
        "repeats": sorted(({"key": k, "repeat_calls": v[0], "out_chars": v[1], "sessions": len(v[2]), "examples": v[3]} for k, v in repeats.items()), key=lambda x: -x["repeat_calls"])[:60],
        "read_repeat": {"calls": read_rep[0], "top_files": read_rep[2].most_common(15)},
        "persisted_output": [{"tool": k[0], "key": k[1], "n": v} for k, v in trunc.most_common(30)],
        "bigouts_over_15k": {"n": len(bigouts), "chars": sum(b[0] for b in bigouts), "top": Counter(b[2] for b in bigouts).most_common(15)},
        "subagent_types": subagent_types.most_common(),
        "subagent_calls_avg": round(sum(sub_calls_per_agent) / max(1, len(sub_calls_per_agent)), 1),
        "wait_poll": waits.most_common(20),
        "seq_pairs": [{"a": a, "b": b, "n": n} for (a, b), n in seq_pairs.most_common(40) if a == b][:25],
    }
    bt = defaultdict(lambda: [0, 0, 0])
    for (tool, _), s in stats.items():
        x = bt[tool]
        x[0] += s.calls
        x[1] += s.err
        x[2] += s.chars
    res["by_tool"] = {t: {"calls": v[0], "errors": v[1], "out_chars": v[2]} for t, v in sorted(bt.items(), key=lambda kv: -kv[1][0])}
    cx = os.path.expanduser("~/.codex/sessions")
    if os.path.isdir(cx):
        res["codex"] = codex(cx)
    json.dump(res, open(out, "w"), indent=1)
    print("done", dict(tot))


if __name__ == "__main__":
    main(sys.argv[1], [os.path.abspath(d) for d in sys.argv[2:]])
