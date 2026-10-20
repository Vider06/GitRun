# GitRun Premade

GitRun Premade is the maintained artifact/content branch for reusable GitRun resources.

This branch is intentionally separate from the GitRun application source tree. It contains premade Docker images, VM image definitions, reusable workflows, shared metadata, and tests consumed by GitRun.

## Layout

Core/, Dockers/, VM-Images/, Workflows/, Testing/, and Other/ contain maintained Premade content.

## Rules

Every artifact must document its purpose, inputs, outputs, security assumptions, versioning, and ownership. Docker builds use BuildKit/buildx. Artifact references must be explicit and reproducible.

The branch is not a source branch for the GitRun application and must not be merged into application branches.

## Kitty Dispatch workflows

The `Workflows/GitRun-Dependent/kitty-dispatch-plan.yml` reusable workflow asks the local GitRun controller for a validated one-use runner route. Its outputs explicitly distinguish `controller_online`, `kitty_online` (true/false/unknown), and `mode=kitty|github`, and include status/reason details; `kitty-dispatch-example.yml` shows how to feed the JSON label array into a downstream job's dynamic `runs-on`.

This workflow depends on the matching GitRun build that implements `GitDispatchRun --plan`, opt-in Kitty settings, aggregate telemetry, Adaptive Resource Management, Shadow mode OFF, GSR authorization for `GitDispatchRun/Plan`, and an online managed Linux planner runner labeled `gitrun-control`. If that control runner cannot start, the workflow cannot run to detect an offline controller. GitHub performs the final assignment from labels; this is not direct runner-ID pinning.
