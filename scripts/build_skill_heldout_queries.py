#!/usr/bin/env python3
"""Build a frozen held-out skill retrieval eval from corpus records.

The output is not a substitute for independent human labeling. It is a larger
corpus-derived holdout designed to catch broad regressions without reusing the
small hand-tuned development query set.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import random
import re
from pathlib import Path


STOP_PHRASES = (
    "use when",
    "use this skill when",
    "this skill should be used when",
    "load when",
    "trigger when",
)


ABSTAIN_QUERIES = [
    'What is the capital of Romania? Answer directly without using a specialized workflow.',
    'Compute 144 divided by 12 and return only the number.',
    'Tell me whether the word accomodate is spelled correctly.',
    'Summarize this note in one sentence: The build passed. The deploy is scheduled.',
    'Rename a local variable in a tiny helper function; no external docs or special tooling are needed.',
    'Make a folder named presentations in the filesystem, but do not create slides or a deck.',
    'The word worker appears in this sentence, but I only need a synonym for employee.',
    'Should I stripe a deck of cards before playing? No payment integration is involved.',
    'Convert the phrase hello world to title case.',
    'Answer with a short definition of photosynthesis.',
    'Count the letters in banana and return only the number.',
    'Explain what HTTP stands for in one sentence.',
    'Rewrite this sentence to be more concise: I am writing to ask if it might be possible to meet tomorrow.',
    'Sort these words alphabetically: pear, apple, orange.',
    'What day comes after Tuesday?',
    'Translate bonjour to English.',
    'Give me a synonym for fast.',
    'Is 29 a prime number?',
    'Return the lowercase version of FIELDCRAFT.',
    'What is 2 to the power of 8?',
    'Convert GOOD MORNING to lowercase.',
    'Make this sentence title case: fieldcraft skill retrieval eval.',
    'Return the uppercase version of hello there.',
    'Alphabetize these names: Zoe, Ana, Mihai.',
    'How many vowels are in the word retrieval?',
    'What is the plural of analysis?',
    'Give an antonym for expensive.',
    'What is the past tense of run?',
    "Is 'their' the right word in: their going home?",
    'Fix the typo in: seperate concerns.',
    'Evaluate 18 plus 27.',
    'What is 15 percent of 200?',
    'Round 3.14159 to two decimals.',
    'Convert 5 kilometers to meters.',
    'What is half of 96?',
    'Return true if 10 is greater than 7.',
    'What is the next number in the sequence 2, 4, 6, 8?',
    'How many days are in February in a non-leap year?',
    'What color do you get by mixing red and blue?',
    'Name the largest ocean on Earth.',
    'Who wrote Pride and Prejudice? Answer directly.',
    'What does CPU stand for?',
    'Define cache in one sentence.',
    'What is JSON? Keep it short.',
    'Explain recursion in one sentence.',
    'What is the chemical symbol for water?',
    'Which planet is known as the red planet?',
    'What language is mostly spoken in Brazil?',
    'What currency is used in Japan?',
    'What time zone is Berlin in during winter?',
    'Extract the email address from: Contact me at user@example.com tomorrow.',
    'Extract the phone number from: Call +1 555 123 4567.',
    'Return only the domain from https://example.com/path.',
    'Remove extra spaces from this sentence: hello    world.',
    "Split 'a,b,c' by commas and list the items.",
    'Count words in: agents retrieve compact skill cards.',
    'Does this sentence contain the word apple: I bought an orange?',
    'Replace cats with dogs in: cats are cute.',
    'Reverse the string abcdef.',
    'Return the first three characters of Fieldcraft.',
    'Tell me if this is valid JSON: {"a":1}.',
    'What is the file extension in report.pdf?',
    'What is the basename of /tmp/example.txt?',
    'Join these words with hyphens: skill retrieval eval.',
    'Remove punctuation from: hello, world!',
    'Make this slug: Skill Intelligence MVP.',
    'What is the length of the word benchmark?',
    'Complete this sentence with a common phrase: better late than ___.',
    'Give me three rhymes for light.',
    'Write a one-line apology for being late.',
    'Draft a short thank-you note for a meeting.',
    'Make this sentence more polite: send me the file now.',
    'Shorten this phrase: at this point in time.',
    'Generate a simple project name for a notes app.',
    'Suggest a better variable name for x that stores user count.',
    'Should this sentence end with a question mark: Are you coming',
    'Add a comma where needed: After dinner we walked home.',
    'Return only yes or no: is the sky usually blue?',
    'Classify sentiment as positive, negative, or neutral: I love this tool.',
    'Classify sentiment: This is frustrating and slow.',
    'Pick the odd one out: apple, pear, carrot, banana.',
    'What is a common abbreviation for tablespoon?',
    'Convert 24-hour time 18:30 to 12-hour time.',
    'What month number is September?',
    'What is tomorrow after Monday?',
    'Return the initials for Ada Lovelace.',
    'Format 1234567 with commas.',
    'What is the opposite of enable?',
    'Name one common HTTP status code for not found.',
    'What does URL stand for?',
    'Summarize in five words: The deployment failed because the database migration timed out.',
    'Turn this into a bullet: Review logs before deploying.',
    'Make a checklist with two items: build and test.',
    'What is the singular of criteria?',
    'What is the comparative form of good?',
    'Is this a complete sentence: Because the test failed.',
    'Find the noun in: The agent retrieved cards.',
    'What part of speech is quickly?',
    'Return a regex-free answer: how many digits are in 20260507?',
    'Say whether 2026 is a leap year.',
    'What is the Roman numeral for 4?',
    'Convert binary 1010 to decimal.',
    'What is the square root of 81?',
    'What is 7 factorial?',
    'Solve for x: x + 3 = 10.',
    'What is the average of 2, 4, and 9?',
    'Return the minimum: 12, 3, 7.',
    'Return the maximum: 12, 3, 7.',
    'What is the median of 1, 5, 9?',
    'Does 42 divide evenly by 7?',
    'What is 100 minus 37?',
    'Convert one hour to minutes.',
    'How many centimeters are in a meter?',
    'What is freezing point of water in Celsius?',
    'What is the capital of France?',
    'What is the capital of Germany?',
    'What is the capital of Italy?',
    'Who painted the Mona Lisa?',
    'What gas do plants absorb from the air?',
    'What is H2O commonly called?',
    'What is the third letter of alphabet?',
    'Put these in chronological order: 2020, 1999, 2010.',
    'Return the last word in: compact cards help routing.',
    'What is the first word in: skill intelligence works.',
]


HARD_TECHNICAL_ABSTAINS = [
    "How do I handle rate limiting in an Express API? Give general advice, no project-specific workflow.",
    "What's the best pattern for React state management in a medium app? Answer conceptually.",
    "My webpack config is broken; explain common causes at a high level without using a specialized tool.",
    "How should I structure environment variables in a Node service? Give a short general answer.",
    "What are common ways to paginate a REST API?",
    "How do I avoid SQL injection in a generic web app?",
    "What is a reasonable retry strategy for transient HTTP errors?",
    "How should I name branches in git for feature work?",
    "What is the difference between unit and integration tests?",
    "How do I choose between npm and pnpm in general?",
    "What does idempotency mean for a backend endpoint?",
    "How do I think about caching API responses conceptually?",
    "What is a migration in a database context?",
    "How should I split a large function in general?",
    "What are common accessibility checks for a form?",
    "What is the difference between authentication and authorization?",
    "How do I debug a generic 500 error? Give a checklist only.",
    "What are common causes of flaky tests?",
    "How do I write a useful pull request description?",
    "What should I include in a basic README?",
]

NEAR_MISS_QUERIES = [
    {
        "query": "Configure Turborepo remote caching and turbo.json outputs for a multi-package JavaScript monorepo; do not switch to Nx or GitLab CI.",
        "expected": ["turborepo-caching"],
    },
    {
        "query": "Write Bats tests for Bash scripts with fixtures and setup/teardown; this is not Jest, pytest, or generic JavaScript testing.",
        "expected": ["bats-testing-patterns"],
    },
    {
        "query": "Use Cloudflare Sandbox SDK for secure execution of untrusted AI-generated code with containers and code interpreter APIs, not Durable Objects chat state.",
        "expected": ["sandbox-sdk"],
    },
    {
        "query": "Create a brand-first standalone marketing landing page after interviewing for colors, typography, shape language, and CTA; not an app UI component task.",
        "expected": ["brand-landingpage"],
    },
    {
        "query": "Tune SwiftUI matchedGeometryEffect and transition timing for a card animation; not a Metal shader or general view refactor.",
        "expected": ["swiftui-animation"],
    },
    {
        "query": "Optimize SwiftUI scrolling performance by auditing body recomputation and state updates; not a visual animation polish task.",
        "expected": ["swiftui-performance-audit"],
    },
    {
        "query": "Build a custom Metal fragment shader and wire it into SwiftUI shader effects; not normal SwiftUI animation timing.",
        "expected": ["metal-shader-expert"],
    },
    {
        "query": "Inspect a failing GitHub Actions check on a PR and use logs to identify the CI failure; not a generic PR comment addressing task.",
        "expected": ["gh-fix-ci"],
        "acceptable": ["github:gh-fix-ci"],
    },
    {
        "query": "Address unresolved GitHub PR review threads by making requested code changes; not debugging a failing CI job.",
        "expected": ["github:gh-address-comments"],
    },
    {
        "query": "Find a group meeting slot across several connected calendars; not a daily agenda brief or meeting prep summary.",
        "expected": ["google-calendar:google-calendar-group-scheduler"],
    },
    {
        "query": "Prepare a one-day calendar agenda with conflicts and free windows; not scheduling a new group meeting.",
        "expected": ["google-calendar:google-calendar-daily-brief"],
    },
    {
        "query": "Edit exact ranges and formulas in a connected Google Sheet; not a Google Doc, Slide deck, or Drive file search task.",
        "expected": ["google-drive:google-sheets"],
    },
    {
        "query": "Build a Cloudflare Durable Object for stateful coordination; not just general Workers best-practices review.",
        "expected": ["cloudflare:durable-objects"],
    },
    {
        "query": "Review Cloudflare Worker code for bindings, floating promises, globals, streaming, secrets, and observability; not building an Agent SDK app.",
        "expected": ["cloudflare:workers-best-practices"],
    },
    {
        "query": "Analyze Core Web Vitals and render-blocking resources for page load performance; not generic Cloudflare Worker deployment.",
        "expected": ["cloudflare:web-perf"],
    },
    {
        "query": "Set up Stripe subscription billing, webhooks, and customer portal behavior; not a generic payment word or unrelated checkout copy.",
        "expected": ["stripe:stripe-best-practices"],
    },
    {
        "query": "Create a PowerPoint deck and render slide visuals for review; not a Canva social resize or Google Docs task.",
        "expected": ["presentations:Presentations"],
        "acceptable": ["google-slides", "pptx"],
    },
    {
        "query": "Render a PDF visually and inspect page layout; not editing a Word document or spreadsheet.",
        "expected": ["pdf"],
        "acceptable": ["documents", "documents:documents"],
    },
    {
        "query": "Create an Excel workbook with formulas and charts from CSV data; not a Google Sheet connector task.",
        "expected": ["spreadsheets:Spreadsheets"],
    },
    {
        "query": "Generate a reusable Codex skill with frontmatter and trigger guidance; not install an existing skill from GitHub.",
        "expected": ["skill-creator"],
    },
]


def normalize_space(text: str) -> str:
    return re.sub(r"\s+", " ", text).strip()


def strip_skill_name(text: str, skill_name: str) -> str:
    variants = {
        skill_name,
        skill_name.replace("-", " "),
        skill_name.replace("_", " "),
        skill_name.split(":")[-1],
        skill_name.split(":")[-1].replace("-", " "),
        skill_name.split(":")[-1].replace("_", " "),
    }
    result = text
    for variant in sorted(variants, key=len, reverse=True):
        if not variant:
            continue
        pattern = r"(?<![A-Za-z0-9])" + re.escape(variant) + r"(?![A-Za-z0-9])"
        result = re.sub(pattern, "the relevant workflow", result, flags=re.IGNORECASE)
    return normalize_space(result)


def clean_description(description: str) -> str:
    text = normalize_space(description)
    text = re.sub(r"^[>|][-+]?\s*", "", text)
    text = normalize_space(text)
    lowered = text.lower()
    for phrase in STOP_PHRASES:
        if lowered.startswith(phrase):
            text = text[len(phrase):].lstrip(" :—-")
            break
    text = re.sub(r"\bUse\b", "Work", text, count=1)
    return text.rstrip(".")


def load_records(path: Path) -> list[dict]:
    records: list[dict] = []
    with path.open() as f:
        for line in f:
            if line.strip():
                records.append(json.loads(line))
    return records


def stable_sample(records: list[dict], limit: int, seed: str) -> list[dict]:
    rng = random.Random(int(hashlib.sha256(seed.encode()).hexdigest()[:16], 16))
    by_name: dict[str, dict] = {}
    for record in records:
        name = record.get("skill_name")
        description = clean_description(record.get("description", ""))
        if len(description.split()) < 8:
            continue
        if name and name not in by_name:
            by_name[name] = record
    values = list(by_name.values())
    rng.shuffle(values)
    return values[:limit]


def build_positive_query(record: dict, idx: int) -> dict:
    skill_name = record["skill_name"]
    description = clean_description(record.get("description", ""))
    triggers = [normalize_space(t) for t in record.get("triggers", []) if normalize_space(t)]
    trigger_hint = ""
    if triggers:
        trigger_hint = " The task involves " + triggers[0].rstrip(".") + "."
    query = (
        "I need help with a real project task: "
        f"{description}.{trigger_hint} "
        "Pick the most relevant compact workflow card for this task."
    )
    query = strip_skill_name(query, skill_name)
    return {
        "id": f"holdout-{idx:04d}",
        "query": query,
        "expected": [skill_name],
        "acceptable": [],
        "label_source": "deterministic_corpus_holdout",
        "review_status": "frozen_unreviewed",
    }


INDEPENDENT_POSITIVE_QUERIES = [
    {"query": "This giant SwiftUI screen has networking, mutations, and layout all mixed together. Help me split it without changing behavior.", "expected": ["ohapotato-view-service-splitting"], "acceptable": ["swiftui-view-refactor", "build-ios-apps:swiftui-view-refactor"]},
    {"query": "The app opens in Simulator but shows a blank screen. I need build/run/log inspection, not just code reading.", "expected": ["ios-debugger-agent"], "acceptable": ["build-ios-apps:ios-debugger-agent"]},
    {"query": "Scrolling a SwiftUI list stutters whenever the model updates. Find the view recomputation or state problem.", "expected": ["swiftui-performance-audit"], "acceptable": ["build-ios-apps:swiftui-performance-audit"]},
    {"query": "The card-to-detail transition snaps instead of flowing. Tune the matched geometry and transition behavior.", "expected": ["swiftui-animation"]},
    {"query": "I need a fragment shader effect in an Apple app and want the Metal pipeline reviewed.", "expected": ["metal-shader-expert"]},
    {"query": "Adopt the translucent iOS 26 look in this SwiftUI navigation stack and make sure the API usage is right.", "expected": ["swiftui-liquid-glass"], "acceptable": ["build-ios-apps:swiftui-liquid-glass"]},
    {"query": "Check the latest official OpenAI guidance for Responses API usage and cite sources.", "expected": ["openai-docs"]},
    {"query": "Open localhost, fill the login form, click through the flow, and tell me where the UI breaks.", "expected": ["playwright"], "acceptable": ["browser-use:browser", "browser"]},
    {"query": "The thing I need to inspect is a macOS menu popover, so capture the actual desktop window.", "expected": ["screenshot"]},
    {"query": "Render this contract PDF page by page and check whether layout or clauses look wrong.", "expected": ["pdf"], "acceptable": ["documents", "documents:documents"]},
    {"query": "Modify this Word file but preserve formatting, then render it to verify pages did not shift.", "expected": ["doc"], "acceptable": ["documents", "documents:documents", "docx"]},
    {"query": "Turn this CSV into an Excel file with formulas, charts, and formatting.", "expected": ["spreadsheets:Spreadsheets"], "acceptable": ["xlsx", "Spreadsheets"]},
    {"query": "Build an investor update slide deck and iterate on rendered slide quality.", "expected": ["presentations:Presentations"], "acceptable": ["google-slides", "pptx"]},
    {"query": "A PR check is red. Pull the CI logs and figure out the failing command.", "expected": ["gh-fix-ci"], "acceptable": ["github:gh-fix-ci"]},
    {"query": "There are PR review threads asking for changes. Find them and patch only those issues.", "expected": ["github:gh-address-comments"]},
    {"query": "Look through Gmail and bucket messages by urgent, reply soon, waiting, and FYI.", "expected": ["gmail:gmail-inbox-triage"], "acceptable": ["gmail:gmail"]},
    {"query": "Find times next week when all four attendees are free and rank the options.", "expected": ["google-calendar:google-calendar-group-scheduler"], "acceptable": ["google-calendar:google-calendar"]},
    {"query": "Give me tomorrow's calendar agenda with conflicts, free blocks, and prep notes.", "expected": ["google-calendar:google-calendar-daily-brief"], "acceptable": ["google-calendar:google-calendar-meeting-prep"]},
    {"query": "Open the connected spreadsheet, inspect the tab, clean the table, and update exact cell ranges.", "expected": ["google-drive:google-sheets"]},
    {"query": "Build a Cloudflare chat agent with persistent coordination and WebSockets.", "expected": ["cloudflare:agents-sdk"], "acceptable": ["cloudflare:building-ai-agent-on-cloudflare", "cloudflare:durable-objects"]},
    {"query": "Review this Worker for production mistakes around bindings, promises, globals, and observability.", "expected": ["cloudflare:workers-best-practices"], "acceptable": ["cloudflare:cloudflare"]},
    {"query": "Profile page speed and Core Web Vitals and explain render-blocking resources.", "expected": ["cloudflare:web-perf"]},
    {"query": "Set up recurring Stripe billing with webhook signature verification and portal behavior.", "expected": ["stripe:stripe-best-practices"], "acceptable": ["stripe-integration"]},
    {"query": "Write App Store release notes from git history since the last tag, only user-visible changes.", "expected": ["app-store-changelog"]},
    {"query": "Draft a PRD with goals, non-goals, personas, requirements, and success metrics.", "expected": ["prd-writer"]},
    {"query": "Do a source-backed research report comparing competitors and open questions.", "expected": ["deep-research"]},
    {"query": "Create a reusable Codex instruction package with frontmatter and trigger guidance.", "expected": ["skill-creator"]},
    {"query": "Install a Codex skill from a GitHub repo path and verify it is available.", "expected": ["skill-installer"]},
    {"query": "Generate a short Sora clip, poll it, and download the resulting video.", "expected": ["sora"]},
    {"query": "Make a new AI-generated hero image asset for the product page.", "expected": ["imagegen"]},
    {"query": "Configure Turborepo remote cache and turbo.json outputs for faster monorepo builds.", "expected": ["turborepo-caching"]},
    {"query": "Write Bats unit tests for Bash scripts with fixtures and setup hooks.", "expected": ["bats-testing-patterns"]},
    {"query": "Use Cloudflare Sandbox SDK to execute untrusted generated code in isolated containers.", "expected": ["sandbox-sdk"]},
    {"query": "Interview me for brand adjectives, color direction, typography, and CTA before generating a landing page.", "expected": ["brand-landingpage"]},
    {"query": "Set up Cedar policy checks and signed receipts for Claude Code tool calls.", "expected": ["protect-mcp-setup"]},
    {"query": "Choose and tune embedding models and chunking strategy for a RAG app.", "expected": ["embedding-strategies"]},
    {"query": "Implement document Q&A over proprietary files with retrieval-augmented generation.", "expected": ["rag-implementation"]},
    {"query": "Write an ADR explaining the database choice and consequences.", "expected": ["architecture-decision-records"]},
    {"query": "Implement mTLS for service-to-service communication in a zero-trust setup.", "expected": ["mtls-configuration"]},
    {"query": "Package a Python CLI for PyPI with pyproject.toml and release metadata.", "expected": ["python-packaging"]},
]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--positives", type=int, default=120)
    parser.add_argument("--abstains", type=int, default=20)
    parser.add_argument("--seed", default="fieldcraft-skill-heldout-v1")
    args = parser.parse_args()

    records = stable_sample(load_records(args.corpus), args.positives, args.seed)
    queries = [build_positive_query(record, i + 1) for i, record in enumerate(records)]
    abstain_pool = (ABSTAIN_QUERIES + HARD_TECHNICAL_ABSTAINS)[: args.abstains]
    for i, query in enumerate(abstain_pool, start=len(queries) + 1):
        queries.append({
            "id": f"holdout-{i:04d}",
            "query": query,
            "should_abstain": True,
            "acceptable": [],
            "label_source": "manual_adversarial_holdout",
            "review_status": "reviewed",
        })

    for extra_idx, row in enumerate(NEAR_MISS_QUERIES, start=len(queries) + 1):
        queries.append({
            "id": f"near-miss-{extra_idx:04d}",
            "query": row["query"],
            "expected": row["expected"],
            "acceptable": row.get("acceptable", []),
            "label_source": "manual_near_miss_holdout",
            "review_status": "reviewed",
        })

    for extra_idx, row in enumerate(INDEPENDENT_POSITIVE_QUERIES, start=len(queries) + 1):
        queries.append({
            "id": f"independent-{extra_idx:04d}",
            "query": row["query"],
            "expected": row["expected"],
            "acceptable": row.get("acceptable", []),
            "label_source": "manual_independent_holdout",
            "review_status": "reviewed",
        })

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w") as f:
        for query in queries:
            f.write(json.dumps(query, ensure_ascii=False) + "\n")
    print(f"wrote {len(queries)} held-out queries to {args.output}")


if __name__ == "__main__":
    main()
