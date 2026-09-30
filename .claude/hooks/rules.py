#!/usr/bin/env python3
"""CLAUDE.md's rules as Claude Code hooks (wired in .claude/settings.json).

Mechanical rules are checked here, in code. Jev (ADR 0002) gets only the
judgments code cannot make: which skill and which specs/ADRs a prompt needs,
and whether a diff changed spec'd behaviour while the spec stayed put. Jev
never blocks a tool call, and every Jev path fails open: no key, a timeout or
a bad answer means the hook does nothing.

  JEV_HOOKS    on (default) | shadow (ask Jev and log, inject nothing) | off
  JEV_API_KEY  or TYPESAFE_API_KEY; unset = Jev paths are skipped
  JEV_API_URL  https://api.typesafe.ai (default) or https://openrouter.ai/api
               (same /v1/systemone contract, OpenRouter key)
  log          .claude/jev-hooks.log, one JSON line per Jev call. Tune the
               thresholds below against it; TypeSafe's own numbers are examples.

Self-check, no network: python3 .claude/hooks/rules.py --selftest
"""

import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import time
import urllib.request

ROOT = pathlib.Path(os.environ.get("CLAUDE_PROJECT_DIR") or pathlib.Path(__file__).resolve().parents[2])
MODE = os.environ.get("JEV_HOOKS", "on")
KEY = os.environ.get("JEV_API_KEY") or os.environ.get("TYPESAFE_API_KEY")
URL = os.environ.get("JEV_API_URL", "https://api.typesafe.ai") + "/v1/systemone"

SKILL_CONFIDENCE = 0.7  # Choice confidence before a skill is suggested
DOC_P = 0.7  # Noul p(yes) before a spec/ADR is pointed at
MAX_DOCS = 3
DRIFT_CONFIDENCE = 0.7  # Choice confidence before Stop is held for a stale spec
DIFF_CHARS = 20_000  # Jev degrades on irrelevant state; the limit is ~150k chars

CREATE_TABLE = re.compile(r"(?is)\bCREATE TABLE\s+(?:IF NOT EXISTS\s+)?(\w+)\s*\((.*?)\n\);")
TENANT_COLUMN = re.compile(r"(?i)\bworkspace_id\s+uuid\s+NOT NULL\b")  # nullable = cross-tenant (auth_links)
MIGRATION = re.compile(r"backend/migrations/\d{4}_[^/]+\.sql")
STRING_LITERAL = re.compile(r'r#"(.*?)"#|"((?:[^"\\]|\\.)*)"', re.S)
MODEL_PKG = (
    r"openai|async-openai|anthropic|@anthropic-ai/[\w.-]+|@ai-sdk/[\w.-]+|ai|langchain[\w.-]*|@langchain/[\w.-]+"
    r"|ollama[\w.-]*|llm|cohere[\w.-]*|mistralai|@mistralai/[\w.-]+|@google/genai|rig-core|fastembed|pgvector|candle-[\w-]+"
)
MODEL_DEP = re.compile(rf'(?m)^\s*"?(?:{MODEL_PKG})"?\s*[=:]')  # a Cargo.toml or package.json dependency line
MODEL_INSTALL = re.compile(
    rf"\b(?:cargo\s+add|npm\s+(?:i|install|add)|pnpm\s+add|yarn\s+add|bun\s+add)\b[^;&|\n]*?(?<![\w@/.-])(?:{MODEL_PKG})(?![\w/.-])"
)
MODEL_API = re.compile(
    r"api\.openai\.com|api\.anthropic\.com|generativelanguage\.googleapis\.com|api\.mistral\.ai|api\.cohere\.(?:ai|com)|openrouter\.ai"
)
RAW_HTML = re.compile(r"dangerouslySetInnerHTML|\.(?:inner|outer)HTML\s*=|insertAdjacentHTML|\bsrcdoc\b|document\.write\(")
INLINE_DISPOSITION = re.compile(r'(?i)content.disposition[^;\n]*\binline\b|"inline;')
ADR_ACCEPTED = re.compile(r"(?m)^Accepted\s*$")
SPEC_AGREED = re.compile(r"(?m)^Status:\s*Agreed\b")

MODEL_RULE = "Jev is the only model (ADR 0002). Another model vendor needs a new ADR first: run /adr and ask the user."


def decide(event, decision, reason):
    return {"hookSpecificOutput": {"hookEventName": event, "permissionDecision": decision, "permissionDecisionReason": reason}}


def introduced(pattern, new, before):
    """First match in `new` that is not already in `before` — so existing text never re-trips a rule."""
    return next((m.group(0) for m in pattern.finditer(new) if m.group(0) not in before), None)


def rel_path(path):
    rel = os.path.relpath(path or "/", ROOT)
    return None if rel.startswith("..") else rel


def rls_tables(sql):
    """Tables this SQL forces RLS on and gives a policy, directly or in 0001's FOREACH loop."""
    tables = [
        t for t in re.findall(r"(?i)ALTER TABLE (\w+) FORCE ROW LEVEL SECURITY", sql)
        if re.search(rf"(?i)CREATE POLICY \w+ ON {t}\b", sql)
    ]
    for names, body in re.findall(r"(?is)FOREACH\s+\w+\s+IN\s+ARRAY\s+ARRAY\[(.*?)\]\s+LOOP(.*?)END LOOP", sql):
        if re.search(r"(?i)FORCE ROW LEVEL SECURITY", body) and re.search(r"(?i)CREATE POLICY", body):
            tables += re.findall(r"'(\w+)'", names)
    return tables


def tenant_sql():
    """Tenant tables are the ones with RLS. The migrations are the list, so a new one joins by existing."""
    tables = {t for p in ROOT.glob("backend/migrations/*.sql") for t in rls_tables(p.read_text(errors="replace"))} - {"workspaces"}
    return re.compile(rf"\b(?:FROM|JOIN|INTO|UPDATE)\s+(?:{'|'.join(sorted(tables))})\b", re.I) if tables else None


def committed(rel):
    return subprocess.run(["git", "cat-file", "-e", f"HEAD:{rel}"], cwd=ROOT, capture_output=True, timeout=5).returncode == 0


def pre_tool_use(ev):
    tool, inp = ev.get("tool_name"), ev.get("tool_input") or {}
    if tool == "Bash":
        if m := MODEL_INSTALL.search(inp.get("command", "")):
            return decide("PreToolUse", "deny", f"`{m.group(0)}` adds a model SDK. {MODEL_RULE}")
        return None
    rel = rel_path(inp.get("file_path"))
    if not rel:
        return None
    file = ROOT / rel
    before = file.read_text(errors="replace") if file.is_file() else ""
    edits = inp.get("edits") or [inp]  # MultiEdit carries a list, Edit is one
    new = inp["content"] if "content" in inp else "\n".join(e.get("new_string", "") for e in edits)

    if file.name in ("Cargo.toml", "package.json") and (hit := introduced(MODEL_DEP, new, before)):
        return decide("PreToolUse", "deny", f"`{hit.strip()}` adds a model SDK. {MODEL_RULE}")
    if rel.startswith(("backend/", "frontend/")) and (hit := introduced(MODEL_API, new, before)):
        return decide("PreToolUse", "deny", f"`{hit}` calls another model's API. {MODEL_RULE}")
    if rel.startswith("frontend/") and (hit := introduced(RAW_HTML, new, before)):
        return decide("PreToolUse", "deny", f"`{hit}`: customer email is never rendered as HTML (CLAUDE.md, spec 002). Mail bodies are attacker-controlled; render them as text.")
    if rel.startswith("backend/src/") and introduced(INLINE_DISPOSITION, new, before):
        return decide("PreToolUse", "deny", "Attachments are always served as downloads, `Content-Disposition: attachment` (CLAUDE.md, spec 002).")
    if MIGRATION.fullmatch(rel) and before and committed(rel):
        return decide("PreToolUse", "deny", f"{rel} is committed, and sqlx checksums every applied migration: editing it breaks each database that already ran it. Write the change as the next numbered migration.")

    if re.fullmatch(r"docs/adr/\d{4}-[^/]+\.md", rel):
        if re.search(r"## Status\s+(Accepted|Superseded)", before):
            status_only = (
                tool == "Edit"
                and inp.get("old_string", "").strip() == "Accepted"
                and inp.get("new_string", "").strip().startswith("Superseded by")
            )
            if not status_only:
                return decide("PreToolUse", "deny", f"{rel} is accepted and ADRs are append-only (CLAUDE.md). The only allowed edit is its status to `Superseded by NNNN`; record the new decision with /adr.")
        if introduced(ADR_ACCEPTED, new, before):
            return decide("PreToolUse", "ask", f"Flipping {rel} to Accepted is the user's call (/adr).")
    if re.fullmatch(r"specs/\d{3}-[^/]+\.md", rel) and introduced(SPEC_AGREED, new, before):
        return decide("PreToolUse", "ask", f"Marking {rel} Agreed is the user's call: it unlocks implementation (/spec).")
    return None


def unscoped_sql(text):
    """SQL literals that touch a tenant table but never mention workspace_id."""
    hits, tenant = [], tenant_sql()
    for m in STRING_LITERAL.finditer(text):
        sql = m.group(1) or m.group(2) or ""
        if tenant and tenant.search(sql) and "workspace_id" not in sql:
            hits.append(f"line {text.count(chr(10), 0, m.start()) + 1}: {' '.join(sql.split())[:80]}")
    return hits


def missing_rls(sql):
    """Tables with a NOT NULL workspace_id that this migration creates without RLS and a policy,
    unless a `-- cross-tenant:` comment right above says why (sessions, jobs)."""
    protected = set(rls_tables(sql))
    return [
        m.group(1) for m in CREATE_TABLE.finditer(sql)
        if TENANT_COLUMN.search(m.group(2))
        and m.group(1) not in protected
        and "cross-tenant" not in sql[: m.start()].rsplit(";", 1)[-1].lower()
    ]


def post_tool_use(ev):
    # ponytail: string-literal scan. Misses SQL assembled with format!/push_str; RLS still backs it up.
    rel = rel_path((ev.get("tool_input") or {}).get("file_path"))
    if not (rel and (ROOT / rel).is_file()):
        return None
    if MIGRATION.fullmatch(rel) and (tables := missing_rls((ROOT / rel).read_text(errors="replace"))):
        return {
            "decision": "block",
            "reason": f"{rel} creates tenant table(s) {', '.join(tables)} without row-level security. Add `ALTER TABLE t ENABLE ROW LEVEL SECURITY`, "
            "`ALTER TABLE t FORCE ROW LEVEL SECURITY` and `CREATE POLICY tenant ON t USING (workspace_id = app_workspace_id())` in the same migration (ADR 0003), "
            "and extend backend/tests/rls.rs. If a table is deliberately cross-tenant (like sessions), put a `-- cross-tenant: <why>` comment right above it.",
        }
    if not (rel.startswith("backend/src/") and rel.endswith(".rs")):
        return None
    if hits := unscoped_sql((ROOT / rel).read_text(errors="replace")):
        return {
            "decision": "block",
            "reason": f"{rel}: SQL on a tenant table without a `workspace_id` filter. Every tenant query runs in `tenant_tx` and filters on `workspace_id` itself; RLS is the backstop, not the filter (ADR 0003). "
            + "; ".join(hits),
        }
    return None


def log(entry):
    try:
        with (ROOT / ".claude" / "jev-hooks.log").open("a") as f:
            f.write(json.dumps({"t": time.strftime("%Y-%m-%dT%H:%M:%S"), "mode": MODE, **entry}) + "\n")
    except OSError:
        pass


def jev(event, state, questions):
    """One Jev request, all questions fanned out in it. None on any failure: fail open."""
    if MODE == "off" or not KEY:
        return None
    body = json.dumps({"model": "jev-latest", "state": state, "questions": questions}).encode()
    req = urllib.request.Request(URL, body, {"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"})
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=4) as res:
            answers = json.load(res)["answers"]
    except Exception as e:  # an outage, a bad key or a new response shape must never block work
        log({"event": event, "error": str(e)[:300]})
        return None
    log({
        "event": event,
        "ms": round((time.monotonic() - t0) * 1000),
        "state": json.dumps(state)[:300],
        "answers": {k: {f: a[f] for f in ("choice", "noul", "confidence") if f in a} for k, a in answers.items()},
    })
    return answers


def frontmatter_description(text):
    m = re.search(r"(?m)^description:\s*(.+)$", text)
    return m.group(1).strip() if m else text.strip().splitlines()[0]


def section(text, heading):
    """First paragraph under `## heading`."""
    m = re.search(rf"(?m)^## {heading}\s*\n+((?:.+\n?)+)", text)
    return " ".join(m.group(1).split())[:400] if m else ""


def doc_roster():
    """(stem, path, title, gist) for every spec and ADR. The docs are the roster, so a new one joins by existing."""
    docs = []
    for p in sorted([*ROOT.glob("specs/[0-9]*.md"), *ROOT.glob("docs/adr/[0-9]*.md")]):
        text = p.read_text(errors="replace")
        gist = section(text, "Problem") if p.parent.name == "specs" else section(text, "Decision")
        docs.append((p.stem, str(p.relative_to(ROOT)), text.splitlines()[0].lstrip("# ").strip(), gist))
    return docs


def user_prompt_submit(ev):
    # ponytail: Jev sees the prompt alone, so "yes, do it" routes to nothing. Add the last
    # assistant message from transcript_path if short follow-ups turn out to matter.
    prompt = (ev.get("prompt") or "").strip()
    if len(prompt) < 15 or prompt.startswith("/"):
        return None
    skills = {p.parent.name: frontmatter_description(p.read_text()) for p in sorted(ROOT.glob(".claude/skills/*/SKILL.md"))}
    docs = doc_roster()
    questions = {
        "skill": {
            "type": "choice",
            "instructions": "A developer sent this request to their coding agent. Which of the agent's skills should it load before starting?",
            "criteria": {**skills, "none": "No skill fits: a question, a bug fix or refactor inside existing specs, tests, docs or chores."},
        },
        **{
            f"doc:{stem}": {
                "type": "noul",
                "instructions": {
                    "question": "Will doing this request change, depend on, or risk violating what this project document records?",
                    "document": {"title": title, "gist": gist},
                },
                "criteria": {
                    "true": "The request is about the area this document governs.",
                    "false": "The document is unrelated to the request.",
                },
            }
            for stem, _, title, gist in docs
        },
    }
    answers = jev("UserPromptSubmit", {"request": prompt[:8000]}, questions)
    if not answers:
        return None
    lines = []
    s = answers.get("skill") or {}
    if s.get("choice") in skills and s.get("confidence", 0) >= SKILL_CONFIDENCE:
        lines.append(f"The `{s['choice']}` skill likely applies; load it unless it clearly does not (Jev, confidence {s['confidence']:.2f}).")
    paths = {f"doc:{stem}": path for stem, path, _, _ in docs}
    ranked = sorted(((a.get("noul", 0), paths[k]) for k, a in answers.items() if k in paths), reverse=True)
    if picked := [f"{path} ({p:.2f})" for p, path in ranked if p >= DOC_P][:MAX_DOCS]:
        lines.append("Governing docs; read them before changing what they cover: " + ", ".join(picked))
    if not lines or MODE != "on":
        return None
    return {"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": "<jev_routing>\n" + "\n".join(lines) + "\n</jev_routing>"}}


def git(*args):
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, timeout=5).stdout


def stop(ev):
    if ev.get("stop_hook_active"):
        return None
    status = [(line[:2], line[3:]) for line in git("status", "--porcelain", "--untracked-files=all").splitlines()]
    code = [p for _, p in status if p.startswith(("backend/src/", "backend/migrations/", "frontend/src/"))]
    if not code or any(p.startswith("specs/") for _, p in status):
        return None
    diff = git("diff", "HEAD", "--", *code)
    for flag, p in status:  # untracked files are not in `git diff`
        if flag == "??" and p in code and len(diff) < DIFF_CHARS:
            diff += f"\n+++ new file {p}\n" + (ROOT / p).read_text(errors="replace")
    specs = {stem: f"{title}: {gist}" for stem, path, title, gist in doc_roster() if path.startswith("specs/")}
    questions = {
        "spec": {
            "type": "choice",
            "instructions": "These uncommitted changes were made to a help desk's code. Whose described behaviour or HTTP contract do they change?",
            "criteria": {
                **specs,
                "none": "No described behaviour or contract changes: a refactor, tests, performance, or a fix that makes the code do what its spec already says.",
            },
        }
    }
    answers = jev("Stop", {"changed_files": code, "diff": diff[:DIFF_CHARS]}, questions)
    a = (answers or {}).get("spec") or {}
    if a.get("choice") not in specs or a.get("confidence", 0) < DRIFT_CONFIDENCE or MODE != "on":
        return None
    seen = pathlib.Path(tempfile.gettempdir()) / f"muninn-jev-stop-{ev.get('session_id', 'none')}"
    if a["choice"] in (seen.read_text().split() if seen.exists() else []):
        return None  # asked once this session; the answer was given or the user moved on
    with seen.open("a") as f:
        f.write(a["choice"] + "\n")
    return {
        "decision": "block",
        "reason": f"The uncommitted code looks like it changes behaviour in specs/{a['choice']}.md, and specs/ is untouched (Jev, confidence {a['confidence']:.2f}). "
        "The spec changes with the code, in the same commit (CLAUDE.md). Update it, or tell the user why it does not need to change.",
    }


HANDLERS = {"PreToolUse": pre_tool_use, "PostToolUse": post_tool_use, "UserPromptSubmit": user_prompt_submit, "Stop": stop}


def selftest():
    global ROOT, jev
    ROOT = pathlib.Path(tempfile.mkdtemp())
    (ROOT / "docs/adr").mkdir(parents=True)
    (ROOT / "specs").mkdir()
    (ROOT / "docs/adr/0002-jev.md").write_text("# 2. Jev is the only model\n\n## Status\n\nAccepted\n\n## Decision\n\nJev only.\n")
    (ROOT / "specs/002-mail.md").write_text("# 002. Tickets and email\n\nStatus: Draft\n\n## Problem\n\nMail comes in.\n")

    def pre(tool, **inp):
        out = pre_tool_use({"tool_name": tool, "tool_input": inp})
        return out and out["hookSpecificOutput"]["permissionDecision"]

    p = lambda rel: str(ROOT / rel)
    assert pre("Bash", command="npm i -D @ai-sdk/openai") == "deny"
    assert pre("Bash", command="cargo add openai") == "deny"
    assert pre("Bash", command="npm install axios email-validator") is None
    assert pre("Edit", file_path=p("backend/Cargo.toml"), old_string="", new_string='async-openai = "0.28"') == "deny"
    assert pre("Edit", file_path=p("frontend/package.json"), old_string="", new_string='    "ai": "^5.0.0",') == "deny"
    assert pre("Write", file_path=p("frontend/src/Mail.tsx"), content="<div dangerouslySetInnerHTML={{__html: body}} />") == "deny"
    assert pre("Write", file_path=p("frontend/src/Mail.tsx"), content="<pre>{body}</pre>") is None
    assert pre("Edit", file_path=p("backend/src/tickets.rs"), old_string="", new_string='format!("inline; filename=\\"{n}\\"")') == "deny"
    assert pre("Edit", file_path=p("docs/adr/0002-jev.md"), old_string="Jev only.", new_string="Jev and Claude.") == "deny"
    assert pre("Edit", file_path=p("docs/adr/0002-jev.md"), old_string="Accepted", new_string="Superseded by 0011") is None
    assert pre("Write", file_path=p("docs/adr/0011-new.md"), content="# 11. X\n\n## Status\n\nAccepted\n") == "ask"
    assert pre("Edit", file_path=p("specs/002-mail.md"), old_string="Status: Draft", new_string="Status: Agreed") == "ask"
    assert pre("Edit", file_path=p("specs/002-mail.md"), old_string="Mail comes in.", new_string="Mail arrives.") is None

    (ROOT / "backend/migrations").mkdir(parents=True)
    table = "CREATE TABLE tickets (\n    id uuid PRIMARY KEY,\n    workspace_id uuid NOT NULL REFERENCES workspaces\n);\n"
    rls = "ALTER TABLE tickets ENABLE ROW LEVEL SECURITY;\nALTER TABLE tickets FORCE ROW LEVEL SECURITY;\nCREATE POLICY tenant ON tickets USING (workspace_id = app_workspace_id());\n"
    (ROOT / "backend/migrations/0001_init.sql").write_text(table + rls)
    jobs = "-- cross-tenant: the worker runs outside any tenant\nCREATE TABLE jobs (\n    workspace_id uuid NOT NULL REFERENCES workspaces\n);\n"
    assert missing_rls(table) == ["tickets"] and missing_rls(table + rls + jobs) == []
    assert missing_rls(jobs.split("\n", 1)[1]) == ["jobs"]
    loop = "FOREACH t IN ARRAY ARRAY['agents', 'contacts'] LOOP\n EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);\n EXECUTE format('CREATE POLICY tenant ON %I USING (true)', t);\nEND LOOP;"
    assert rls_tables(loop) == ["agents", "contacts"] and rls_tables(table + rls) == ["tickets"]
    assert missing_rls(table + rls.rsplit("CREATE POLICY", 1)[0]) == ["tickets"]  # forced but no policy: sees nothing, not protected
    new_migration = p("backend/migrations/0005_new.sql")
    assert pre("Write", file_path=new_migration, content=table) is None  # not on disk yet, not committed
    assert pre("Edit", file_path=p("backend/migrations/0001_init.sql"), old_string="uuid", new_string="text") is None  # uncommitted
    git("init", "-q")
    git("add", "backend/migrations")
    git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init")
    assert pre("Edit", file_path=p("backend/migrations/0001_init.sql"), old_string="uuid", new_string="text") == "deny"

    assert unscoped_sql('sqlx::query("SELECT id FROM tickets WHERE id = $1")')
    assert not unscoped_sql('sqlx::query("SELECT id FROM tickets WHERE workspace_id = $1 AND id = $2")')
    assert not unscoped_sql('sqlx::query(r#"SELECT token FROM auth_links WHERE id = $1"#)')

    jev = lambda *_: {"skill": {"choice": "spec", "confidence": 0.9}, "doc:0002-jev": {"noul": 0.8}, "doc:002-mail": {"noul": 0.2}}
    (ROOT / ".claude/skills/spec").mkdir(parents=True)
    (ROOT / ".claude/skills/spec/SKILL.md").write_text("---\ndescription: Write a spec first\n---\n")
    ctx = user_prompt_submit({"prompt": "add a reply-drafting feature using an LLM"})["hookSpecificOutput"]["additionalContext"]
    assert "`spec` skill" in ctx and "docs/adr/0002-jev.md" in ctx and "002-mail" not in ctx, ctx
    print("rules.py selftest ok")


def main():
    if sys.argv[1:] == ["--selftest"]:
        return selftest()
    try:
        ev = json.load(sys.stdin)
    except ValueError:
        return
    handler = HANDLERS.get(ev.get("hook_event_name"))
    if handler and (out := handler(ev)):
        print(json.dumps(out))


if __name__ == "__main__":
    main()
