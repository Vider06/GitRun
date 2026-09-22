# Public repository checklist

Before switching GitRun to public visibility:

1. Rotate or revoke every credential that has ever appeared in repository content, logs, issues, or CI output.
2. Scan the complete Git history for secrets and personal data.
3. Decide whether commit-author emails should be rewritten to GitHub noreply addresses.
4. Remove temporary diagnostics, host-maintenance workflows, local infrastructure scripts, and private operational notes.
5. Verify that runtime configuration files and generated state are ignored.
6. Enable secret scanning and push protection.
7. Enable Dependabot alerts and code scanning.
8. Protect `main` with required CI checks and pull-request review.
9. Confirm the public README describes the actual supported installation paths and security model.
10. Re-run the full CI suite after the visibility change.

History rewriting is intentionally not automated by this checklist because it changes commit SHAs and can invalidate existing clones, pull requests, and tags. Use GitHub's documented sensitive-data-removal procedure and coordinate all existing clones before force-pushing rewritten history.
