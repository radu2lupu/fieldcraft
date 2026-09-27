#!/usr/bin/env python3
"""Create a normalized benchmark skill corpus for the SWE-Bench-CL 56-task set.

This is intentionally deterministic. It does not claim to be the final
Fieldcraft normalizer; it produces the first benchmark-ready artifact that
matches the architecture docs closely enough to test the retrieval/runtime path.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PLAN_DIR = ROOT / "benchmarks/swe-bench-cl/four-tier/multirepo-56"


@dataclass(frozen=True)
class CardSpec:
    skill_name: str
    title: str
    description: str
    triggers: list[str]
    procedure: list[str]
    constraints: list[str]
    verification_steps: list[str]
    source_refs: list[str]
    category: str
    negative_cues: list[str]
    differentiators: list[str]


SOURCE_REFS = {
    "debug": [
        "eval/skills/external/zenobi-dotfiles/.../systematic-debugging/SKILL.md",
        "eval/skills/external/laurigates-claude-plugins/code-quality-plugin/skills/debugging-methodology/SKILL.md",
    ],
    "testing": [
        "eval/skills/external/laurigates-claude-plugins/python-plugin/skills/python-testing/SKILL.md",
        "eval/skills/external/laurigates-claude-plugins/python-plugin/skills/pytest-advanced/SKILL.md",
        "eval/skills/external/zenobi-dotfiles/.../test-driven-development/SKILL.md",
    ],
    "django": [
        "eval/skills/external/zenobi-dotfiles/.../django-developer/SKILL.md",
        "eval/skills/registries/terminalskills-skills/skills/django/SKILL.md",
        "https://docs.djangoproject.com/en/dev/internals/contributing/writing-code/unit-tests/",
    ],
    "sympy": [
        "eval/skills/external/claude-scientific-skills/skills/sympy/SKILL.md",
        "https://docs.sympy.org/latest/contributing/new-contributors-guide/writing-tests.html",
    ],
    "sphinx": [
        "eval/skills/corpus/swe_bench_cl_seed_skills.jsonl:sphinx-doc-build-directives",
        "https://www.sphinx-doc.org/en/master/extdev/testing.html",
    ],
    "matplotlib": [
        "eval/skills/external/matplotlib-skill/skills/matplotlib/SKILL.md",
        "eval/skills/external/claude-scientific-skills/skills/matplotlib/SKILL.md",
        "https://matplotlib.org/stable/devel/index.html",
    ],
    "sklearn": [
        "eval/skills/external/claude-scientific-skills/skills/scikit-learn/SKILL.md",
        "eval/skills/external/SciAgent-Skills/skills/scientific-computing/scikit-learn-machine-learning/SKILL.md",
        "https://scikit-learn.org/stable/developers/index.html",
    ],
    "astropy": [
        "eval/skills/external/claude-scientific-skills/skills/astropy/SKILL.md",
        "https://docs.astropy.org/en/latest/development/testguide.html",
    ],
    "pytest": [
        "eval/skills/external/laurigates-claude-plugins/python-plugin/skills/pytest-advanced/SKILL.md",
        "eval/skills/registries/terminalskills-skills/skills/pytest/SKILL.md",
        "https://pytest.org/en/8.2.x/contributing.html",
    ],
}


def card(
    skill_name: str,
    title: str,
    description: str,
    triggers: list[str],
    procedure: list[str],
    constraints: list[str],
    verification_steps: list[str],
    category: str,
    source_keys: list[str],
    negative_cues: list[str] | None = None,
    differentiators: list[str] | None = None,
) -> CardSpec:
    refs = []
    for key in source_keys:
        refs.extend(SOURCE_REFS[key])
    return CardSpec(
        skill_name=skill_name,
        title=title,
        description=description,
        triggers=triggers,
        procedure=procedure,
        constraints=constraints,
        verification_steps=verification_steps,
        source_refs=refs,
        category=category,
        negative_cues=negative_cues or [],
        differentiators=differentiators or [],
    )


def specs() -> list[CardSpec]:
    general = [
        card(
            "swebench-root-cause-debugging",
            "SWE-Bench Root Cause Debugging",
            "Use for repository bugfix tasks where the agent must reproduce, locate root cause, patch minimally, and verify behavior.",
            ["SWE-Bench issue", "failing regression test", "unexpected behavior", "bugfix in existing repository"],
            [
                "Read the failure, issue text, and nearest tests before editing.",
                "Trace from failing behavior to the smallest responsible abstraction.",
                "Patch at the source of the incorrect behavior, not only at the symptom.",
                "Run the narrowest relevant test first, then adjacent tests if feasible.",
            ],
            [
                "Do not rewrite broad subsystems before identifying the failing boundary.",
                "Do not treat a passing smoke test as proof when the reported regression has a narrower test path.",
            ],
            ["run the named failing test", "run the nearest existing test module"],
            "general",
            ["debug"],
            ["new greenfield app", "pure documentation writing"],
            ["Root-cause-first flow is required before proposing fixes."],
        ),
        card(
            "python-regression-test-workflow",
            "Python Regression Test Workflow",
            "Use when fixing Python library bugs that need focused regression tests and minimal verification.",
            ["add regression test", "pytest failure", "unit test for bug", "focused test module"],
            [
                "Find existing tests covering the same behavior.",
                "Add one failing test that captures the reported bug before broad refactors.",
                "Keep fixtures local unless existing convention uses shared conftest fixtures.",
                "Verify the new test fails before the fix when practical, then passes after the fix.",
            ],
            [
                "Avoid tests that only assert implementation details.",
                "Avoid broad test rewrites that obscure the regression.",
            ],
            ["run the new test directly", "run the nearest existing test file"],
            "general",
            ["testing"],
            ["frontend visual testing", "browser-only E2E"],
            ["A regression test should fail for the original bug, not just cover code."],
        ),
        card(
            "python-property-based-regression-tests",
            "Python Property-Based Regression Tests",
            "Use for validators, parsers, numeric transformations, simplifiers, and normalize/round-trip behavior where examples miss edge cases.",
            ["property-based", "hypothesis", "round trip", "normalization", "validator edge case", "parser invariant"],
            [
                "Identify the invariant: round-trip, idempotence, monotonicity, validity after normalization, or shape preservation.",
                "Prefer a small deterministic regression test when the issue gives a concrete failing case.",
                "Use property-based checks when adjacent inputs are likely to expose the same bug family.",
            ],
            [
                "Do not add slow broad randomized tests to a focused bugfix unless the project already uses them nearby.",
            ],
            ["run deterministic regression first", "run property test with bounded examples if added"],
            "general",
            ["testing", "debug"],
            ["UI snapshot only", "deployment config"],
            ["Property tests are justified by invariants, not by generic desire for more tests."],
        ),
    ]
    repo_cards = [
        card("django-orm-query-expressions", "Django ORM Query Expressions", "Use for Django ORM bugs involving QuerySet composition, Q objects, annotations, expressions, output fields, related joins, aggregates, and SQL compiler behavior.", ["QuerySet", "Q object", "annotation", "expression", "output_field", "join", "aggregate", "SQL compiler"], ["Start from the failing ORM test and identify the query construction stage: expression resolution, join promotion, compiler SQL rendering, or result conversion.", "Patch inside existing ORM abstractions rather than adding special cases at call sites.", "Check database-backend compatibility before asserting SQL strings."], ["Avoid changing public QuerySet semantics globally unless the regression requires it.", "Watch for backend-specific SQL differences."], ["run focused tests under tests/queries, aggregation, expressions, or annotations", "run the new regression test on SQLite first"], "django", ["django"], ["admin form layout", "static files", "template rendering"], ["Django ORM fixes often belong in expression resolution or compiler joins, not in user-facing QuerySet methods."]),
        card("django-validation-and-regex", "Django Validation and Regex", "Use for Django validators, checks framework, URL/user/email/password token validation, regex edge cases, and model/form validation regressions.", ["validator", "URLValidator", "regex", "check framework", "password reset token", "model validation", "form validation"], ["Find the validator/check class and the adjacent tests.", "Add boundary tests for invalid and nearby valid inputs.", "Prefer the smallest parsing or regex change that preserves documented valid cases."], ["Do not broaden rejection without testing encoded or legacy valid forms.", "Avoid replacing mature validators with ad hoc parsing."], ["run tests/validators or the relevant checks/forms test module"], "django", ["django"], ["ORM SQL compiler", "migration autodetector"], ["Boundary valid cases are as important as the invalid case."]),
        card("django-migrations-multidb-contenttypes", "Django Migrations, Multi-DB, and ContentTypes", "Use for Django migration autodetector, makemigrations, router.allow_migrate, contenttypes, historical models, and multi-database consistency bugs.", ["makemigrations", "migration autodetector", "allow_migrate", "contenttypes", "historical model", "multi database", "router"], ["Trace whether the bug occurs during state rendering, autodetection, migration writing, or checks.", "Preserve historical model semantics and router arguments.", "Add tests with the smallest model/app setup that exercises the migration state."], ["Do not use live model metadata where migration state is required.", "Avoid assuming a single default database when routers are involved."], ["run migration/autodetector/admin checks tests relevant to the change"], "django", ["django"], ["URL regex", "template rendering"], ["Migration code often must use app_label/model_name from project state rather than live models."]),
        card("django-admin-forms-templates", "Django Admin, Forms, and Templates", "Use for Django admin checks, ModelAdmin formfield hooks, form widget overrides, template/admin display, and admin validation behavior.", ["ModelAdmin", "formfield_for_manytomany", "admin.E108", "widget override", "admin checks", "forms"], ["Locate the admin check or formfield hook path.", "Preserve extension hooks and instance-vs-class attribute behavior.", "Add regression tests in admin checks/options/forms adjacent to existing coverage."], ["Do not break documented ModelAdmin override points.", "Avoid solving admin checks by weakening validation globally."], ["run focused admin checks/options tests"], "django", ["django"], ["ORM joins", "migrations"], ["Admin checks can differ for instance attributes versus class attributes."]),
        card("sympy-simplification-subs-products", "SymPy Simplification, Subs, and Products", "Use for SymPy bugs involving simplification, substitution, parenthesizing, products, printers, parse evaluation, and expression preservation.", ["simplification", "Subs", "Product", "evaluate=False", "printer", "parse_expr", "parentheses", "Min", "Max"], ["Reproduce with the smallest symbolic expression.", "Identify whether the issue is construction, simplification, substitution, parsing, or printing.", "Preserve exact expression semantics and avoid eager evaluation when evaluate=False is requested."], ["Do not canonicalize away user-requested unevaluated structure.", "Avoid fixing one printer by changing core expression semantics unless required."], ["run the targeted sympy module test", "add a small expression regression test"], "sympy", ["sympy"], ["matrix shape operations", "Django ORM"], ["evaluate=False and Subs parenthesizing are semantic preservation issues, not formatting-only bugs."]),
        card("sympy-sets-assumptions-logic", "SymPy Sets, Assumptions, and Logic", "Use for SymPy truthiness, assumptions, relational parsing, set logic, Boolean comparisons, and symbolic equality edge cases.", ["S.false", "Boolean", "assumptions", "relational", "sets", "truthiness", "symbolic equality"], ["Check whether Python truthiness is being confused with SymPy Boolean semantics.", "Keep symbolic and numeric equality paths distinct.", "Add tests for both exact and floating inputs when truthiness is involved."], ["Do not use Python bool conversion on symbolic values unless explicitly intended."], ["run assumptions/sets/core relational tests"], "sympy", ["sympy"], ["Matplotlib units", "pytest marks"], ["SymPy Boolean values are not interchangeable with numeric zero/one."]),
        card("sympy-evalf-numeric-evaluation", "SymPy Numeric Evaluation and Lambdify", "Use for SymPy evalf, lambdify, mpmath wrapping, rationals, numeric precision, and code generation numeric behavior.", ["evalf", "lambdify", "mpmath", "Rational", "precision", "autowrap", "codegen"], ["Find the numeric conversion boundary.", "Preserve exact symbolic representation until the backend requires numeric wrapping.", "Test with rational, float, and backend-specific inputs when relevant."], ["Avoid converting exact rationals to imprecise floats too early."], ["run lambdify/evalf/codegen focused tests"], "sympy", ["sympy"], ["Django validation", "Sphinx autodoc"], ["Backend wrappers may need to preserve rational semantics explicitly."]),
        card("sympy-matrix-array-operations", "SymPy Matrix and Array Operations", "Use for SymPy matrix insertion, array arguments, shape handling, and indexed/matrix expression bugs.", ["Matrix", "col_insert", "array argument", "shape", "Indexed", "autowrap array"], ["Reduce to a minimal matrix/array shape case.", "Check row/column bounds and empty-array behavior.", "Add regression tests for shape and element order."], ["Do not assume square matrices or non-empty arrays."], ["run matrices or codegen array tests"], "sympy", ["sympy"], ["sets assumptions", "Sphinx builders"], ["Matrix bugs often hide in boundary shapes and insertion indices."]),
        card("sphinx-toctree-index-domain", "Sphinx Toctree, Index, and Domain Resolution", "Use for Sphinx bugs involving toctree, index entries, glossary terms, domain objects, nitpick resolution, cross-references, and duplicate detection.", ["toctree", "index", "domain", "glossary", "nitpick", "cross reference", "duplicate term"], ["Create or find the smallest Sphinx test project.", "Identify whether resolution occurs at parse, environment, domain, or builder stage.", "Assert warning locations and output nodes where relevant."], ["Do not fix only one builder if the domain/environment state is shared."], ["run focused Sphinx test with test_app or make_app"], "sphinx", ["sphinx"], ["Matplotlib axes", "Django migrations"], ["Sphinx bugs often need assertions on both warnings and doctree/environment state."]),
        card("sphinx-autodoc-typehints", "Sphinx Autodoc and Type Hints", "Use for Sphinx autodoc issues involving annotations, generic classes, Literal/Union types, meta public/private, docstring signatures, and mocked imports.", ["autodoc", "type hints", "Union", "Literal", "generic", "mock import", "meta public", "docstring default"], ["Reproduce with a tiny documented object.", "Trace from autodoc import/introspection to annotation formatting and domain resolution.", "Test both rendered output and warnings when annotations create references."], ["Avoid evaluating annotations that should remain mocked or stringified."], ["run Sphinx autodoc focused tests"], "sphinx", ["sphinx"], ["LaTeX whitespace only", "Django forms"], ["Autodoc bugs often involve introspection under mocked imports or delayed annotation evaluation."]),
        card("sphinx-latex-html-builders", "Sphinx LaTeX and HTML Builders", "Use for Sphinx builder-specific output bugs, LaTeX whitespace/highlighting, HTML rendering, inline literals, and output transforms.", ["LaTeX", "HTML builder", "inline code", "highlighting", "PDF output", "builder output"], ["Determine whether the issue is parser node, transform, translator, or builder output.", "Use the smallest document and compare builder-specific output.", "Keep shared doctree changes compatible across builders."], ["Do not make HTML fixes in LaTeX translator paths or vice versa unless shared nodes require it."], ["run builder-specific Sphinx tests"], "sphinx", ["sphinx"], ["autodoc type introspection", "pytest capture"], ["Builder-specific fixes should assert rendered output, not just doctree shape."]),
        card("sphinx-warning-location-events", "Sphinx Warning Locations and Events", "Use for Sphinx warning location, source-read/include events, duplicate warnings, and event mutation behavior.", ["warning location", "source-read", "include", "events", "duplicate warning", "line number"], ["Trace source path and line metadata through parser/events.", "Test warning message and location together.", "For include/source-read, verify event mutations affect the intended source."], ["Do not drop location metadata to silence a warning."], ["run warning/event focused Sphinx tests"], "sphinx", ["sphinx"], ["sklearn metrics", "Django validators"], ["Warning correctness includes file and line provenance."]),
        card("sphinx-rst-directives-includes", "Sphinx reST Directives and Includes", "Use for Sphinx reStructuredText directives, include handling, source transforms, and nested document source behavior.", ["include directive", "reST directive", "source-read", "nested source", "docutils"], ["Build the minimal document tree with include/directive structure.", "Trace whether the source is modified before or after include expansion.", "Assert both doctree and emitted output/warnings."], ["Avoid global parser changes for one directive unless docutils semantics require it."], ["run include/directive focused Sphinx tests"], "sphinx", ["sphinx"], ["autodoc only", "Matplotlib legends"], ["Include handling crosses source boundaries; test the included file, not only root source."]),
        card("matplotlib-axis-scale-limits", "Matplotlib Axis Scale and Limits", "Use for Matplotlib bugs involving axes limits, hist ranges, density normalization, autoscaling, transforms, and scale behavior.", ["Axes", "axis limits", "autoscale", "hist range", "density", "scale", "transform"], ["Create a minimal plot reproducer and inspect artists/data limits.", "Identify whether bug is in data conversion, artist creation, autoscale, or draw-time transform.", "Add image-independent tests when possible by asserting limits, arrays, or artist state."], ["Avoid image comparison when a deterministic state assertion is enough."], ["run focused Matplotlib unit test or image test if required"], "matplotlib", ["matplotlib"], ["Sphinx builders", "Django ORM"], ["Many Matplotlib regressions can be verified through artist state without image diffs."]),
        card("matplotlib-legend-colorbar-subfigure", "Matplotlib Legends, Colorbars, and SubFigures", "Use for Matplotlib legend, colorbar, subfigure, layout, constrained layout, hspace/wspace, and artist parenting bugs.", ["legend", "colorbar", "SubFigure", "layout", "constrained_layout", "wspace", "hspace", "artist parent"], ["Trace artist parent/figure/subfigure ownership.", "Check layout engine interaction and stale state.", "Add tests for artist placement or ownership rather than broad image diffs when possible."], ["Do not assume all artists belong directly to Figure when SubFigure is involved."], ["run focused legend/colorbar/subfigure tests"], "matplotlib", ["matplotlib"], ["date formatting", "pytest skip"], ["SubFigure support often requires preserving non-Figure parents."]),
        card("matplotlib-units-dates-formatters", "Matplotlib Units, Dates, and Formatters", "Use for Matplotlib unit conversion, date locators/formatters, ConciseDateFormatter, tick labels, and converter behavior.", ["units", "dates", "formatter", "locator", "ConciseDateFormatter", "tick labels", "converter"], ["Reproduce with minimal data and inspect tick values/labels.", "Trace unit conversion before formatter output.", "Test boundary periods and label context such as year/month rollover."], ["Avoid hardcoding label strings without considering locale/timezone-independent behavior."], ["run dates/units/ticker focused tests"], "matplotlib", ["matplotlib"], ["legend layout", "Django forms"], ["Date formatter correctness often depends on context across the whole displayed interval."]),
        card("matplotlib-backend-figure-state", "Matplotlib Backend and Figure State", "Use for Matplotlib backend renderer, pickling, draw state, draggable artists, gid propagation, annotation state, and stale/callback behavior.", ["backend", "renderer", "pickle", "draggable", "gid", "AnnotationBbox", "figure state", "draw"], ["Identify whether state is stored on artist, canvas, renderer, or callback.", "Ensure pickled/restored figures avoid unserializable transient backend state.", "Add tests that assert artist/backend interactions without relying on a GUI backend."], ["Do not persist live callbacks or renderer objects through pickle unless already supported."], ["run backend/artist/pickle focused tests"], "matplotlib", ["matplotlib"], ["hist density only", "Sphinx autodoc"], ["Backend state should separate persistent artist properties from transient renderer/callback state."]),
        card("matplotlib-widgets-selectors", "Matplotlib Widgets and Selectors", "Use for Matplotlib interactive widgets, selectors, draggable legends, callback lifetimes, and event handling.", ["widget", "selector", "draggable", "callback", "event handling", "interactive"], ["Use a non-interactive backend test where possible.", "Trace callback registration and cleanup.", "Verify state survives draw/pickle only when appropriate."], ["Avoid requiring a GUI for regression tests unless the existing suite does."], ["run widgets/backend tests"], "matplotlib", ["matplotlib"], ["Sphinx warnings", "sklearn CV"], ["Interactive state often needs explicit cleanup to avoid leaks or pickle failures."]),
        card("sklearn-estimator-api-validation", "scikit-learn Estimator API and Validation", "Use for scikit-learn estimator API, fit/transform contracts, validation, set_output, pipelines, FeatureUnion, show_versions, and estimator attributes.", ["estimator", "fit", "transform", "Pipeline", "FeatureUnion", "set_output", "validation", "show_versions", "__len__"], ["Identify the estimator contract being violated.", "Preserve fit/transform return shapes and metadata conventions.", "Add regression tests using sklearn's estimator testing patterns where nearby."], ["Do not special-case one estimator if a base mixin contract should handle it."], ["run focused sklearn estimator/pipeline tests"], "sklearn", ["sklearn"], ["Matplotlib axes", "Django admin"], ["scikit-learn fixes should respect estimator API contracts and metadata propagation."]),
        card("sklearn-model-selection-cv", "scikit-learn Model Selection and Cross-Validation", "Use for scikit-learn model_selection, CV splitters, BaseSearchCV, refit timing, shuffling, and cross-validation behavior.", ["model_selection", "Cross-validation", "BaseSearchCV", "StratifiedKFold", "refit", "shuffle", "splitter"], ["Reproduce with small deterministic arrays.", "Check random_state, shuffle, ordering, and metadata recorded during fit/refit.", "Add tests that assert split indices or recorded timing/attributes."], ["Do not change splitter ordering without checking documented invariants."], ["run model_selection focused tests"], "sklearn", ["sklearn"], ["Sphinx toctree", "Django migrations"], ["CV fixes often require deterministic split index assertions."]),
        card("sklearn-metrics-numerical-stability", "scikit-learn Metrics and Numerical Stability", "Use for scikit-learn numeric stability, empty arrays, zero division, sparse SVM, KMeans reproducibility, metrics, and floating-point edge cases.", ["numeric stability", "ZeroDivisionError", "empty support_vectors", "KMeans", "n_jobs", "sparse", "metric"], ["Create a minimal numeric reproducer.", "Check dtype, sparse/dense path, empty inputs, and parallel determinism.", "Use tolerances appropriate to existing tests."], ["Avoid exact float equality unless existing API guarantees it."], ["run focused metrics/cluster/svm tests"], "sklearn", ["sklearn"], ["Django validators", "Sphinx events"], ["Numerical regressions need tolerance-aware assertions and edge input coverage."]),
        card("sklearn-preprocessing-feature-extraction", "scikit-learn Preprocessing and Feature Extraction", "Use for scikit-learn preprocessing, feature extraction, ColumnTransformer, pandas output, remainder estimators, and transform aggregation.", ["preprocessing", "feature extraction", "ColumnTransformer", "pandas output", "remainder", "transform", "FeatureUnion"], ["Trace feature names, output containers, and column order through fit/transform.", "Test dense/sparse or pandas/numpy outputs as relevant.", "Preserve metadata routing and shape invariants."], ["Do not drop feature names or container type when set_output requests pandas."], ["run compose/preprocessing focused tests"], "sklearn", ["sklearn"], ["model selection only", "pytest logging"], ["ColumnTransformer/FeatureUnion bugs often involve metadata and output container propagation."]),
        card("astropy-io-registry-identify", "Astropy IO Registry and Format Identify", "Use for Astropy IO registry, identify_format, FITS card formatting, minversion, inherited docstrings, and format detection bugs.", ["io.registry", "identify_format", "FITS Card", "format identify", "minversion", "InheritDocstrings"], ["Find the registry identifier or formatting path.", "Test both matching and non-matching formats.", "Preserve public registry call signatures and tuple arity expectations."], ["Do not mask identifier exceptions that should signal invalid formats unless existing API expects fallback."], ["run astropy io/registry/fits focused tests"], "astropy", ["astropy"], ["pytest marks", "Django ORM"], ["Registry identifiers must handle negative cases without tuple/arity crashes."]),
        card("astropy-units-quantity-ufunc", "Astropy Units, Quantity, and ufuncs", "Use for Astropy Quantity arithmetic, units compatibility, __array_ufunc__, NDData mask propagation, and unit conversion bugs.", ["Quantity", "units", "__array_ufunc__", "NDData", "mask propagation", "unit conversion", "ValueError", "NotImplemented"], ["Reproduce with minimal Quantity/NDData inputs.", "Check whether incompatible inputs should return NotImplemented or raise.", "Test mask/unit propagation for operands with and without masks."], ["Do not silently drop units or masks."], ["run astropy units/nddata focused tests"], "astropy", ["astropy"], ["Sphinx autodoc", "sklearn CV"], ["Returning NotImplemented can be correct for ufunc dispatch where raising blocks other operands."]),
        card("astropy-table-mixin-io", "Astropy Table, Mixin, and IO", "Use for Astropy Table mixins, IO serialization, registry interactions, mask/metadata propagation, and FITS/table round trips.", ["Table", "mixin", "IO", "serialization", "metadata", "mask", "round trip"], ["Create a minimal table/mixin round trip.", "Assert metadata, mask, dtype, and column identity as relevant.", "Check registry format selection before changing serialization."], ["Avoid converting mixin columns to plain columns unless explicitly expected."], ["run astropy table/io focused tests"], "astropy", ["astropy"], ["pytest capture", "Django forms"], ["Mixin table bugs often preserve behavior only if metadata and mask state round-trip."]),
        card("astropy-modeling-separability", "Astropy Modeling Separability", "Use for Astropy modeling separability_matrix, CompoundModel nesting, model composition, and separability propagation bugs.", ["separability_matrix", "CompoundModel", "modeling", "nested model", "composition"], ["Build the smallest nested CompoundModel.", "Trace separability propagation through operators.", "Assert matrix shape and values for nested and non-nested cases."], ["Do not flatten model structure if operator semantics depend on nesting."], ["run astropy modeling separability tests"], "astropy", ["astropy"], ["io registry", "matplotlib widgets"], ["Nested CompoundModels can require recursive separability composition."]),
        card("astropy-coordinates-wcs", "Astropy Coordinates and WCS", "Use for Astropy coordinates, WCS, ITRS/Observed transforms, frames, time/location interactions, and coordinate transformation bugs.", ["coordinates", "WCS", "ITRS", "Observed", "frame transform", "EarthLocation", "Time"], ["Create a minimal frame transform example.", "Check frame attributes, obstime, location, and representation type propagation.", "Compare against existing transform graph expectations."], ["Avoid adding direct transforms that bypass required frame metadata."], ["run astropy coordinates/wcs focused tests"], "astropy", ["astropy"], ["table IO only", "pytest marks"], ["Coordinate transforms depend on frame metadata as much as coordinate values."]),
        card("astropy-timeseries-required-columns", "Astropy TimeSeries Required Columns", "Use for Astropy TimeSeries, required columns, NDData-like propagation, and validation of time/mask/table structures.", ["TimeSeries", "required columns", "time column", "mask", "NDDataRef"], ["Reproduce with minimal table/time series inputs.", "Check required column validation and propagation after arithmetic or slicing.", "Test missing and present required columns."], ["Do not bypass required-column checks for convenience."], ["run astropy timeseries/nddata tests"], "astropy", ["astropy"], ["coordinates transform only", "Django migrations"], ["TimeSeries validity depends on required columns surviving operations."]),
        card("pytest-unittest-skip-pdb", "pytest unittest, skip, and pdb Interactions", "Use for pytest bugs involving unittest.TestCase, skipped tests, setUp/tearDown, --pdb, --runxfail, and skip/xfail reporting.", ["unittest.TestCase", "skip", "tearDown", "setUpClass", "--pdb", "--runxfail", "xfail"], ["Identify whether behavior occurs at collection, setup, call, teardown, or reporting.", "Add a focused test using pytester/inline runs when existing tests use it.", "Preserve unittest lifecycle semantics for skipped tests."], ["Do not run teardown for skipped unittest cases unless documented behavior requires it."], ["run testing/test_unittest.py or focused pytester test"], "pytest", ["pytest", "testing"], ["Django validators", "Matplotlib dates"], ["pytest lifecycle bugs need phase-specific assertions."]),
        card("pytest-reporting-serialization", "pytest Reporting and Serialization", "Use for pytest terminal reporting, location reporting, JUnit/serialization, pastebin metadata, and report object behavior.", ["reporting", "location", "serialization", "JUnit", "terminal", "pastebin", "TestReport"], ["Trace report fields from collection/call into terminal or serialized output.", "Add assertions on report location, nodeid, longrepr, or serialized fields.", "Keep output compatibility unless the issue specifies a changed format."], ["Do not fix display by corrupting report objects used by plugins."], ["run focused reporting/terminal/junit tests"], "pytest", ["pytest", "testing"], ["caplog fixture only", "sklearn metrics"], ["Report objects are plugin API; display fixes must preserve structured fields."]),
        card("pytest-marks-mro", "pytest Marks and MRO", "Use for pytest mark inheritance, class MRO, skip/xfail precedence, and mark lookup on classes/functions.", ["marks", "MRO", "mark inheritance", "skip location", "xfail", "class marks"], ["Construct minimal classes demonstrating MRO/mark order.", "Trace mark collection from class/function/module sources.", "Assert order and effective mark behavior."], ["Do not flatten MRO in a way that changes Python class semantics."], ["run focused mark tests"], "pytest", ["pytest", "testing"], ["capture binary mode", "Django admin"], ["Mark lookup must respect Python MRO and pytest precedence rules."]),
        card("pytest-capture-pastebin-bytes", "pytest Capture, Pastebin, and Bytes/Text Boundaries", "Use for pytest capture EncodedFile, binary/text mode, pastebin HTTP payloads, lexer selection, and captured output encoding bugs.", ["capture", "EncodedFile", "binary mode", "pastebin", "bytes", "text mode", "lexer"], ["Identify text/bytes boundary and encoding expectation.", "Use pytester to exercise CLI options when possible.", "Assert mode, payload, or captured output without depending on external network."], ["Do not make network calls in tests for pastebin behavior; mock or inspect request payload."], ["run focused capture/pastebin tests"], "pytest", ["pytest", "testing"], ["unittest skip", "Sphinx builders"], ["Capture bugs often require preserving text wrappers while avoiding binary mode leakage."]),
        card("pytest-collection-import-hooks", "pytest Collection and Import Hooks", "Use for pytest collection, import path handling, __init__.py collection, package discovery, conftest/plugin hooks, and collection regressions.", ["collection", "import hook", "__init__.py", "package discovery", "conftest", "collect", "hookimpl"], ["Build a minimal pytester file tree.", "Trace collection decisions before item execution.", "Assert collected items and ignored files."], ["Do not import arbitrary package files during collection unless pytest explicitly should."], ["run focused collection tests"], "pytest", ["pytest", "testing"], ["caplog logging", "Django migrations"], ["Collection regressions are best tested with file-tree fixtures and collected item assertions."]),
        card("pytest-caplog-logging", "pytest caplog and Logging", "Use for pytest caplog fixture, logging level restoration, captured records, stage records, and LogCaptureFixture behavior.", ["caplog", "logging", "LogCaptureFixture", "log level", "records", "get_records"], ["Reproduce with a tiny test using caplog.", "Track handler/level state before, during, and after test phases.", "Assert restoration and captured records separately."], ["Do not change global logging behavior beyond the fixture lifecycle."], ["run testing/logging focused tests"], "pytest", ["pytest", "testing"], ["pastebin bytes", "marks MRO"], ["caplog fixes must restore logging state after the fixture scope ends."]),
    ]
    return general + repo_cards


DANGEROUS_PATTERNS = [
    re.compile(r"ignore (all )?(previous|prior) instructions", re.I),
    re.compile(r"curl\b.*\|\s*(sh|bash)", re.I | re.S),
    re.compile(r"(OPENAI_API_KEY|ANTHROPIC_API_KEY|AWS_SECRET|GITHUB_TOKEN)"),
    re.compile(r"(exfiltrate|send.*secret|upload.*token)", re.I),
]


def scanner_findings(text: str) -> list[str]:
    return [pat.pattern for pat in DANGEROUS_PATTERNS if pat.search(text)]


def to_record(spec: CardSpec) -> dict:
    body = "\n".join(
        [
            f"# {spec.title}",
            "",
            "## Procedure",
            *[f"- {step}" for step in spec.procedure],
            "",
            "## Constraints",
            *[f"- {item}" for item in spec.constraints],
            "",
            "## Verification",
            *[f"- {item}" for item in spec.verification_steps],
            "",
            "## Differentiating Details",
            *[f"- {item}" for item in spec.differentiators],
        ]
    )
    source_material = json.dumps(
        {
            "skill_name": spec.skill_name,
            "title": spec.title,
            "description": spec.description,
            "triggers": spec.triggers,
            "procedure": spec.procedure,
            "constraints": spec.constraints,
            "verification_steps": spec.verification_steps,
            "source_refs": spec.source_refs,
            "negative_cues": spec.negative_cues,
            "differentiators": spec.differentiators,
        },
        sort_keys=True,
    )
    content_hash = hashlib.sha256(source_material.encode()).hexdigest()
    return {
        "skill_name": spec.skill_name,
        "title": spec.title,
        "description": spec.description,
        "triggers": spec.triggers,
        "activation_cues": spec.triggers,
        "negative_cues": spec.negative_cues,
        "procedure": "\n".join(f"- {step}" for step in spec.procedure),
        "constraints": spec.constraints,
        "required_capabilities": ["read repository", "edit code", "run focused tests"],
        "verification_steps": spec.verification_steps,
        "risk_status": "eligible",
        "risk_summary": "Eligible normalized benchmark card; source material is external/public or official docs and runtime payload is deterministic.",
        "source_uri": "fieldcraft://normalized-swecl-benchmark/" + spec.skill_name,
        "source_refs": spec.source_refs,
        "category": spec.category,
        "body": body,
        "hash": content_hash,
        "content_hash": "sha256:" + content_hash,
        "card_schema_version": 1,
        "normalization_version": "swecl-deterministic-v1",
        "atomization_status": "split" if spec.category != "general" else "not_needed",
        "differentiating_content": spec.differentiators,
    }


def read_jsonl(path: Path) -> list[dict]:
    rows = []
    with path.open() as f:
        for line in f:
            if line.strip():
                rows.append(json.loads(line))
    return rows


def write_json(path: Path, data: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")


def write_jsonl(path: Path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w") as f:
        for row in rows:
            f.write(json.dumps(row, ensure_ascii=False, sort_keys=True) + "\n")


def parse_required_skills(skill_needs: Path) -> Counter[str]:
    counts: Counter[str] = Counter()
    in_required = False
    for line in skill_needs.read_text().splitlines():
        if line.startswith("## Required Skill Cards"):
            in_required = True
            continue
        if in_required and line.startswith("## "):
            break
        if not in_required:
            continue
        match = re.match(r"- `([^`]+)`: (\d+) tasks", line)
        if match:
            counts[match.group(1)] = int(match.group(2))
    return counts


def build_queries(tasks_path: Path, coverage: dict[str, list[str]]) -> list[dict]:
    acceptable_overrides = {
        "pytest-dev__pytest-5262": ["pytest-capture-pastebin-bytes"],
    }
    queries = []
    for task in read_jsonl(tasks_path):
        iid = task["instance_id"]
        expected = task.get("needed_skills") or coverage.get(iid, [])
        problem = task.get("problem_statement") or task.get("title") or ""
        # Remove explicit skill IDs if they appear, but preserve domain terms like
        # "migrations", "autodoc", or "SubFigure"; those are the realistic cues
        # the retriever should use at runtime.
        redacted = problem
        for skill in expected:
            redacted = re.sub(re.escape(skill), "[redacted-skill]", redacted, flags=re.I)
        queries.append(
            {
                "id": iid,
                "query": "\n".join(
                    [
                        f"Repository: {task['repo']}",
                        f"Instance: {iid}",
                        "Problem:",
                        redacted.strip(),
                        "Failing tests:",
                        json.dumps(task.get("FAIL_TO_PASS") or task.get("fail_to_pass_sample") or []),
                    ]
                ),
                "expected": expected[:3],
                "acceptable": acceptable_overrides.get(iid, []),
                "source": "swe-bench-cl-normalized",
                "label_source": "task_family_mapping",
            }
        )
    return queries


def parse_task_skill_map(skill_needs: Path) -> dict[str, list[str]]:
    mapping: dict[str, list[str]] = {}
    current: str | None = None
    for line in skill_needs.read_text().splitlines():
        task_match = re.match(r"- `([^`]+)`: ", line)
        if task_match:
            current = task_match.group(1)
            continue
        skills_match = re.match(r"\s+Skills: (.*)", line)
        if current and skills_match:
            mapping[current] = re.findall(r"`([^`]+)`", skills_match.group(1))
            current = None
    return mapping


def scorecard(records: list[dict], required: Counter[str], task_map: dict[str, list[str]]) -> dict:
    names = {row["skill_name"] for row in records}
    missing = sorted(set(required) - names)
    findings = {row["skill_name"]: scanner_findings(json.dumps(row)) for row in records}
    leaking = {name: pats for name, pats in findings.items() if pats}
    by_category = Counter(row["category"] for row in records)
    source_ref_counts = {row["skill_name"]: len(row.get("source_refs") or []) for row in records}
    differentiator_counts = {row["skill_name"]: len(row.get("differentiating_content") or []) for row in records}
    uncovered_tasks = {
        iid: skills for iid, skills in task_map.items() if not any(skill in names for skill in skills)
    }
    return {
        "normalization_version": "swecl-deterministic-v1",
        "card_count": len(records),
        "required_skill_count": len(required),
        "required_skills_covered": len(set(required) & names),
        "missing_required_skills": missing,
        "uncovered_task_count": len(uncovered_tasks),
        "uncovered_tasks": uncovered_tasks,
        "category_counts": dict(sorted(by_category.items())),
        "scanner": {
            "post_normalization_scanner_pass": not leaking,
            "findings": leaking,
        },
        "fidelity": {
            "cards_with_source_refs": sum(1 for v in source_ref_counts.values() if v > 0),
            "cards_with_differentiating_content": sum(1 for v in differentiator_counts.values() if v > 0),
            "minimum_source_ref_count": min(source_ref_counts.values()) if source_ref_counts else 0,
        },
        "runtime_gate_decision": "eligible_for_benchmark" if not missing and not leaking and not uncovered_tasks else "needs_review",
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tasks", type=Path, default=PLAN_DIR / "tasks.jsonl")
    parser.add_argument("--skill-needs", type=Path, default=PLAN_DIR / "skill-needs.md")
    parser.add_argument("--output", type=Path, default=ROOT / "eval/skills/corpus/swecl_normalized_benchmark_skills.jsonl")
    parser.add_argument("--queries", type=Path, default=PLAN_DIR / "normalized-skill-queries.jsonl")
    parser.add_argument("--scorecard", type=Path, default=PLAN_DIR / "normalized-skill-scorecard.json")
    args = parser.parse_args()

    records = [to_record(spec) for spec in specs()]
    required = parse_required_skills(args.skill_needs)
    task_map = parse_task_skill_map(args.skill_needs)
    queries = build_queries(args.tasks, task_map)
    report = scorecard(records, required, task_map)

    write_jsonl(args.output, records)
    write_jsonl(args.queries, queries)
    write_json(args.scorecard, report)
    print(f"wrote {len(records)} normalized cards to {args.output}")
    print(f"wrote {len(queries)} eval queries to {args.queries}")
    print(f"runtime gate: {report['runtime_gate_decision']}")
    if report["missing_required_skills"]:
        print("missing:", ", ".join(report["missing_required_skills"]))
    return 0 if report["runtime_gate_decision"] == "eligible_for_benchmark" else 2


if __name__ == "__main__":
    raise SystemExit(main())
