---
type: Reference
title: Quickstart
description: Entry point for the nian-vision repository wiki. Covers what the repository is, what it currently contains, and where to go next.
tags: [quickstart, navigation, overview]
timestamp: 2025-08-26
---

# nian-vision — Quickstart

**Repository**: `nian-vision` (hosted at `git.niand.io.vn/coyote/nian-vision.git`)
**Current state**: Greenfield — single initial commit with empty `README.md` and OpenWiki infrastructure scaffolding. No application code exists yet.

## What This Repository Is

`nian-vision` is a newly initialized repository under the `coyote` organization on a private GitLab instance. The name suggests a vision-related project, but no application source code has been committed yet. The repository currently contains only OpenWiki documentation scaffolding, which will automatically generate and maintain this wiki.

## What's Here Now

| Path | Purpose |
|------|---------|
| `README.md` | Empty project README (placeholder) |
| `AGENTS.md` | OpenWiki agent instructions (auto-generated) |
| `CLAUDE.md` | OpenWiki agent instructions (auto-generated) |
| `openwiki/INSTRUCTIONS.md` | Wiki generation brief — defines scope and priorities |
| `.forgejo/workflows/openwiki-update.yml` | Forgejo Actions workflow that regenerates this wiki daily |
| `skills/mermaid-diagrams/` | Mermaid diagram skill for wiki generation |
| `skills/write-connector/` | Connector authoring skill for wiki generation |

## Where to Go Next

- **[Architecture Overview](/openwiki/architecture/overview.md)** — Explains the OpenWiki infrastructure and CI/CD pipeline.
- **[Source Map](/openwiki/source-map.md)** — Current file inventory with purpose annotations.

## Getting Started (When Code Arrives)

Once application code is added to this repository, this quickstart will include:

1. How to clone and set up the project locally
2. Build and run instructions
3. Key dependencies and configuration
4. Where to find tests

For now, the primary workflow is:

```bash
# Clone the repository
git clone ssh://git@git.niand.io.vn/coyote/nian-vision.git
cd nian-vision
```

## Backlog

| Area | Source Anchor | Reason Deferred |
|------|---------------|-----------------|
| Application architecture | (none) | No application code exists yet |
| Domain concepts | (none) | No domain model defined yet |
| Data models | (none) | No data layer exists yet |
| Testing guidance | (none) | No tests or test framework configured yet |
| Operations / runbook | (none) | No deployment or operational setup exists yet |
| Integrations | (none) | No external integrations configured yet |
| Key workflows | (none) | No application workflows exist yet |
