---
name: local-infrastructure
description: Provisions and manages local development PostgreSQL databases and MinIO buckets with linf. Use when a user asks to create, connect, inspect, or modify local Docker development infrastructure, databases, object storage, or environment values.
---

# Local development infrastructure

Use `linf` for local development PostgreSQL and MinIO. Do not replace it with raw Docker, Docker Compose, `psql`, or `mc` commands.

## Safety rules

- Work on the local Docker target only. Do not infer a remote host; require an explicit target, SSH host, and verified fingerprint before any remote action.
- Start read-only: run `linf doctor --json`, `linf target list --json`, and, when a target exists, `linf engine list --json`.
- Stop if Docker CLI or daemon checks fail. State the failed check and its remedy; do not attempt a workaround.
- Before every mutation, show the exact `linf` commands and ask for confirmation. Use `--plan` for engine, DB, and bucket creation. `target add-local` has no plan, so name it explicitly in the confirmation.
- Never pass passwords or secret keys on the command line. Never put generated `.env` values in a repository, a chat response, or logs unless the user explicitly asks for them.
- Do not use `--yes` unless the user explicitly authorizes a destructive operation. Never delete, reset, rotate credentials, or modify unmanaged Docker resources unless explicitly requested.

## Create local infrastructure

First determine the requested resources: PostgreSQL DB, MinIO bucket, or both; the project name, target, and any requested DB, user, bucket, port, or image overrides. Use defaults when the user does not specify an override.

1. Select a registered local target before generating a plan. Use the target explicitly named by the user; otherwise use the only registered local target. If no local target exists, show `linf target add-local --name local`, ask for confirmation, and run it before planning any engine or resource. If multiple local targets exist and none is named, ask the user to select one. Call the selected or newly created target `<target>` in every remaining command; never assume its name is `local`.
2. For each requested engine, preview it only after `<target>` exists:
   ```sh
   linf engine ensure <target> postgres 17 --plan
   linf engine ensure <target> minio latest --plan
   ```
3. Preview project resources after the engine plan:
   ```sh
   linf db create --target <target> --project <project> --plan
   linf bucket create --target <target> --project <project> --plan
   ```
4. Show the plan results and exact approved creation commands, then ask for confirmation. Run only the approved commands without `--plan`, in this order: ensure engine, create resource.
5. Verify each created resource:
   ```sh
   linf db test <database>
   linf bucket test <bucket>
   ```
   Use the names returned by the create commands rather than guessing their normalized forms.
6. Only when requested, print connection values with `linf db env <database>` or `linf bucket env <bucket>`.

## Existing infrastructure

- Inspect before changing anything: `linf db list --json` and `linf bucket list --json`.
- Reuse the target and engine already associated with a resource. Do not create a second shared engine merely because a project name is new.
- If a requested name already exists, report the conflict and offer to use the existing resource or choose a different name. Do not overwrite it.

## Examples

- “Create a local PostgreSQL database for acme” → preflight, select or register a local target, show the engine/DB plan, wait for confirmation, create, test, then offer the `.env` block.
- “Set up PostgreSQL and MinIO for acme” → preflight, select or register a local target, show both engine and resource plans, wait for confirmation, create and test each resource.
- “Show the connection values for acme” → inspect managed resources and print only the requested `linf ... env` output.
