---
type: Reference
title: Architecture Overview
description: Documents the OpenWiki documentation infrastructure and CI/CD pipeline set up in the nian-vision repository using Forgejo Actions, including the daily automated wiki update workflow.
tags: [architecture, infrastructure, ci-cd, openwiki, forgejo]
timestamp: 2025-08-26
---

# Architecture Overview

The `nian-vision` repository is currently in a greenfield state — no application code has been committed yet. This page documents the OpenWiki documentation infrastructure that was scaffolded in the initial commit and will serve as the foundation for documenting the project as it grows.

## OpenWiki Documentation Infrastructure

OpenWiki is a tool that automatically generates and maintains a developer-friendly knowledge base from repository source code. It runs as a scheduled Forgejo Actions workflow and produces documentation under the `openwiki/` directory.

### How It Works

1. A **Forgejo Actions workflow** (`.forgejo/workflows/openwiki-update.yml`) runs daily at 08:00 (Asia/Ho_Chi_Minh) via cron, or on manual dispatch.
2. The workflow installs **OpenWiki v0.2.3** globally, along with Mermaid and jsdom for diagram validation.
3. OpenWiki inspects the repository source code, git history, and configuration.
4. It generates or updates Markdown documentation pages under `openwiki/`.
5. A **pull request** is automatically created on the `openwiki/update` branch, force-pushed and opened (or updated) via the Forgejo API.

### CI/CD Pipeline

The workflow is defined in [`.forgejo/workflows/openwiki-update.yml`](.forgejo/workflows/openwiki-update.yml):

| Configuration | Value |
|---------------|-------|
| **Trigger** | Cron (`0 8 * * *` Asia/Ho_Chi_Minh) + manual `workflow_dispatch` |
| **Runner** | `docker` |
| **Node.js** | v22 |
| **OpenWiki version** | 0.2.3 |
| **LLM provider** | OpenAI-compatible (opencode.ai) |
| **Model** | `mimo-v2.5` |
| **Tracing** | LangSmith (project: `openwiki`) |
| **PR branch** | `openwiki/update` |
| **PR commit message** | `docs: update OpenWiki` |
| **Timeout** | 45 minutes |
| **Concurrency** | `openwiki-update` group, no cancel-in-progress |

### Required Secrets

The workflow references these Forgejo repository secrets:

| Secret | Purpose |
|--------|---------|
| `OPENWIKI_FORGEJO_TOKEN` | Forgejo API token for checkout and pull request creation |
| `OPENAI_COMPATIBLE_API_KEY` | Authentication for the OpenAI-compatible LLM provider (opencode.ai) |
| `LANGSMITH_API_KEY` | Optional — enables LangSmith tracing for debugging |

### Authentication Model

Unlike GitHub Actions, Forgejo Actions does not have built-in permissions scopes. The workflow uses a personal Forgejo token (`OPENWIKI_FORGEJO_TOKEN`) for both repository checkout (via `data.forgejo.org/actions/checkout`) and API calls to create/update pull requests. The token is passed explicitly to both the checkout step and the curl-based PR publishing step.

## Agent Instruction Files

Two files provide instructions to AI coding assistants:

- **`AGENTS.md`** — Standard agent instructions pointing to `openwiki/quickstart.md`.
- **`CLAUDE.md`** — Claude-specific instructions (identical content to AGENTS.md).

Both contain the same OpenWiki brief, directing agents to start with the quickstart and not hand-edit generated wiki pages.

## Skills

The `skills/` directory contains instruction files for specialized wiki generation capabilities:

| Skill | Purpose |
|-------|---------|
| `mermaid-diagrams` | Guides diagram generation using Mermaid syntax for flows, lifecycles, and data models |
| `write-connector` | Documents how to author new OpenWiki source connectors |

These are read by OpenWiki during generation, not by the application itself.

## Future Architecture

As application code is added, this page should be updated to cover:

- Application architecture (frameworks, services, modules)
- Runtime and request flows
- Data storage and models
- External integrations
- Deployment topology
