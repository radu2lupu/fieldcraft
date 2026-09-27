#!/usr/bin/env python3
"""Evaluate graph-linked memory and skill retrieval through real MCP tools.

This is intentionally more than a happy-path smoke test:
- seeds several unrelated domains into an isolated Fieldcraft DB;
- adds distractor lessons and skills that should not be retrieved;
- checks positive, paraphrased, and negative queries;
- validates typed 1-hop links, link ordering, preview-only retrieval, and reveal tools.
"""
from __future__ import annotations

import argparse
import json
import os
import sqlite3
import subprocess
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]


@dataclass(frozen=True)
class DomainCase:
    id: str
    project_id: str
    skill_name: str
    skill_description: str
    skill_body: str
    primary_title: str
    primary_summary: str
    primary_steps: list[str]
    files: list[str]
    warning_title: str
    warning_statement: str
    demo_title: str
    demo_summary: str
    query_context: str
    query_task: str
    paraphrase_contexts: list[tuple[str, str]]
    hard_negative_contexts: list[tuple[str, str]]
    negative_context: str
    negative_task: str


CASES = [
    DomainCase(
        id="django-orm",
        project_id="django-django",
        skill_name="django-orm-query-expressions",
        skill_description="Use for Django ORM bugs involving QuerySet composition, expressions, output fields, joins, annotations, and SQL compiler behavior.",
        skill_body="Start from the failing ORM test. Classify expression resolution, join promotion, SQL compiler rendering, or result conversion. Verify with focused queries, expressions, or annotations tests.",
        primary_title="Django QuerySet expression triage",
        primary_summary="Use this when a Django task involves QuerySet expressions, annotations, output fields, joins, or compiler behavior.",
        primary_steps=[
            "Run the focused failing ORM test.",
            "Classify the failure as expression resolution, join promotion, compiler rendering, or result conversion.",
            "Patch the smallest source location and rerun the focused test.",
        ],
        files=["django/db/models/sql/query.py", "django/db/models/expressions.py"],
        warning_title="Django migration router gotcha",
        warning_statement="Migration router/contenttypes failures are distinct from runtime QuerySet expression bugs.",
        demo_title="Prior QuerySet cloning fix",
        demo_summary="A prior task fixed QuerySet cloning by tracing expression reuse through compiler setup.",
        query_context="Django QuerySet annotation expression output_field compiler bug",
        query_task="Fix a Django ORM expression regression and verify focused query tests.",
        paraphrase_contexts=[
            ("Django annotations produce incorrect SQL when expressions are reused in a QuerySet", "Triage the ORM compiler path and add the smallest focused regression test."),
            ("An ORM regression appears only when combining Q objects and annotated expressions", "Find the expression-resolution bug without changing migration code."),
            ("query.py expression output field failure", "Use the prior Django ORM debugging path."),
        ],
        hard_negative_contexts=[
            ("Django migration router contenttypes failure during migrate", "Fix migration routing, not runtime QuerySet expressions."),
            ("Django template context processor renders missing variable", "Debug request context rendering."),
        ],
        negative_context="React hydration mismatch in a Next.js route",
        negative_task="Fix client component state synchronization.",
    ),
    DomainCase(
        id="swiftui-animation",
        project_id="apple-app",
        skill_name="swiftui-animation-transitions",
        skill_description="Use for SwiftUI animation, transition, matchedGeometryEffect, phase animation, and view identity bugs.",
        skill_body="Inspect state identity first. Isolate whether the issue is transition placement, matched geometry namespace, transaction animation, or unstable ForEach identity. Verify on simulator with reduced motion considered.",
        primary_title="SwiftUI animation identity triage",
        primary_summary="Use this for SwiftUI animation glitches caused by unstable identity, transition placement, or matched geometry misuse.",
        primary_steps=[
            "Identify the state mutation that should trigger the animation.",
            "Check stable IDs and namespace ownership before changing timing curves.",
            "Verify in a small preview or simulator flow.",
        ],
        files=["Sources/App/Views/AnimatedListView.swift"],
        warning_title="SwiftUI timing curve red herring",
        warning_statement="Animation duration changes rarely fix identity bugs; inspect view identity before tuning curves.",
        demo_title="Prior matched geometry namespace fix",
        demo_summary="A prior task fixed a broken hero animation by moving namespace ownership above conditional branches.",
        query_context="SwiftUI matchedGeometryEffect transition flickers when rows reorder",
        query_task="Fix a native SwiftUI animation glitch without changing unrelated layout.",
        paraphrase_contexts=[
            ("Animated SwiftUI list jumps instead of smoothly transitioning after state changes", "Debug view identity and transition placement."),
            ("matched geometry hero transition breaks after a conditional branch changes", "Inspect namespace ownership and stable IDs."),
            ("SwiftUI reorder animation flicker", "Use animation identity triage."),
        ],
        hard_negative_contexts=[
            ("SwiftUI LazyVStack scroll performance drops on large feeds", "Optimize rendering performance, not animation identity."),
            ("SwiftUI NavigationSplitView selection is lost after window restore", "Fix navigation state restoration."),
        ],
        negative_context="Django migration SQL generated for contenttypes is incorrect",
        negative_task="Fix migration router behavior.",
    ),
    DomainCase(
        id="cloudflare-do",
        project_id="cloudflare-worker",
        skill_name="cloudflare-durable-object-sqlite",
        skill_description="Use for Cloudflare Durable Objects, RPC methods, alarms, WebSockets, SQLite storage, and stateful Worker coordination.",
        skill_body="Model the Durable Object as the authority for state. Keep request validation in the Worker, use DO RPC for coordination, persist durable state in SQLite when needed, and test with wrangler dev.",
        primary_title="Durable Object state coordination triage",
        primary_summary="Use this when a Worker bug depends on Durable Object state, alarms, WebSockets, or SQLite-backed coordination.",
        primary_steps=[
            "Identify the object namespace and routing key.",
            "Separate edge request validation from object-owned state transitions.",
            "Verify with wrangler dev or focused unit tests around the object method.",
        ],
        files=["src/worker.ts", "src/room.ts", "wrangler.jsonc"],
        warning_title="Worker KV consistency gotcha",
        warning_statement="KV is eventually consistent and should not replace Durable Object authority for live coordination.",
        demo_title="Prior room alarm scheduling fix",
        demo_summary="A prior task fixed room cleanup by moving alarm scheduling into the Durable Object.",
        query_context="Cloudflare Worker Durable Object loses chat room state across WebSocket messages",
        query_task="Fix state coordination in a Durable Object and validate with wrangler.",
        paraphrase_contexts=[
            ("A Workers app has a stateful room object whose SQLite-backed data is not updated correctly", "Patch the Durable Object method rather than edge routing."),
            ("Durable Object WebSocket room loses participant state between messages", "Debug object-owned state transition."),
            ("cloudflare room sqlite coordination bug", "Use Durable Object state triage."),
        ],
        hard_negative_contexts=[
            ("Cloudflare Pages static asset cache header is wrong", "Fix CDN cache headers."),
            ("Cloudflare Worker KV value is stale after write in another colo", "Handle KV consistency semantics."),
        ],
        negative_context="Stripe subscription invoice proration incorrect after plan upgrade",
        negative_task="Fix billing behavior.",
    ),
    DomainCase(
        id="stripe-billing",
        project_id="payments-api",
        skill_name="stripe-subscription-proration",
        skill_description="Use for Stripe subscription updates, invoice previews, proration behavior, billing cycles, Checkout, and PaymentIntent integration decisions.",
        skill_body="Confirm whether the flow uses Checkout, Billing subscription update APIs, or PaymentIntents. Preview invoices before changing proration behavior. Verify webhook idempotency and test clock scenarios.",
        primary_title="Stripe subscription proration triage",
        primary_summary="Use this when a billing task touches subscription updates, invoice previews, proration, or webhook idempotency.",
        primary_steps=[
            "Identify the Stripe API surface used by the flow.",
            "Preview the invoice before changing proration flags.",
            "Verify webhook idempotency and billing-cycle expectations.",
        ],
        files=["app/billing/stripe.ts", "app/api/webhooks/stripe/route.ts"],
        warning_title="Stripe webhook replay gotcha",
        warning_statement="Subscription fixes must preserve idempotent webhook handling because events can be replayed.",
        demo_title="Prior invoice preview fix",
        demo_summary="A prior task fixed upgrade pricing by comparing upcoming invoice previews before applying updates.",
        query_context="Stripe subscription upgrade charges wrong proration amount and webhook repeats",
        query_task="Fix subscription billing logic and verify invoice preview behavior.",
        paraphrase_contexts=[
            ("A billing API updates plans but the upcoming invoice and webhook side effects are wrong", "Triage Stripe subscription update and idempotency."),
            ("Subscription plan change generates an unexpected upcoming invoice", "Inspect proration behavior before patching."),
            ("stripe upgrade invoice preview mismatch", "Use billing proration triage."),
        ],
        hard_negative_contexts=[
            ("Stripe Checkout session redirect URL is malformed", "Fix checkout success and cancel URL construction."),
            ("Stripe PaymentIntent requires 3DS confirmation on client", "Debug PaymentIntent confirmation flow."),
        ],
        negative_context="Cloudflare Durable Object alarm does not fire for inactive room cleanup",
        negative_task="Fix Worker state cleanup.",
    ),
    DomainCase(
        id="github-ci",
        project_id="repo-ci",
        skill_name="github-actions-ci-triage",
        skill_description="Use for GitHub Actions CI failures, workflow logs, matrix jobs, failing checks, artifact inspection, and minimal PR fixes.",
        skill_body="Inspect failing check runs and logs first. Reproduce locally when possible. Patch the smallest failing surface, rerun the focused command, and summarize changed behavior for the PR.",
        primary_title="GitHub Actions failing check triage",
        primary_summary="Use this when a task requires debugging failing GitHub Actions checks or CI workflow behavior.",
        primary_steps=[
            "Identify the failing check and relevant log excerpt.",
            "Map the log to the local command or workflow step.",
            "Patch the smallest issue and rerun the focused validation command.",
        ],
        files=[".github/workflows/ci.yml", "package.json"],
        warning_title="CI cache red herring",
        warning_statement="Do not assume cache corruption until the failing command and dependency versions are inspected.",
        demo_title="Prior matrix node version fix",
        demo_summary="A prior task fixed CI by aligning the matrix Node version with package engine requirements.",
        query_context="GitHub Actions CI matrix fails only on node 22 with package engine mismatch",
        query_task="Debug a failing PR check and make the smallest workflow or package fix.",
        paraphrase_contexts=[
            ("A pull request has one failing GitHub check and the workflow log points at the runtime version", "Triage the CI log and patch the workflow."),
            ("Actions matrix passes locally but fails for one Node version", "Inspect CI runtime and package metadata."),
            ("github ci check log engine mismatch", "Use failing check triage."),
        ],
        hard_negative_contexts=[
            ("GitHub issue labels need triage and project assignment", "Organize issues, not CI workflow failures."),
            ("GitHub release notes need generation from merged pull requests", "Write changelog content."),
        ],
        negative_context="SwiftUI matched geometry animation flickers during list reorder",
        negative_task="Fix app animation identity.",
    ),
]


DISTRACTOR_LESSONS = [
    ("Postgres vacuum tuning", "Tune autovacuum thresholds for write-heavy tables.", ["db/postgres.conf"]),
    ("React hydration mismatch triage", "Separate server-rendered markup from client-only state.", ["app/page.tsx"]),
    ("Terraform provider pinning", "Pin provider versions and inspect generated plans.", ["main.tf"]),
    ("Kubernetes probe tuning", "Distinguish readiness, liveness, and startup probe failures.", ["deployment.yaml"]),
    ("Python packaging metadata", "Fix pyproject build-system and wheel metadata issues.", ["pyproject.toml"]),
    ("Rust lifetime diagnostic", "Narrow borrow checker failures around ownership boundaries.", ["src/lib.rs"]),
    ("Elasticsearch analyzer config", "Debug token filters, analyzers, and index mappings.", ["schema.json"]),
    ("S3 multipart upload retry", "Handle retryable upload parts and checksum validation.", ["storage.ts"]),
]


def rpc(proc: subprocess.Popen, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
    assert proc.stdin is not None
    assert proc.stdout is not None
    rpc.counter += 1
    request = {"jsonrpc": "2.0", "id": rpc.counter, "method": method}
    if params is not None:
        request["params"] = params
    proc.stdin.write(json.dumps(request) + "\n")
    proc.stdin.flush()
    line = proc.stdout.readline()
    if not line:
        raise RuntimeError("MCP server closed stdout")
    response = json.loads(line)
    if "error" in response:
        raise RuntimeError(f"RPC error: {response['error']}")
    return response["result"]


rpc.counter = 0


def call_tool(proc: subprocess.Popen, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    result = rpc(proc, "tools/call", {"name": name, "arguments": arguments})
    content = result["content"][0]["text"]
    return json.loads(content)


def write_skill_fixture(root: Path, case: DomainCase) -> Path:
    skill_dir = root / case.skill_name
    skill_dir.mkdir(parents=True, exist_ok=True)
    (skill_dir / "SKILL.md").write_text(
        f"""---
name: {case.skill_name}
description: {case.skill_description}
---
# {case.skill_name}

## When to use
{case.skill_description}

## Procedure
{case.skill_body}
""",
        encoding="utf-8",
    )
    return skill_dir


def write_distractor_skill(root: Path, name: str, description: str) -> Path:
    skill_dir = root / name
    skill_dir.mkdir(parents=True, exist_ok=True)
    (skill_dir / "SKILL.md").write_text(
        f"""---
name: {name}
description: {description}
---
# {name}

## Procedure
{description}
""",
        encoding="utf-8",
    )
    return skill_dir


def ingest_skill(fieldcraft_home: Path, skill_dir: Path) -> None:
    env = os.environ.copy()
    env["FIELDCRAFT_HOME"] = str(fieldcraft_home)
    subprocess.run(
        ["cargo", "run", "-q", "-p", "fieldcraft-cli", "--", "skills", "ingest", "--source", str(skill_dir)],
        cwd=ROOT,
        env=env,
        check=True,
        capture_output=True,
        text=True,
        timeout=120,
    )


def skill_card_id(fieldcraft_home: Path, skill_name: str) -> str:
    with sqlite3.connect(fieldcraft_home / "memory.sqlite3") as conn:
        row = conn.execute("select id from skill_cards where skill_name = ?", (skill_name,)).fetchone()
    if not row:
        raise RuntimeError(f"skill card not ingested: {skill_name}")
    return str(row[0])


def start_server(fieldcraft_home: Path) -> subprocess.Popen:
    env = os.environ.copy()
    env["FIELDCRAFT_HOME"] = str(fieldcraft_home)
    return subprocess.Popen(
        ["cargo", "run", "-q", "-p", "fieldcraft-cli", "--", "serve"],
        cwd=ROOT,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )


def write_lesson(
    proc: subprocess.Popen,
    *,
    project_id: str,
    kind: str,
    title: str,
    payload: dict[str, Any],
    links: list[dict[str, Any]] | None = None,
) -> str:
    args: dict[str, Any] = {
        "project_id": project_id,
        "kind": kind,
        "title": title,
        "payload": payload,
    }
    if links is not None:
        args["links"] = links
    result = call_tool(proc, "lesson_write", args)
    return str(result["id"])


def seed_case(proc: subprocess.Popen, case: DomainCase, card_id: str) -> dict[str, Any]:
    warn_id = write_lesson(
        proc,
        project_id=case.project_id,
        kind="claim",
        title=case.warning_title,
        payload={"statement": case.warning_statement, "scope": case.primary_title, "confidence": 0.9},
    )
    demo_id = write_lesson(
        proc,
        project_id=case.project_id,
        kind="procedure",
        title=case.demo_title,
        payload={"summary": case.demo_summary, "steps": case.primary_steps[:2], "files": case.files[:1]},
        links=[{"target_id": card_id, "edge_type": "REQUIRES", "confidence": 0.7}],
    )
    primary_id = write_lesson(
        proc,
        project_id=case.project_id,
        kind="procedure",
        title=case.primary_title,
        payload={"summary": case.primary_summary, "steps": case.primary_steps, "files": case.files},
        links=[
            {"target_id": card_id, "edge_type": "REQUIRES", "confidence": 1.0},
            {"target_id": warn_id, "edge_type": "WARNS_ABOUT", "confidence": 0.9},
            {"target_id": demo_id, "edge_type": "DEMONSTRATES", "confidence": 0.8},
        ],
    )
    return {
        "case": case,
        "primary_id": primary_id,
        "skill_card_id": card_id,
        "expected_links": [warn_id, card_id, demo_id],
        "expected_edge_order": ["WARNS_ABOUT", "REQUIRES", "DEMONSTRATES"],
        "second_hop_from": demo_id,
        "expected_second_hop": [card_id],
    }


def seed_distractors(proc: subprocess.Popen) -> list[str]:
    ids = []
    for idx, (title, summary, files) in enumerate(DISTRACTOR_LESSONS):
        ids.append(
            write_lesson(
                proc,
                project_id=f"distractor-{idx}",
                kind="procedure",
                title=title,
                payload={
                    "summary": summary,
                    "steps": ["Inspect the failing evidence.", "Patch the smallest relevant surface."],
                    "files": files,
                },
            )
        )
    return ids


def retrieve_lesson(proc: subprocess.Popen, project_id: str, context: str, task: str, files: list[str]) -> dict[str, Any]:
    return call_tool(
        proc,
        "lesson_retrieve",
        {"project_id": project_id, "context": context, "task": task, "files": files},
    )


def score_positive(case_row: dict[str, Any], retrieve_result: dict[str, Any], links_result: dict[str, Any]) -> dict[str, Any]:
    lessons = retrieve_result.get("lessons") or []
    primary = lessons[0] if lessons else {}
    retrieve_links = primary.get("links") or []
    explicit_links = links_result.get("links") or []
    returned_id = primary.get("id")
    if returned_id == case_row["second_hop_from"]:
        expected = set(case_row["expected_second_hop"])
        expected_order = ["REQUIRES"]
    else:
        expected = set(case_row["expected_links"])
        expected_order = case_row["expected_edge_order"]
    retrieve_ids = [link.get("id") for link in retrieve_links if link.get("id")]
    explicit_ids = [link.get("id") for link in explicit_links if link.get("id")]
    edge_order = [link.get("edge_type") for link in retrieve_links]
    acceptable_entrypoints = {case_row["primary_id"], case_row["second_hop_from"]}
    return {
        "primary_returned": returned_id,
        "primary_correct": returned_id == case_row["primary_id"],
        "entrypoint_correct": returned_id in acceptable_entrypoints,
        "retrieve_link_ids": retrieve_ids,
        "lesson_links_ids": explicit_ids,
        "retrieve_link_precision": len(set(retrieve_ids) & expected) / len(retrieve_ids) if retrieve_ids else 0.0,
        "retrieve_link_recall": len(set(retrieve_ids) & expected) / len(expected),
        "lesson_links_precision": len(set(explicit_ids) & expected) / len(explicit_ids) if explicit_ids else 0.0,
        "lesson_links_recall": len(set(explicit_ids) & expected) / len(expected),
        "edge_order": edge_order,
        "edge_order_correct": edge_order == expected_order,
        "raw_content_in_retrieve": "content" in primary or any(link.get("raw_content_returned") for link in retrieve_links),
        "retrieve_result": retrieve_result,
        "links_result": links_result,
    }


def score_type_filters(case_row: dict[str, Any], filtered: dict[str, dict[str, Any]]) -> dict[str, Any]:
    expected_by_type = {
        "WARNS_ABOUT": {case_row["expected_links"][0]},
        "REQUIRES": {case_row["expected_links"][1]},
        "DEMONSTRATES": {case_row["expected_links"][2]},
    }
    results: dict[str, Any] = {}
    all_ok = True
    for edge_type, payload in filtered.items():
        ids = {link.get("id") for link in payload.get("links") or [] if link.get("id")}
        expected = expected_by_type[edge_type]
        ok = ids == expected and all(link.get("edge_type") == edge_type for link in payload.get("links") or [])
        results[edge_type] = {"ids": sorted(ids), "expected": sorted(expected), "ok": ok}
        all_ok = all_ok and ok
    results["all_ok"] = all_ok
    return results


def score_second_hop(case_row: dict[str, Any], links_result: dict[str, Any]) -> dict[str, Any]:
    ids = [link.get("id") for link in links_result.get("links") or [] if link.get("id")]
    expected = set(case_row["expected_second_hop"])
    return {
        "from": case_row["second_hop_from"],
        "ids": ids,
        "precision": len(set(ids) & expected) / len(ids) if ids else 0.0,
        "recall": len(set(ids) & expected) / len(expected),
        "ok": set(ids) == expected,
        "links_result": links_result,
    }


def reveal_checks(proc: subprocess.Popen, project_id: str, links: list[dict[str, Any]]) -> dict[str, Any]:
    lesson_ok = 0
    skill_ok = 0
    failures: list[str] = []
    for link in links:
        if link.get("kind") == "skill":
            result = call_tool(
                proc,
                "skill_reveal",
                {
                    "skill_card_id": link["id"],
                    "project_id": project_id,
                    "reason": "linked skill matches the seeded positive graph case",
                },
            )
            card = result.get("skill_card") or {}
            if card.get("procedure") and not result.get("raw_skill_body_returned"):
                skill_ok += 1
            else:
                failures.append(f"bad skill reveal: {link['id']}")
        else:
            result = call_tool(
                proc,
                "lesson_reveal",
                {"lesson_id": link["id"], "reason": "linked lesson matches the seeded positive graph case"},
            )
            lesson = result.get("lesson") or {}
            if lesson.get("content") and not result.get("raw_archive_returned"):
                lesson_ok += 1
            else:
                failures.append(f"bad lesson reveal: {link['id']}")
    return {"lesson_reveals_ok": lesson_ok, "skill_reveals_ok": skill_ok, "reveal_failures": failures}


def metric(value: Any) -> str:
    if isinstance(value, float):
        return f"{value:.3f}"
    return str(value)


def aggregate(rows: list[dict[str, Any]], negatives: list[dict[str, Any]]) -> dict[str, Any]:
    positives = [row for row in rows if row["query_type"].startswith("positive")]
    direct = [row for row in positives if row["query_type"] == "positive-direct"]
    return {
        "positive_query_count": len(positives),
        "negative_query_count": len(negatives),
        "entrypoint_accuracy": sum(1 for row in positives if row["entrypoint_correct"]) / len(positives),
        "direct_primary_accuracy": sum(1 for row in direct if row["primary_correct"]) / len(direct),
        "primary_accuracy": sum(1 for row in positives if row["primary_correct"]) / len(positives),
        "retrieve_link_precision": sum(row["retrieve_link_precision"] for row in positives) / len(positives),
        "retrieve_link_recall": sum(row["retrieve_link_recall"] for row in positives) / len(positives),
        "lesson_links_precision": sum(row["lesson_links_precision"] for row in positives) / len(positives),
        "lesson_links_recall": sum(row["lesson_links_recall"] for row in positives) / len(positives),
        "edge_order_accuracy": sum(1 for row in positives if row["edge_order_correct"]) / len(positives),
        "raw_retrieve_content_violations": sum(1 for row in positives if row["raw_content_in_retrieve"]),
        "negative_empty_rate": sum(1 for row in negatives if row["empty"]) / len(negatives),
        "negative_false_positive_count": sum(0 if row["empty"] else 1 for row in negatives),
        "reveal_failure_count": sum(len(row["reveal"].get("reveal_failures", [])) for row in rows if "reveal" in row),
        "type_filter_accuracy": sum(1 for row in positives if row.get("type_filters", {}).get("all_ok")) / len(positives),
        "second_hop_accuracy": sum(1 for row in positives if row.get("second_hop", {}).get("ok")) / len(positives),
    }


def render_report(summary: dict[str, Any], rows: list[dict[str, Any]], negatives: list[dict[str, Any]]) -> str:
    lines = [
        "# Graph Link Retrieval Eval",
        "",
        "This eval uses an isolated Fieldcraft DB and the real MCP stdio tools. It seeds multiple domains plus distractors, then checks direct retrieval, paraphrases, hard negatives, typed links, type-filtered links, one explicit second hop, ordering, and reveal flow.",
        "",
        "Edge order oracle: link previews are expected in semantic priority order `WARNS_ABOUT`, `REQUIRES`, `DEMONSTRATES`, matching the repository sort contract. This deliberately prioritizes hazards before prerequisites before examples.",
        "",
        "| metric | value |",
        "| --- | ---: |",
    ]
    for key, value in summary.items():
        lines.append(f"| {key} | {metric(value)} |")
    lines.extend(["", "## Positive Cases", "", "| case | query | entrypoint | primary | link precision | link recall | order |", "| --- | --- | ---: | ---: | ---: | ---: | ---: |"])
    for row in rows:
        lines.append(
            f"| {row['case_id']} | {row['query_type']} | {row['entrypoint_correct']} | {row['primary_correct']} | {metric(row['retrieve_link_precision'])} | {metric(row['retrieve_link_recall'])} | {row['edge_order_correct']} |"
        )
    lines.extend(["", "## Negative Cases", "", "| case | query | empty | returned |", "| --- | --- | ---: | --- |"])
    for row in negatives:
        returned = ",".join(row["returned_ids"])
        lines.append(f"| {row['case_id']} | {row['query_type']} | {row['empty']} | {returned} |")
    lines.extend(["", "## Trace", "", "Full trace is in `graph_link_trace.json`.", ""])
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out-dir", type=Path, default=ROOT / "eval/skills/graph_link_eval_v1")
    args = parser.parse_args()
    args.out_dir.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="fieldcraft-graph-eval-") as tmp:
        tmp_path = Path(tmp)
        fieldcraft_home = tmp_path / "fieldcraft-home"
        fieldcraft_home.mkdir()
        skills_root = tmp_path / "skills"
        card_ids: dict[str, str] = {}
        for case in CASES:
            ingest_skill(fieldcraft_home, write_skill_fixture(skills_root, case))
            card_ids[case.id] = skill_card_id(fieldcraft_home, case.skill_name)
        for idx, (title, description, _files) in enumerate(DISTRACTOR_LESSONS):
            ingest_skill(
                fieldcraft_home,
                write_distractor_skill(skills_root, f"distractor-skill-{idx}", f"{title}: {description}"),
            )

        proc = start_server(fieldcraft_home)
        try:
            rpc(proc, "initialize", {})
            case_rows = [seed_case(proc, case, card_ids[case.id]) for case in CASES]
            seed_distractors(proc)
            time.sleep(0.1)

            positive_rows: list[dict[str, Any]] = []
            negative_rows: list[dict[str, Any]] = []
            for case_row in case_rows:
                case: DomainCase = case_row["case"]
                positive_queries = [("positive-direct", case.query_context, case.query_task)]
                positive_queries.extend(
                    (f"positive-paraphrase-{idx + 1}", context, task)
                    for idx, (context, task) in enumerate(case.paraphrase_contexts)
                )
                for query_type, context, task in positive_queries:
                    retrieve_result = retrieve_lesson(proc, case.project_id, context, task, case.files[:1])
                    returned_lessons = retrieve_result.get("lessons") or []
                    returned_id = (returned_lessons[0] if returned_lessons else {}).get("id") or case_row["primary_id"]
                    links_result = call_tool(proc, "lesson_links", {"lesson_id": returned_id, "limit": 5})
                    scored = score_positive(case_row, retrieve_result, links_result)
                    scored.update({"case_id": case.id, "query_type": query_type})
                    filtered = {
                        edge_type: call_tool(
                            proc,
                            "lesson_links",
                            {"lesson_id": case_row["primary_id"], "limit": 5, "edge_types": [edge_type]},
                        )
                        for edge_type in ["WARNS_ABOUT", "REQUIRES", "DEMONSTRATES"]
                    }
                    scored["type_filters"] = score_type_filters(case_row, filtered)
                    second_hop = call_tool(
                        proc,
                        "lesson_links",
                        {"lesson_id": case_row["second_hop_from"], "limit": 5, "edge_types": ["REQUIRES"]},
                    )
                    scored["second_hop"] = score_second_hop(case_row, second_hop)
                    if query_type == "positive-direct":
                        scored["reveal"] = reveal_checks(proc, case.project_id, links_result.get("links") or [])
                    positive_rows.append(scored)

                negative_queries = [("off-domain", case.negative_context, case.negative_task)]
                negative_queries.extend(
                    (f"hard-negative-{idx + 1}", context, task)
                    for idx, (context, task) in enumerate(case.hard_negative_contexts)
                )
                for negative_type, context, task in negative_queries:
                    negative_result = retrieve_lesson(
                        proc,
                        case.project_id,
                        context,
                        task,
                        ["unrelated/file.txt"],
                    )
                    returned_ids = [lesson.get("id") for lesson in negative_result.get("lessons") or [] if lesson.get("id")]
                    negative_rows.append(
                        {
                            "case_id": case.id,
                            "query_type": negative_type,
                            "empty": len(returned_ids) == 0,
                            "returned_ids": returned_ids,
                            "retrieve_result": negative_result,
                        }
                    )
        finally:
            if proc.stdin:
                proc.stdin.close()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()

    trace = {"summary": aggregate(positive_rows, negative_rows), "positives": positive_rows, "negatives": negative_rows}
    (args.out_dir / "graph_link_trace.json").write_text(json.dumps(trace, indent=2, sort_keys=True, default=str) + "\n")
    (args.out_dir / "graph_link_report.md").write_text(render_report(trace["summary"], positive_rows, negative_rows) + "\n")
    print(args.out_dir / "graph_link_report.md")

    required = {
        "entrypoint_accuracy": 1.0,
        "direct_primary_accuracy": 1.0,
        "retrieve_link_precision": 1.0,
        "retrieve_link_recall": 1.0,
        "lesson_links_precision": 1.0,
        "lesson_links_recall": 1.0,
        "edge_order_accuracy": 1.0,
        "raw_retrieve_content_violations": 0,
        "negative_empty_rate": 1.0,
        "reveal_failure_count": 0,
        "type_filter_accuracy": 1.0,
        "second_hop_accuracy": 1.0,
    }
    failures = [f"{key} expected {want}, got {trace['summary'].get(key)}" for key, want in required.items() if trace["summary"].get(key) != want]
    if failures:
        print("\n".join(failures))
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
