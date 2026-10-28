#!/usr/bin/env bash
set -Eeuo pipefail

# Additive GitHub repository housekeeping for GitRun.
# Requires GitHub CLI and repository administration permissions.
# Does not delete/rename labels, replace the topic set, or change branch rules.

REPO="Vider06/GitRun"
DESCRIPTION="GitRun — secure, self-hosted GitHub Actions runner orchestration and management."
TOPICS=(gitrun rust github-actions self-hosted-runners developer-tools automation)

command -v gh >/dev/null 2>&1 || { printf 'Error: install GitHub CLI (gh) first.\n' >&2; exit 1; }
gh auth status >/dev/null 2>&1 || { printf 'Error: authenticate with gh auth login first.\n' >&2; exit 1; }
gh repo view "$REPO" >/dev/null

printf 'Repository: %s\n' "$REPO"
printf 'Actions: update description, add topics without removing existing topics, and create missing labels.\n'
printf 'No labels will be deleted or renamed. Branch protection and source files will not be changed.\n'
read -r -p 'Continue? [y/N] ' answer
[[ "$answer" == "y" || "$answer" == "Y" ]] || { printf 'Cancelled.\n'; exit 0; }

gh api --method PATCH "repos/$REPO" -f description="$DESCRIPTION" >/dev/null

# GitHub replaces the topic set on PUT, so merge current and desired topics first.
mapfile -t existing_topics < <(gh api "repos/$REPO/topics" --jq '.names[]' 2>/dev/null || true)
declare -A seen_topics=()
topics=()
for topic in "${existing_topics[@]}" "${TOPICS[@]}"; do
  [[ -n "$topic" && -z "${seen_topics[$topic]+x}" ]] || continue
  seen_topics["$topic"]=1
  topics+=("$topic")
done
topic_args=()
for topic in "${topics[@]}"; do topic_args+=(-f "names[]=$topic"); done
gh api --method PUT "repos/$REPO/topics" "${topic_args[@]}" >/dev/null

ensure_label() {
  local name="$1" color="$2" description="$3"
  local encoded_name
  encoded_name=$(printf '%s' "$name" | sed 's/%/%25/g; s/ /%20/g; s/:/%3A/g')
  if gh api "repos/$REPO/labels/$encoded_name" >/dev/null 2>&1; then
    printf 'Exists:  %s\n' "$name"
    return 0
  fi
  gh api --method POST "repos/$REPO/labels" \
    -f name="$name" -f color="$color" -f description="$description" >/dev/null
  printf 'Added:   %s\n' "$name"
}

# Type
ensure_label "type: bug" "d73a4a" "A defect or behavior that does not match expectations"
ensure_label "type: enhancement" "a2eeef" "A new capability or improvement to existing behavior"
ensure_label "type: documentation" "0075ca" "Documentation content, accuracy, or navigation"
ensure_label "type: maintenance" "6f42c1" "Repository upkeep and internal maintenance"
ensure_label "type: question" "d876e3" "A usage or design question that needs clarification"

# Area
ensure_label "area: cli" "1d76db" "CLI commands and terminal management console"
ensure_label "area: dashboard" "1d76db" "Dashboard and desktop interface"
ensure_label "area: runners" "1d76db" "Runner lifecycle, execution and orchestration"
ensure_label "area: scheduler" "1d76db" "Scheduling, Kitty and adaptive resource management"
ensure_label "area: security" "b60205" "Security-sensitive code, policy or configuration"
ensure_label "area: packaging" "fbca04" "Installers, packages and release packaging"
ensure_label "area: workflows" "ededed" "GitHub workflows and repository automation"
ensure_label "area: docs" "0075ca" "Documentation structure and contributor guidance"

# Priority and status. Apply only after triage; these labels do not imply an SLA.
ensure_label "priority: critical" "b60205" "Critical impact; maintainer triage required"
ensure_label "priority: high" "d93f0b" "High impact; prioritize during triage"
ensure_label "priority: normal" "fbca04" "Normal priority after triage"
ensure_label "priority: low" "c5def5" "Low impact or non-urgent improvement"
ensure_label "status: needs-triage" "ededed" "Needs initial maintainer classification"
ensure_label "status: blocked" "5319e7" "Cannot progress until a dependency or decision is resolved"
ensure_label "status: help-wanted" "008672" "Well-scoped contribution suitable for community help"

printf '\nRepository description, additive topics, and label setup completed.\n'
printf 'Review repository settings at: https://github.com/%s/settings\n' "$REPO"
