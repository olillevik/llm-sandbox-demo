# ADR 0001: Dependency admission policy for llm-box

- **Status:** Accepted
- **Date:** 2026-04-16

## Context

`llm-box` is intended to be a small, security-conscious control plane around boxed LLM CLIs. We want fast iteration, but we do not want to casually grow a dependency tree that expands supply-chain risk, review burden, attack surface, or long-term maintenance cost.

The project is also being built with LLM assistance. That makes dependency discipline more important, not less: it is easy for an implementation loop to add a crate because it is convenient, even when the same behavior would be simple to own directly.

## Decision

Every new direct dependency must clear two bars before being added:

1. **Non-trivial to own:** the dependency should solve a problem that would be meaningfully complex, error-prone, or distracting for this project to implement and maintain ourselves.
2. **Security posture is acceptable:** the dependency should not introduce disproportionate supply-chain or operational risk relative to the value it provides.

A new dependency proposal should explicitly answer:

1. What specific problem does it solve?
2. Why is that problem non-trivial for us to own ourselves?
3. What is the expected security impact?
4. What transitive dependencies or build-time code generation does it bring in?
5. Is the crate mature, actively maintained, and broadly scrutinized?
6. Can we achieve the same outcome with the standard library or an already-approved dependency?

Popularity alone is **not** enough. A widely used crate can still be risky if it is too large, poorly scoped for our needs, weakly maintained, or introduces unnecessary transitive dependencies.

## Security stance

For this project, dependency review should consider at least:

- **Supply-chain trust:** maintainers, release cadence, project reputation, and whether there are obvious ownership red flags
- **Transitive footprint:** how much extra code enters the build graph
- **Build-time execution:** especially proc-macros and build scripts
- **Runtime privilege:** filesystem, subprocess, network, terminal, and parsing exposure
- **Cryptographic correctness:** if a crate touches cryptography or security-sensitive parsing, we strongly prefer established ecosystem implementations over homegrown code

We do **not** assume a crate is safe simply because we do not know the maintainers personally. The correct default is:

- there is always some supply-chain risk
- we accept that risk only when the value is clear
- we prefer smaller and more established dependency surfaces

## Approved dependency rationale

These are the currently approved direct Rust dependencies for the first rewrite pass.

### clap

- **Why keep it:** command parsing for `llm-box`, provider subcommands, help text, and passthrough arguments is non-trivial to implement cleanly and maintain over time.
- **Why not own it:** hand-rolled parsing becomes brittle quickly once we support provider dispatch, policy commands, help output, and future extensibility.
- **Security posture:** mature and broadly used; the main caution is proc-macro and transitive dependency footprint.
- **Trust note:** no specific trust red flag is known from our current review, but it remains external code and should be version-pinned and kept under normal dependency review.

### serde

- **Why keep it:** structured state files are core to the product.
- **Why not own it:** custom serialization logic would be repetitive and fragile.
- **Security posture:** ubiquitous and heavily scrutinized; derive macros add build-time code generation, which should be acknowledged explicitly.
- **Trust note:** no specific red flag identified; still subject to normal version review and dependency hygiene.

### serde_json

- **Why keep it:** `pending.jsonl`, session metadata, and related state are JSON-based.
- **Why not own it:** JSON parsing and serialization are not worth implementing ourselves.
- **Security posture:** standard ecosystem choice; parser correctness matters, but using a mature crate is safer than a homegrown parser.
- **Trust note:** accepted as a default JSON implementation unless a narrower requirement emerges.

### sha2

- **Why keep it:** stable workspace hashing is required.
- **Why not own it:** cryptographic primitives are exactly the kind of code we should not implement ourselves.
- **Security posture:** preferred over custom hashing implementations because correctness matters more than dependency purity here.
- **Trust note:** accepted because established crypto crates are safer than bespoke alternatives, while still requiring routine supply-chain review.

### anyhow

- **Why keep it:** only if it materially improves readability of operational error paths in the Rust control plane.
- **Why not own it:** manual boxed-error plumbing is easy but noisy; this crate is convenience, not core capability.
- **Security posture:** low direct runtime risk, but it is also the easiest dependency to avoid.
- **Trust note:** acceptable if it keeps the code clearer; if error handling stays simple, prefer not to add it.

## Consequences

- We will move a bit slower when adding dependencies.
- We will accept some targeted dependencies where they clearly replace non-trivial owned complexity.
- We will avoid convenience-driven crate growth.
- We will review direct dependencies as part of architecture, not as an afterthought.

## Follow-up guidance

When proposing a new dependency in a PR or LLM-driven change, include a short note answering:

1. Why is this hard enough that we should not own it?
2. What new security or supply-chain exposure does it add?
3. Why is it better than the simplest in-repo alternative?
