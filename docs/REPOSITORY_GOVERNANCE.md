# Repository Governance

This document describes the maintenance conventions used to keep GitRun's repository discoverable, consistent, and safe to contribute to.

## Change boundaries

- Prefer focused pull requests with a clear scope and reviewable diff.
- Do not combine release/version changes with unrelated repository housekeeping.
- Avoid deleting or renaming established files, labels, or workflows without checking their references and explaining the migration.
- Keep templates concise and ask contributors only for information needed to act on a report.
- Keep security vulnerability reports out of public issues. Use the private reporting route configured in the repository's GitHub security settings.

## Issues and pull requests

Use the closest available issue template. Repository-wide consistency, documentation navigation, labels, metadata, and contributor-experience issues belong in the repository-maintenance template.

Pull request descriptions should explain the user-visible or maintainer-visible outcome, enumerate material changes, call out compatibility or migration concerns, and identify any manual follow-up. Do not claim checks or reviews that were not performed.

## Labels

Labels should help triage rather than duplicate one another. Prefer a small, stable vocabulary grouped by purpose:

- **Type:** bug, enhancement, documentation, maintenance, question.
- **Area:** CLI, dashboard, runners, security, packaging, documentation, workflows.
- **Priority:** reserved for issues whose impact has been assessed.
- **Status:** use only where a defined workflow needs it.

Before creating or renaming labels, inspect the current set and references. Add labels conservatively; do not bulk-delete or rename existing labels as part of a polish pass.

## Repository metadata

Keep the description short and product-focused. Topics should identify GitRun's primary language, project type, and relevant ecosystem without keyword stuffing. Changes to description, topics, branch protection, security features, and other repository settings are administrative operations and are not applied by a file-only pull request.

## Maintenance checklist

- Check links and references before moving files.
- Keep issue forms and pull request templates aligned with the actual contribution process.
- Prefer additive changes when current configuration has not been fully audited.
- Separate repository settings that require privileged access from changes reviewable in a pull request.
- Do not include credentials, tokens, private keys, or sensitive diagnostic data in issues or pull requests.
