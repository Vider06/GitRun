#!/usr/bin/env bash
set -Eeuo pipefail

# Additive repository housekeeping for GitRun. Run from Termux after installing
# GitHub CLI (gh) and authenticating with a token that can administer the repo.
# This script deliberately does not alter branch protection or delete/rename labels.

REPO="Vider06/GitRun"
DESCRIPTION="GitRun — secure, self-hosted GitHub Actions runner orchestration and management."
TOPICS=(gitrun rust github-actions self-hosted-runners developer-tools automation)

command -v gh >/dev/null 2>&1 || { printf 'Error: GitHub CLI (gh) is required.\n' >&2; exit 1; }
gh auth status >/dev/null 2>&1 || { printf 'Error: run gh auth login first.\n' >&2; exit 1; }
gh repo view "$REPO" >/dev/null

printf 'Repository: %s\n' "$REPO"
printf 'This will update the repository description, add topics, and ensure a small set of triage labels exists.\n'
printf 'It will not delete labels, change branch protection, or modify source branches.\n'
read -r -p 'Continue? [y/N] ' answer
[[ "$answer" == "y" || "$answer" == "Y" ]] || { printf 'Cancelled.\n'; exit 0; }

gh api --method PATCH "repos/$REPO" -f description="$DESCRIPTION" >/dev/null

# GitHub topic updates replace the topic set, so preserve existing topics first.
mapfile -t existing_topics < <(gh api "repos/$REPO/topics" --jq '.names[]' 2>/dev/null || true)
declare -A seen=()
topics=()
for topic in "${existing_topics[@]}" "${TOPICS[@]}"; do
  [[ -n "$topic" && -z "${seen[$topic]+x}" ]] || continue
  seen["$topic"]=1
  topics+=("$topic")
done
topic_json=$(printf '%s\n' "${topics[@]}" | python -c 'import json,sys; print(json.dumps({"names":[x.rstrip("\n") for x in sys.stdin if x.rstrip("\n")]}))')
gh api --method PUT "repos/$REPO/topics" --input - <<<"$topic_json" >/dev/null

ensure_label() {
  local name="$1" color="$2" description="$3"
  if gh api "repos/$REPO/labels/$(printf '%s' "$name" | sed 's/ /%20/g')" >/dev/null 2>&1; then
    printf 'Label exists: %s\n' "$name"
  else
    gh api --method POST "repos/$REPO/labels" -f name="$name" -f color="$color" -f description="$description" >/dev/null
    printf 'Created label: %s\n' "$name"
  fi
}

ensure_label "maintenance" "6f42c1" "Repository upkeep, consistency, and internal improvements"
ensure_label "documentation" "0075ca" "Documentation changes or issues"
ensure_label "question" "d876e3" "Questions about using or contributing to GitRun"
ensure_label "area: security" "b60205" "Security-sensitive code or configuration"
ensure_label "area: packaging" "fbca04" "Installers, packages, and release packaging"
ensure_label "area: workflows" "ededed" "GitHub workflows and automation"

printf '\nRepository description, additive topics, and label setup completed.\n'
printf 'Review current settings at: https://github.com/%s/settings\n' "$REPO"
