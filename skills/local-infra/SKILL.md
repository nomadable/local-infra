---
name: local-infrastructure
description: Provisions and manages local development PostgreSQL databases and MinIO buckets, and opens the native PostgreSQL SQL Workbench, with the linf CLI. Use for dev databases, DATABASE_URL, object storage, SQL execution/catalog/export, external PostgreSQL profiles, backups, or linf/local-infra requests.
---

# Local development infrastructure (`linf`)

`linf` runs one shared PostgreSQL container and one shared MinIO container per Docker target, then creates a
dedicated database + user or bucket + access key per project. Use it instead of raw Docker, Docker Compose,
`psql`, or `mc`. Every command supports `--json`; prefer it and read exit codes.

Full command list: [references/commands.md](references/commands.md).
Step-by-step recipes (new project, `.env`, reset, backup, remote): [references/workflows.md](references/workflows.md).

## Safety rules

- Local Docker target only. Never infer a remote host. A remote action needs an explicit target name, SSH host,
  and a host-key fingerprint the user verified.
- Start read-only: `linf doctor --json`, `linf target list --json`, then `linf db list --json` and
  `linf bucket list --json`. Stop and report the failing check and its `remedy` if Docker is not usable. Do not
  work around it with other tools.
- Preview before mutating: run `engine ensure`, `db create`, `bucket create`, `db drop`, `bucket drop`,
  `engine rm`, and `backup restore` with `--plan`, show the plan and the exact commands, and get confirmation.
  `target add-local` has no `--plan`; name it explicitly when asking.
- Never pass passwords or secret keys as arguments. Never write generated credentials into the repository,
  the chat, or logs unless the user asks. `.env` files must stay untracked (`.gitignore`).
- `--yes` only for a destructive command the user explicitly authorized in this conversation. Never touch
  Docker resources `linf` does not manage (`linf discover` is read-only). Never run `linf reset` unprompted.
- SQL passwords are never argv values or connection-URI fields. Use a prompt, `--secret-stdin`, or
  `LINF_SQL_PASSWORD`. External profiles default to verified TLS, read-only access, no stored password, and no
  query text history. Writable external sessions require an explicit `read-write` profile plus exact name
  confirmation for that one session.
- Resource names come from command output, not guesses: project `acme` becomes database `acme_dev`, user
  `acme_user`, bucket `acme-dev`, but read `database.database_name` / `bucket.bucket_name` from the JSON.

## Decide what to do

| User asks for | Do |
| --- | --- |
| A dev database / `DATABASE_URL` | Preflight → select target → `engine ensure … postgres 17 --plan` → `db create --plan` → confirm → create → `db test` → offer `db env` |
| A bucket / S3 credentials | Same with `engine ensure … minio latest --plan` → `bucket create --plan` → `bucket test` → offer `bucket env` |
| Both for a new project | One confirmation covering both engine plans and both resource plans; create engines first, then resources |
| Connection values for an existing project | `db list --json` / `bucket list --json` to find it, then only the requested `db env`, `db url`, `bucket env`, `bucket endpoint` |
| Wire values into the app | Append `linf db env <db>` / `linf bucket env <bucket>` output to the project's untracked `.env`; never commit it |
| Reset / fresh data | Prefer `backup run` first, then `db drop --plan` + `db create --plan`, or `db duplicate` for a scratch copy |
| Lost or leaked password | `db rotate-password` / `bucket rotate-key`, then refresh the `.env` |
| Something is broken | `linf doctor --json`; on Linux "socket permission" means the user is not in the `docker` group |
| Query a managed PostgreSQL DB | Read-only preflight → `linf sql exec <database> --file - --output json`; use the TUI only when the user requests an interactive workspace |
| Add or use an external PostgreSQL profile | Default verified TLS/read-only/no stored secret → take password from stdin or prompt → `sql connection add` tests before saving → exact name confirmation for any writable session |

## Create resources (canonical flow)

1. Pick the target. Use the one the user named; otherwise the only registered local target. None registered →
   show `linf target add-local --name local`, confirm, run it. Several and none named → ask. Call it `<target>`.
2. Plan the engines (idempotent; reuses an existing container):
   ```sh
   linf engine ensure <target> postgres 17 --plan
   linf engine ensure <target> minio latest --plan
   ```
3. Plan the project resources:
   ```sh
   linf db create --target <target> --project <project> --plan
   linf bucket create --target <target> --project <project> --plan
   ```
4. Show both plans and the exact creation commands; wait for confirmation. Then run them without `--plan`,
   engines first, resources second, with `--json` so you can read the created names.
5. Verify with `linf db test <database>` and `linf bucket test <bucket>`.
6. Hand over values only on request: `linf db env <database>`, `linf bucket env <bucket>`. Say which
   variables were written (`DATABASE_URL`, `PG*`, `S3_*`, `AWS_*`) rather than echoing secrets.

## Existing infrastructure

- Inspect before changing anything. Reuse the target and engine a resource already belongs to; never create a
  second engine because a project name is new.
- A name conflict is reported, not overwritten. Offer to reuse the existing resource or pick another name.
- Engines bind to `127.0.0.1` by default. Do not change `--bind` unless the user asks for LAN access.

## Environment notes

- macOS and Linux (Ubuntu, Debian, Fedora, Arch/Omarchy) are supported. Credentials live in a `0600` local
  vault in the state directory, so the same values are recoverable from a terminal, an SSH session, or an agent.
- On Linux, Docker Engine must be running (`systemctl start docker`) and the user must be in the `docker` group.
  `linf doctor` distinguishes the two and prints the fix.
- Clipboard commands (`copy-url`, `copy-env`) need a terminal; from an agent prefer `env`/`url` to stdout.
