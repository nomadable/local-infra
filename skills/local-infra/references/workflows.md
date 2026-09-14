# `linf` workflows for coding agents

Each recipe assumes the preflight from `SKILL.md` already passed and a local target `<target>` was chosen.
Replace `<project>` with the app's short name (lowercase, letters, digits, `-`/`_`).

## 1. New project needs PostgreSQL

```sh
linf engine ensure <target> postgres 17 --plan
linf db create --target <target> --project <project> --plan
# show both plans, get confirmation, then:
linf engine ensure <target> postgres 17 --json
linf db create --target <target> --project <project> --json   # read .database.database_name
linf db test <database>                                       # the name read from that JSON
```

Then wire the app (see recipe 4). Do not create a second engine when `linf engine list --json` already shows a
running `postgres` engine on that target.

## 2. New project needs an S3 bucket

```sh
linf engine ensure <target> minio latest --plan
linf bucket create --target <target> --project <project> --plan
# confirm, then:
linf engine ensure <target> minio latest --json
linf bucket create --target <target> --project <project> --json   # read .bucket.bucket_name
linf bucket test <bucket>                                         # the name read from that JSON
```

SDK settings the app needs: path-style addressing (`S3_FORCE_PATH_STYLE=true`), region `us-east-1`, endpoint
from `linf bucket endpoint <bucket>`. The env block already carries the `AWS_*` variants for AWS SDKs.

## 3. Both at once

Plan all four commands, present one summary, confirm once, then run engines first and resources second.
A partial failure leaves the earlier resources in place; report what exists and continue from the failed step,
never redo `db create` for a name that now exists.

## 4. Put connection values into the project

```sh
grep -qx '.env' .gitignore || printf '.env\n' >> .gitignore
linf db env <database> >> .env
linf bucket env <bucket> >> .env
```

Tell the user which variable names were added, not their values. `db env` prints plain `KEY=value` lines even
with `--json`; if the framework wants different names (for example `DB_HOST`), rename them with a small script and
keep the file untracked. For a checked-in `.env.example`, write placeholder values only.

## 5. Reset the dev database

Safest order: backup, then drop and recreate, then re-run migrations/seeds.

```sh
linf backup run <database> --json           # keep the file path
linf db drop <database> --plan              # shows what goes away; it deletes the user too
linf db create --target <target> --project <project> --plan
```

Show both plans and stop. Only after the user explicitly approves the drop in this conversation, run
`linf db drop <database> --yes`, then `linf db create --target <target> --project <project> --json`. The new
database gets a new password: refresh `.env` (recipe 4). For a throwaway copy without touching the original, use
`linf db duplicate <database> <database>_scratch` instead.

## 6. Backup and restore

```sh
linf backup run <database> --out ./backups --json
linf backup list <database> --json
linf backup verify <id>
linf backup restore ./backups/<file>.dump --into <database> --overwrite --plan
```

`--overwrite` replaces existing data. Show the plan, stop, and run the same command with `--yes` instead of
`--plan` only after the user approves.

## 7. Rotate a leaked credential

```sh
linf db rotate-password <database> --json
linf bucket rotate-key <bucket> --json
```

Update `.env` afterwards and restart the app. Never paste the new secret into the chat.

## 8. Remote Docker host (only when the user explicitly asks)

```sh
linf target verify <host>                    # user compares the fingerprint out-of-band
linf target add-ssh --name <name> --host <host> --user <user> --fingerprint 'SHA256:…'
linf target test <name>
linf engine ensure <name> postgres 17 --plan
linf db create --target <name> --project <project> --plan
linf tunnel start <database>                 # then `linf db env <database>` shows the local endpoint
```

## 9. Troubleshooting with `linf doctor --json`

| Check name | Meaning | Fix to suggest |
| --- | --- | --- |
| `Docker CLI` not ok | `docker` binary missing | macOS: Docker Desktop. Arch: `sudo pacman -S docker`. Debian/Ubuntu: `sudo apt install docker.io` |
| `Docker 데몬 (…)` says socket permission | Daemon runs, user lacks access | `sudo usermod -aG docker $USER`, then re-login or `newgrp docker` |
| `Docker 데몬 (…)` cannot connect | Daemon stopped | macOS: start Docker Desktop. Linux: `sudo systemctl enable --now docker` |
| `비밀번호 저장소` not ok | Secret vault unusable | Check state-directory permissions; `LINF_STATE_DIR` for CI |
| `등록된 Target` not ok | No target yet | `linf target add-local --name local` after confirmation |
| Port already in use on `engine ensure` | Another PostgreSQL/MinIO on that port | `--port` with a free port, or reuse the existing engine |
| `db test` fails after a fresh create | Engine still starting | Retry once after a few seconds; then `linf engine logs <target> postgres 17` |

`LINF_STATE_DIR=<dir>` isolates state, config, backups, and the vault for CI or experiments.
