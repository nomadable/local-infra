# `linf` command reference for agents

Global flags on every command: `--json` (machine-readable stdout, always prefer it) and `-y`/`--yes` (skip the
confirmation of a destructive command; only with explicit user authorization). Bare `linf` opens the TUI: never run
it from an agent. With `--json`, a failure prints `{ok:false, kind, error:{what, cause, next, command?, output?}}` on
**stderr** (stdout stays empty) and exits non-zero, so read the diagnostic from stderr under `.error`.

Value commands (`db url`, `db env`, `bucket url`, `bucket endpoint`, `bucket env`) always print plain text, even
with `--json`: one URL, or `KEY=value` lines. Everything they print contains the live secret.

`<target>`, `<database>`, `<bucket>` accept a display name or id. Names are validated at parse time.

## Preflight

| Command | Purpose | Notes |
| --- | --- | --- |
| `linf doctor --json` | State directory, secret store, Docker CLI/daemon, targets, terminal checks | Array of `{name, ok, detail, remedy}`. Stop on any `ok: false` that concerns Docker. |
| `linf target list --json` | Registered targets and reachability | `[{target:{id, kind:"local"|"ssh", display_name, docker_command, …}, reachable, docker, detail}]` |
| `linf target add-local [--name local] [--docker docker]` | Register this machine's Docker | No `--plan`; confirm explicitly. |
| `linf target test <target>` | Test SSH and Docker permission separately | |
| `linf target verify <host>` | Print the SSH host-key fingerprint before registering | |
| `linf target add-ssh --name <n> --host <h> --user <u> --fingerprint 'SHA256:…' [--port 22] [--identity path] [--docker docker]` | Register a remote Docker host | `--fingerprint` is mandatory non-interactively. Only with explicit user request. |
| `linf target forget <target>` | Unregister only; Docker resources untouched | |
| `linf discover <target>` | Read-only list of containers `linf` does not manage | Never modify what it shows. |

## Engines (shared containers, one per target and engine)

| Command | Purpose |
| --- | --- |
| `linf engine ensure <target> [postgres|minio] [version] [--port N] [--bind 127.0.0.1] [--image img] --plan` | Create if missing, otherwise reuse. Defaults: `postgres 17`, `minio latest`, first free port from 5432 / 9000. |
| `linf engine list --json` | `[{engine:{id, engine, major_version, image, container_name, volume_name, bind_address, host_port, console_port, admin_user, managed}, target:{…}, status:{exists, running, state, health}}]` |
| `linf engine start <target> [engine] [version]`, `linf engine stop <target> [engine] [version]`, `linf engine restart <target> [engine] [version]` | Lifecycle |
| `linf engine logs <target> [engine] [version]` | Container logs |
| `linf engine rm <target> [engine] [version] [--volume] --plan` | Destructive. `--volume` deletes all data of every project on that engine. |

Container names are `linf-postgres-<major>` and `linf-minio-<version>`; volumes `linf-pg<major>-data` and
`linf-minio-<version>-data`. Do not manage them with `docker` directly.

## Databases (PostgreSQL)

| Command | Purpose |
| --- | --- |
| `linf db create --target <target> --project <project> [--name db] [--user user] [--engine postgres] [--version 17] [--encoding UTF8] [--locale C] [--tunnel-port N] --plan` | Creates `<project>_dev` and `<project>_user` (lowercase, `_`), a random password, and grants ownership. |
| `linf db list --json` | `[{database:{id, project_name, database_name, username, credential_ref, last_connection_test_at, last_backup_at}, engine:{…}, target:{…}}]` |
| `linf db url <database>` | `postgresql://user:password@127.0.0.1:port/db` on stdout |
| `linf db env <database>` | `.env` block: `DATABASE_URL`, `PGHOST`, `PGPORT`, `PGDATABASE`, `PGUSER`, `PGPASSWORD` |
| `linf db test <database>` | Real connection test with the stored credentials |
| `linf db duplicate <database> <new_name>` | Copy on the same engine (scratch or snapshot) |
| `linf db rotate-password <database>` | New password; refresh every `.env` afterwards |
| `linf db drop <database> --plan` | Destructive: drops DB and its user |
| `linf db forget <database>` | Unregister only; data stays in PostgreSQL |
| `linf db copy-url <database>`, `linf db copy-env <database>` | Clipboard; needs a terminal, avoid from an agent |

`db create --json` returns `{database:{…}, engine:{…}, url, redacted_url}`. Read `database.database_name` for
later commands. `url` embeds the plaintext password: never echo it; show `redacted_url` instead.
`db rotate-password --json` and `db duplicate --json` return the redacted URL and the new `database` record
respectively; fetch the new secret with `linf db env <database>` when the user needs it.

## Buckets (MinIO, S3-compatible)

| Command | Purpose |
| --- | --- |
| `linf bucket create --target <target> --project <project> [--name bucket] [--access-key KEY] [--version latest] [--region us-east-1] [--tunnel-port N] --plan` | Creates `<project>-dev` (DNS label, `-`), a per-bucket user whose policy only reaches that bucket, and a random secret key. |
| `linf bucket list --json` | `[{bucket:{id, project_name, bucket_name, access_key, credential_ref, …}, engine:{…}, target:{…}}]` |
| `linf bucket url <bucket>` | `s3://…` connection string |
| `linf bucket endpoint <bucket>` | `http://127.0.0.1:9000` (path-style) |
| `linf bucket env <bucket>` | `.env` block: `S3_ENDPOINT`, `S3_BUCKET`, `S3_REGION`, `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY`, `S3_FORCE_PATH_STYLE=true`, plus `AWS_ENDPOINT_URL_S3`, `AWS_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` |
| `linf bucket test <bucket>` | Real access test |
| `linf bucket rotate-key <bucket>` | New secret key |
| `linf bucket drop <bucket> --plan` | Destructive: deletes bucket, objects, and its user |
| `linf bucket forget <bucket>` | Unregister only |

`bucket create --json` returns `{bucket:{…}, engine:{…}, endpoint, url, redacted_url}`. Read `bucket.bucket_name`.
`url` embeds the secret key: never echo it; show `redacted_url` or `endpoint` instead.

The MinIO console is on `console_port` (default 9001) with the engine's admin user; project apps must use the
per-bucket key from `bucket env`, never the admin credentials.

## Backups

| Command | Purpose |
| --- | --- |
| `linf backup run <database> [--out dir] [--format custom]` | `pg_dump` inside the container, streamed to a local file with a SHA-256 checksum |
| `linf backup list [database] --json` | `[{id, resource_kind, file_name, storage_location, format, size, checksum, status, created_at}]` |
| `linf backup verify <id>` | Re-checks the file checksum |
| `linf backup restore <file> --into <database> [--overwrite] --plan` | `pg_restore` into an existing DB; `--overwrite` is destructive |

## Tunnels (remote targets only)

| Command | Purpose |
| --- | --- |
| `linf tunnel start <database>`, `linf tunnel stop <database>`, `linf tunnel restart <database>` | SSH port-forward for one remote DB |
| `linf tunnel start-all` | Every remote resource at once |
| `linf tunnel status --json` | Active tunnels with local ports |

Local targets never need a tunnel; `db env` on a remote resource only shows an endpoint while its tunnel is up.

## Maintenance

| Command | Purpose |
| --- | --- |
| `linf update` | Self-update from the latest GitHub Release (not for source builds) |
| `linf skill install [--agent claude|codex|cursor|gemini|copilot] [-g] [--dir path] [--force]` | (Re)install this skill |
| `linf completions <shell>` | Shell completion script |
| `linf reset` | Deletes every registration and every `linf`-managed container and volume. Never run without an explicit request. |
