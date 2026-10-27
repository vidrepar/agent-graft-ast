"""Task-level benchmark: real edits with `claude -p`, baseline vs agent-graft-ast MCP.

Run with `python3 -I bench.py <cmd>`; run.sh drives the whole suite.
  prep                          create/refresh working copies
  run TASK ARM N [--smoke]      one run -> out/TASK.ARM.N.json
  check TASK                    run TASK's check against its working copy as it is now
  aggregate                     out/*.json -> results.json + RESULTS.md
"""

import json
import shutil
import os
import re
import statistics
import subprocess
import sys
import time
from collections import Counter
from fnmatch import fnmatch
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
OUT = Path(os.environ.get("BENCH_OUT", HERE / "out"))
HOME = Path.home()
WORK = Path(os.environ.get("HOPCOUNT_WORK", HOME / ".cache" / "hopcount"))
BIN = Path(os.environ.get("GRAFT_BIN", REPO / "target" / "release" / "agent-graft-ast"))
MODEL = os.environ.get("MODEL", "claude-sonnet-5-5")
TIMEOUT_S = int(os.environ.get("RUN_TIMEOUT_S", "1500"))
MAX_BUDGET_USD = os.environ.get("MAX_BUDGET_USD", "3")  # per run: claude stops a run that passes it
SUITE_BUDGET_USD = float(os.environ.get("SUITE_BUDGET_USD", "25"))  # all runs in out/ together

# Where each task repo lives: REPO_<NAME> (upper case, dashes as underscores), e.g.
# REPO_RUST_ANALYZER=~/src/rust-analyzer. rust-analyzer is cloned on first use if unset.
RUST_ANALYZER_URL = "https://github.com/rust-lang/rust-analyzer"


def source(repo):
    env = os.environ.get("REPO_" + repo.upper().replace("-", "_"))
    if env:
        return Path(env).expanduser()
    if repo == "rust-analyzer":
        dst = WORK / "rust-analyzer-src"
        if not (dst / ".git").exists():
            subprocess.run(["git", "clone", "-q", "--depth", "1", RUST_ANALYZER_URL, str(dst)], check=True)
        return dst
    sys.exit(f"set REPO_{repo.upper().replace('-', '_')} to a checkout of {repo}")
RSYNC_EXCLUDES = [
    "node_modules", "dist", ".git", "graft", ".agent-graft", ".env*", "target",
    ".turbo", ".astro", ".wrangler", "test-results", "*.sqlite", "*.tsbuildinfo",
    ".herdr", ".next", ".claude", ".docs-store",
]

BOTH_ARMS_SUFFIX = (
    "\n\nMake the edit directly in the files. Do not run builds, tests, type-checkers or "
    "formatters, and do not commit."
)
TREATMENT_SUFFIX = (
    " The agent-graft-ast MCP tools (mcp__agent-graft-ast__*) are available for this "
    "repository: prefer them over Grep/Glob/Read for finding, reading and tracing code."
)

SG = shutil.which("ast-grep") or "ast-grep"
COMBO_SUFFIX = (
    " Two code tools are available for this repository: graft's MCP tools (mcp__graft__*) for finding, "
    f"reading and tracing code (who calls what), and ast-grep through Bash, as `{SG}`, for structural search "
    f"and rewrite, e.g. `{SG} run -p 'foo($A)' -r 'bar($A)' -l rust -U crates/x`. Prefer them over Grep/Glob/Read "
    "and over editing many files by hand."
)
GRAFT = os.environ.get("GRAFT", "graft")

# Isolation: no user/project settings (so no plugins, hooks, CLAUDE.md, permissions rules),
# no skills or slash commands, no auto-memory, no tool search (MCP tools load up front),
# only the built-in file/shell tools, and only the MCP servers in --mcp-config.
CLEAN_ENV_KEYS = ["HOME", "USER", "LOGNAME", "PATH", "SHELL", "LANG", "TMPDIR", "TERM"]
EXTRA_ENV = {"ENABLE_TOOL_SEARCH": "false", "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1"}


REDIRECT_MARK = "agent-graft-ast answered this search"


def claude_args(prompt, mcp_config, task, hooks=None, arm="treatment"):
    """No permission bypass: edits auto-accepted, everything else must be on the allowlist.

    Bash is allowed only for the task's own `test` command, when it defines one.
    `hooks` goes into --settings (the only settings source the run reads).
    """
    allowed = ["Read", "Edit", "Write", "Grep", "Glob", "mcp__agent-graft-ast__*"]
    tools = ["Read", "Edit", "Write", "Grep", "Glob"]
    if arm == "combo":
        # only ast-grep itself, by absolute path and subcommand: `sg` is "switch group" on Linux
        sg = shutil.which("ast-grep") or sys.exit("combo arm: ast-grep not found on PATH")
        allowed = ["Read", "Edit", "Write", "Grep", "Glob", "mcp__graft__*", f"Bash({sg} run:*)", f"Bash({sg} scan:*)"]
        tools.append("Bash")
    if task.get("test"):
        allowed.append(f"Bash({task['test']})")
        if "Bash" not in tools:
            tools.append("Bash")
    return [
        "claude", "-p", prompt,
        "--model", MODEL,
        "--output-format", "stream-json", "--verbose",
        "--setting-sources", "",
        "--settings", json.dumps({"autoMemoryEnabled": False} | ({"hooks": hooks} if hooks else {})),
        "--strict-mcp-config", "--mcp-config", mcp_config,
        "--disable-slash-commands",
        "--tools", ",".join(tools),
        "--allowedTools", ",".join(allowed),
        "--permission-mode", "acceptEdits",
        "--no-session-persistence",
        "--max-budget-usd", MAX_BUDGET_USD,
    ]


def suite_spent():
    total = 0.0
    for f in OUT.glob("*.json"):
        if f.name.endswith(".mcp.json"):
            continue
        try:
            total += float(json.loads(f.read_text()).get("cost_usd") or 0)
        except (ValueError, OSError):
            pass
    return total


def load_tasks():
    tasks = json.loads((HERE / "tasks.json").read_text())
    local = HERE / "local_tasks.json"
    if local.exists():
        tasks += json.loads(local.read_text())
    return {t["id"]: t for t in tasks}


GIT_SAFE = ["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-c", "core.attributesFile=/dev/null"]


def sh(cmd, cwd=None, check=True):
    """Harness commands; git runs without hooks, fsmonitor or system config the agent could plant."""
    env = None
    if cmd and cmd[0] == "git":
        cmd = ["git", *GIT_SAFE, *cmd[1:]]
        env = os.environ | {"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"}
    return subprocess.run(cmd, cwd=cwd, check=check, capture_output=True, text=True, env=env)


def work_dir(repo):
    return WORK / f"{repo}-work"


def prep(repos=None):
    for repo in repos or sorted({t["repo"] for t in load_tasks().values()}):
        dst = work_dir(repo)
        dst.mkdir(parents=True, exist_ok=True)
        fresh = not (dst / ".git").exists()
        if fresh:
            # Copy what git sees (tracked + untracked-not-ignored), minus the excludes:
            # skips worktrees, build output and caches that a plain rsync would drag in.
            src = source(repo)
            listed = sh(["git", "ls-files", "-co", "--exclude-standard", "-z"], cwd=src).stdout
            keep = [p for p in listed.split("\0") if p and (src / p).is_file()
                    and not any(fnmatch(part, e) for part in p.split("/") for e in RSYNC_EXCLUDES)
                    and not p.startswith(".herdr/")]
            subprocess.run(["rsync", "-a", "--from0", "--files-from=-", f"{src}/", f"{dst}/"],
                           input="\0".join(keep), text=True, check=True)
            sh(["git", "init", "-q"], cwd=dst)
            (dst / ".git" / "info").mkdir(exist_ok=True)
            (dst / ".git" / "info" / "exclude").write_text(".agent-graft/\n")
            sh(["git", "add", "-A"], cwd=dst)
            sh(["git", "-c", "user.name=bench", "-c", "user.email=bench@localhost",
                "commit", "-qm", "base", "--no-verify"], cwd=dst)
        print(f"{repo}: {dst} ({'created' if fresh else 'exists'})")


def reset(dst):
    sh(["git", "reset", "--hard", "-q"], cwd=dst)
    sh(["git", "clean", "-fdq"], cwd=dst)


# ---------- checks ----------

def source_files(root, exts, under=("",)):
    skip = {"node_modules", "dist", ".git", ".agent-graft", "target"}
    for base in under:
        for dirpath, dirnames, filenames in os.walk(root / base):
            dirnames[:] = [d for d in dirnames if d not in skip]
            for f in filenames:
                if f.endswith(exts):
                    yield Path(dirpath) / f


def call_args(text, open_idx):
    """Top-level arguments of the call whose '(' is at open_idx."""
    depth, i, start, args, quote = 0, open_idx, open_idx + 1, [], None
    while i < len(text):
        c = text[i]
        if quote:
            if c == "\\":
                i += 1
            elif c == quote:
                quote = None
        elif c in "\"'`":
            # Rust lifetimes ('a) are not strings.
            if not (c == "'" and re.match(r"'[A-Za-z_]\w*[^'\w]", text[i:i + 40] or "")):
                quote = c
        elif c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
            if depth == 0:
                args.append(text[start:i])
                return [a.strip() for a in args if a.strip()]
        elif c == "," and depth == 1:
            args.append(text[start:i])
            start = i + 1
        i += 1
    return None


def calls(root, name, exts, under=("",)):
    """(file, line, args) for every call of `name`, excluding its definition."""
    pat = re.compile(rf"(?<![\w.]){re.escape(name)}\s*\(")
    out = []
    for f in source_files(root, exts, under):
        text = f.read_text(errors="replace")
        for m in pat.finditer(text):
            before = text[max(0, m.start() - 12):m.start()]
            if re.search(r"(function|fn)\s+$", before):
                continue
            line = text.count("\n", 0, m.start()) + 1
            out.append((str(f.relative_to(root)), line, call_args(text, m.end() - 1)))
    return out


def definition_params(root, name, rel, kw):
    text = (root / rel).read_text()
    m = re.search(rf"{kw}\s+{re.escape(name)}\s*(<[^>]*>)?\s*\(", text)
    if not m:
        return None, text
    return call_args(text, m.end() - 1), text


def check_arity(root, name, exts, under, want, last_ok, min_calls):
    found = calls(root, name, exts, under)
    bad = [f"{f}:{l} args={a}" for f, l, a in found
           if a is None or len(a) != want or not last_ok(a[-1])]
    problems = []
    if len(found) < min_calls:
        problems.append(f"only {len(found)} call sites of {name} (expected >= {min_calls})")
    problems += [f"call site not updated: {b}" for b in bad]
    return problems, len(found)


def word_count(root, word, exts):
    pat = re.compile(rf"\b{re.escape(word)}\b")
    hits = Counter()
    for f in source_files(root, exts):
        n = len(pat.findall(f.read_text(errors="replace")))
        if n:
            hits[str(f.relative_to(root))] = n
    return hits


TS = (".ts", ".tsx", ".astro", ".js", ".mjs")


def check_ra_rename_find_struct_impl(root):
    rs = (".rs",)
    old = word_count(root, "find_struct_impl", rs)
    new = word_count(root, "find_impl_for_adt", rs)
    handlers = ["generate_new", "generate_function", "generate_delegate_methods",
                "generate_getter_or_setter", "generate_enum_is_method",
                "generate_enum_projection_method"]
    expect = ["crates/ide-assists/src/utils.rs"] + [
        f"crates/ide-assists/src/handlers/{h}.rs" for h in handlers]
    p = [f"old name left: {f} x{n}" for f, n in old.items()]
    p += [f"new name missing in {f}" for f in expect if f not in new]
    if new.total() < 16:
        p.append(f"only {new.total()} references to find_impl_for_adt (expected >= 16)")
    return p, {"new_name_refs": new.total()}


def check_ra_invert_flip_flag(root):
    rel = "crates/ide-assists/src/utils.rs"
    params, text = definition_params(root, "invert_boolean_expression", rel, "fn")
    p = []
    if not params or len(params) != 3 or not params[2].replace(" ", "").startswith("flip_comparisons:bool"):
        p.append(f"invert_boolean_expression params = {params}")
    sparams, _ = definition_params(root, "invert_special_case", rel, "fn")
    if not sparams or not any("flip_comparisons" in s or ": bool" in s for s in sparams):
        p.append(f"invert_special_case params = {sparams}")
    probs, n = check_arity(root, "invert_boolean_expression", (".rs",), ("crates",), 3,
                           lambda a: a == "true", 7)
    return p + probs, {"call_sites": n}


def check_ra_rename_huge(root):
    rs = (".rs",)
    old = word_count(root, "add_file_edits", rs)
    new = word_count(root, "push_file_edits", rs)
    p = [f"old name left: {f} x{n}" for f, n in old.items()]
    if new.total() < 133:
        p.append(f"only {new.total()} references to push_file_edits (expected >= 133)")
    return p, {"new_name_refs": new.total(), "files": len(new)}


def check_ra_add_field_goto_impl(root):
    rs = (".rs",)
    decl = (root / "crates/ide/src/goto_implementation.rs").read_text()
    body = decl[decl.find("pub struct GotoImplementationConfig"):]
    body = body[:body.find("}") + 1]
    p = [] if re.search(r"pub\s+include_blanket_impls\s*:\s*bool", body) else [f"field missing: {body[:120]!r}"]
    n = 0
    for f in source_files(root, rs):
        text = f.read_text(errors="replace")
        for m in re.finditer(r"\bGotoImplementationConfig\s*\{", text):
            before = text[max(0, m.start() - 11):m.start()].strip()
            if before.endswith("struct") or before.endswith("->"):
                continue
            n += 1
            lit = text[m.end():text.find("}", m.end())]
            if not re.search(r"include_blanket_impls\s*:\s*false", lit):
                p.append(f"literal not updated: {f.relative_to(root)}:{text[:m.start()].count(chr(10)) + 1}")
    if n < 4:
        p.append(f"only {n} literals found (expected >= 4)")
    return p, {"literals": n}


CHECKS = {
    "ra-rename-huge": check_ra_rename_huge,
    "ra-add-field-goto-impl": check_ra_add_field_goto_impl,
    "ra-rename-find-struct-impl": check_ra_rename_find_struct_impl,
    "ra-invert-flip-flag": check_ra_invert_flip_flag,
}


def _load_local():
    """Your own tasks on repos you can't publish: local_tasks.py (CHECKS) + local_tasks.json, both gitignored."""
    path = HERE / "local_tasks.py"
    if not path.exists():
        return
    import importlib.util
    sys.modules.setdefault("bench", sys.modules[__name__])
    spec = importlib.util.spec_from_file_location("local_tasks", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    CHECKS.update(mod.CHECKS)


_load_local()


def run_check(task_id, root):
    try:
        problems, info = CHECKS[task_id](root)
    except Exception as e:  # a crashed check is a failed check, not a crashed run
        problems, info = [f"check crashed: {e!r}"], {}
    return {"pass": not problems, "problems": problems[:20], **info}


# ---------- one run ----------

def parse_stream(path):
    tools, init, result, redirects = Counter(), {}, {}, 0
    for line in path.read_text().splitlines():
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        if e.get("type") == "system" and e.get("subtype") == "init":
            init = e
        elif e.get("type") == "assistant":
            for c in e.get("message", {}).get("content", []):
                if c.get("type") == "tool_use":
                    tools[c.get("name")] += 1
        elif e.get("type") == "user":
            for c in e.get("message", {}).get("content", []):
                if isinstance(c, dict) and c.get("type") == "tool_result" and REDIRECT_MARK in json.dumps(c.get("content")):
                    redirects += 1
        elif e.get("type") == "result":
            result = e
    return init, tools, result, redirects


def run_one(task_id, arm, n, smoke=False):
    task = load_tasks()[task_id]
    spent = suite_spent()
    if spent + float(MAX_BUDGET_USD) > SUITE_BUDGET_USD:
        sys.exit(f"skip {task_id} {arm} {n}: ${spent:.2f} spent, next run could pass the ${SUITE_BUDGET_USD:.0f} suite cap")
    root = work_dir(task["repo"])
    if not (root / ".git").exists():
        prep([task["repo"]])
    reset(root)
    OUT.mkdir(exist_ok=True)
    tag = f"{task_id}.{arm}.{n}" + (".smoke" if smoke else "")

    prompt = task["prompt"] + BOTH_ARMS_SUFFIX
    index_s = None
    hooks = None
    if arm in ("treatment", "ctxonly", "treatment-redirect"):
        db = root / ".agent-graft" / "index.redb"
        t0 = time.monotonic()
        sh([str(BIN), "--root", str(root), "--db", str(db), "index"])
        index_s = round(time.monotonic() - t0, 3)
        mcp = {"mcpServers": {"agent-graft-ast": {
            "command": str(BIN), "args": ["--root", str(root), "--db", str(db), "serve"]}}}
        if arm == "ctxonly":
            # same engine with rename/change_signature hidden: isolates what the edit tools add
            mcp["mcpServers"]["agent-graft-ast"]["env"] = {"AGENT_GRAFT_NO_REFACTOR": "1"}
        if arm == "treatment-redirect":
            # treatment + the PreToolUse hook `init` installs: symbol searches are answered from the index
            hooks = {"PreToolUse": [{"matcher": "Grep|Glob|Bash", "hooks": [{"type": "command", "timeout": 10,
                     "command": f"{BIN} --root {root} --db {db} hook pre-tool"}]}]}
        prompt += TREATMENT_SUFFIX
    elif arm == "combo":
        # graft's graph is built up front, as the treatment's index is, and timed the same way
        t0 = time.monotonic()
        sh([GRAFT, "build", str(root)])
        index_s = round(time.monotonic() - t0, 3)
        mcp = {"mcpServers": {"graft": {"command": GRAFT, "args": ["mcp", str(root)]}}}
        prompt += COMBO_SUFFIX
    elif arm == "baseline":
        mcp = {"mcpServers": {}}
    else:
        sys.exit(f"unknown arm {arm}")
    mcp_path = OUT / f"{tag}.mcp.json"
    mcp_path.write_text(json.dumps(mcp))

    env = {k: os.environ[k] for k in CLEAN_ENV_KEYS if k in os.environ} | EXTRA_ENV
    raw = OUT / f"{tag}.jsonl"
    err = OUT / f"{tag}.stderr"
    t0 = time.monotonic()
    timed_out = False
    with raw.open("w") as fo, err.open("w") as fe:
        try:
            proc = subprocess.run(claude_args(prompt, str(mcp_path), task, hooks, arm), cwd=root, env=env,
                                  stdin=subprocess.DEVNULL, stdout=fo, stderr=fe,
                                  timeout=TIMEOUT_S)
            rc = proc.returncode
        except subprocess.TimeoutExpired:
            rc, timed_out = None, True
    wall = round(time.monotonic() - t0, 3)

    init, tools, result, redirects = parse_stream(raw)
    usage = result.get("usage", {})
    parts = {k: usage.get(k, 0) for k in
             ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")}
    diff = sh(["git", "diff", "--numstat"], cwd=root, check=False).stdout.split("\n")
    rec = {
        "task": task_id, "repo": task["repo"], "arm": arm, "run": n, "smoke": smoke,
        "model_requested": MODEL, "model": init.get("model"),
        "mcp_servers": init.get("mcp_servers"), "tools_available": init.get("tools"),
        "exit_code": rc, "timed_out": timed_out, "is_error": result.get("is_error"),
        "subtype": result.get("subtype"),
        "wall_s": wall, "index_s": index_s,
        "duration_ms": result.get("duration_ms"), "duration_api_ms": result.get("duration_api_ms"),
        "num_turns": result.get("num_turns"),
        "tool_calls": sum(tools.values()), "tool_calls_by_name": dict(tools.most_common()),
        "redirects": redirects,
        "input_tokens_total": sum(parts.values()), "input_tokens_parts": parts,
        "output_tokens": usage.get("output_tokens", 0),
        "cost_usd": result.get("total_cost_usd"),
        "files_changed": [l.split("\t")[-1] for l in diff if l.strip()],
        "check": run_check(task_id, root),
        "final_message": (result.get("result") or "")[:2000],
    }
    (OUT / f"{tag}.json").write_text(json.dumps(rec, indent=2))
    (OUT / f"{tag}.patch").write_text(sh(["git", "diff"], cwd=root, check=False).stdout)  # keep the edit for compiling later
    reset(root)
    c = rec["check"]
    print(f"{tag}: {'PASS' if c['pass'] else 'FAIL'} wall={wall}s turns={rec['num_turns']} "
          f"tools={rec['tool_calls']} in={rec['input_tokens_total']} out={rec['output_tokens']} "
          f"${rec['cost_usd']}")
    if not c["pass"]:
        print("  " + "\n  ".join(c["problems"][:5]))
    return rec


# ---------- aggregate ----------

ARMS = ("baseline", "ctxonly", "combo", "treatment", "treatment-redirect")

METRICS = ["wall_s", "duration_ms", "num_turns", "tool_calls", "input_tokens_total",
           "output_tokens", "cost_usd", "redirects"]


def summarize(recs):
    s = {"runs": len(recs), "pass": sum(r["check"]["pass"] for r in recs)}
    for m in METRICS:
        vals = [r[m] for r in recs if r.get(m) is not None]
        s[m] = {"mean": round(statistics.mean(vals), 4) if vals else None,
                "median": round(statistics.median(vals), 4) if vals else None,
                "sum": round(sum(vals), 4) if vals else None}
    for part in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"):
        vals = [r["input_tokens_parts"][part] for r in recs]
        s[part] = {"mean": round(statistics.mean(vals), 1) if vals else None}
    tools = Counter()
    for r in recs:
        tools.update(r["tool_calls_by_name"])
    s["tool_calls_by_name_total"] = dict(tools.most_common())
    return s


def fmt(v, m):
    if v is None:
        return "-"
    if m == "cost_usd":
        return f"${v:.3f}"
    if m in ("wall_s",):
        return f"{v:.0f}s"
    if m == "duration_ms":
        return f"{v / 1000:.0f}s"
    return f"{v:,.0f}" if v >= 100 else f"{v:.1f}"


def aggregate():
    recs = [json.loads(p.read_text()) for p in sorted(OUT.glob("*.json"))
            if not p.name.endswith(".mcp.json")]
    if not os.environ.get("INCLUDE_SMOKE"):
        recs = [r for r in recs if not r.get("smoke")]
    tasks = load_tasks()
    by = {}
    for r in recs:
        by.setdefault(r["task"], {}).setdefault(r["arm"], []).append(r)
    res = {"model": sorted({r["model"] or r["model_requested"] for r in recs}),
           "tasks": {t: {a: summarize(rs) for a, rs in arms.items()} for t, arms in by.items()},
           "totals": {a: summarize([r for r in recs if r["arm"] == a])
                      for a in ARMS if any(r["arm"] == a for r in recs)}}
    (HERE / "results.json").write_text(json.dumps(res, separators=(",", ":")) + "\n")  # one line: reruns diff in a line or two

    rows = [("pass", "pass rate"), ("wall_s", "wall time"), ("duration_ms", "duration_ms"),
            ("num_turns", "turns"), ("tool_calls", "tool calls"),
            ("input_tokens_total", "input tokens"), ("output_tokens", "output tokens"),
            ("cost_usd", "cost")]

    def table(arms):
        """One column per arm present; deltas are against baseline."""
        names = [a for a in ARMS if arms.get(a)]
        b = arms.get("baseline")
        lines = ["| metric (mean) | " + " | ".join(names) + " |", "|---" * (len(names) + 1) + "|"]
        for m, label in rows:
            cells = []
            for a in names:
                s = arms[a]
                if m == "pass":
                    cells.append(f"{s['pass']}/{s['runs']}")
                    continue
                v = s[m]["mean"]
                d = f" ({(v - b[m]['mean']) / b[m]['mean'] * 100:+.0f}%)" if a != "baseline" and b and b[m]["mean"] else ""
                cells.append(f"{fmt(v, m)}{d}")
            lines.append(f"| {label} | " + " | ".join(cells) + " |")
        for part in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"):
            lines.append(f"| &nbsp;&nbsp;{part} | " + " | ".join(fmt(arms[a][part]["mean"], part) for a in names) + " |")
        used = [", ".join(f"{k} {v}" for k, v in arms[a].get("tool_calls_by_name_total", {}).items()) or "-" for a in names]
        lines.append("| tools used (all runs) | " + " | ".join(used) + " |")
        return "\n".join(lines)

    md = ["# Task benchmark results", "",
          f"Model: {', '.join(res['model'])}. Runs per arm are listed as pass x/n. "
          "Baseline = built-in tools only; ctxonly = built-ins + agent-graft-ast MCP without rename/change_signature; "
          "treatment = built-ins + the full agent-graft-ast MCP. "
          "Input tokens = input + cache_creation + cache_read. Index build time is excluded.", ""]
    for tid, arms in res["tasks"].items():
        md += [f"## {tid} ({tasks[tid]['repo']})", "", table(arms), ""]
    md += ["## Totals (all tasks)", "", table(res["totals"]), ""]
    (HERE / "RESULTS.md").write_text("\n".join(md))
    print(f"aggregated {len(recs)} runs -> results.json, RESULTS.md")


def main():
    cmd, *rest = sys.argv[1:] or ["help"]
    if cmd == "prep":
        prep(rest or None)
    elif cmd == "run":
        task_id, arm, n = rest[:3]
        run_one(task_id, arm, int(n), smoke="--smoke" in rest)
    elif cmd == "check":
        task = load_tasks()[rest[0]]
        print(json.dumps(run_check(rest[0], work_dir(task["repo"])), indent=2))
    elif cmd == "tasks":
        print(" ".join(load_tasks()))
    elif cmd == "aggregate":
        aggregate()
    else:
        print(__doc__)


if __name__ == "__main__":
    main()
