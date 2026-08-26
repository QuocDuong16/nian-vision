---
type: Reference
title: Source Map
description: Inventory of all files currently in the nian-vision repository with their purpose and status.
tags: [source-map, navigation, file-inventory]
timestamp: 2025-08-26
---

# Source Map

Current file inventory for the `nian-vision` repository. As of the initial commit (`d9be068`), the repository contains only scaffolding — no application code.

## Repository Structure

```
nian-vision/
├── .forgejo/
│   └── workflows/
│       └── openwiki-update.yml    # CI: scheduled wiki regeneration (Forgejo Actions)
├── openwiki/
│   └── INSTRUCTIONS.md            # Wiki generation brief
├── skills/
│   ├── mermaid-diagrams/
│   │   └── SKILL.md               # Mermaid diagram generation skill
│   └── write-connector/
│       └── SKILL.md               # Connector authoring skill
├── AGENTS.md                      # Agent instructions (auto-generated)
├── CLAUDE.md                      # Claude agent instructions (auto-generated)
└── README.md                      # Project README (empty)
```

## File Details

### Root Files

| File | Status | Purpose |
|------|--------|---------|
| `README.md` | Empty (0 bytes) | Placeholder project README. Should be populated when application code is added. |
| `AGENTS.md` | Scaffolded | Auto-generated OpenWiki instructions for AI coding agents. Contains a brief pointing to `openwiki/quickstart.md`. |
| `CLAUDE.md` | Scaffolded | Auto-generated OpenWiki instructions for Claude. Identical content to `AGENTS.md`. |

### CI/CD

| File | Purpose |
|------|---------|
| `.forgejo/workflows/openwiki-update.yml` | Forgejo Actions workflow that runs OpenWiki daily to regenerate documentation. Uses OpenAI-compatible provider (opencode.ai) with `mimo-v2.5`. See [Architecture Overview](/openwiki/architecture/overview.md) for details. |

### OpenWiki

| File | Purpose |
|------|---------|
| `openwiki/INSTRUCTIONS.md` | User-authored brief defining what the wiki should cover. This is the configuration that drives wiki generation scope. |

### Skills

| File | Purpose |
|------|---------|
| `skills/mermaid-diagrams/SKILL.md` | Instructions for generating Mermaid diagrams in wiki pages (sequence, state, ER, flowchart). |
| `skills/write-connector/SKILL.md` | Instructions for authoring new OpenWiki source connectors (TypeScript modules with security and ingestion rules). |

## Git History

The repository has a single commit on `main`:

| Commit | Message | Changes |
|--------|---------|---------|
| `d9be068` | first commit | Added `README.md` (empty) |

All other files (AGENTS.md, CLAUDE.md, .forgejo/, openwiki/, skills/) are present in the working tree but not yet committed (untracked).
