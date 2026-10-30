# aq-playbooks

A proposed standalone Rust engine for repeatable policy workflows, with separate compiler and runner components, a command/event extension registry, and composable sub-workflows.

Start with the [design](docs/specs/0001-aq-playbooks.md), which defines the agreed twelve-command AQ host surface and reusable sub-workflows. The [SDLC exploration](docs/research/2026-10-30-sdlc-policy-exploration.md) supplies seventy policy examples used to reason about the scope.

The [v1 capability selection](docs/research/0001-v1-to-v2-capability-selection.md) records the rationale for that surface, with complete inventories kept as supporting evidence rather than migration requirements.

This repository currently contains a design only. The proposed crates, CLI commands, storage adapter, and authoring skill are not implemented. The baseline embeds the runner in the central AQ daemon; it does not require another deployed service.
