# local-infra (`linf`)

`linf`는 하나의 Docker Target 안에서 PostgreSQL 데이터베이스와 MinIO 버킷을 프로젝트별로 분리해 관리하는 터미널 앱입니다. 로컬 Docker와 SSH 원격 Docker를 같은 명령 모델로 다루며, 접속 문자열·`.env`·백업·SSH 터널을 함께 관리합니다.

- PostgreSQL과 MinIO는 Target별 공유 엔진 컨테이너 하나씩만 사용
- DB/버킷마다 전용 사용자 또는 액세스 키 생성
- 접속 URL과 `.env` 블록을 stdout 또는 클립보드로 제공
- SSH Target, 호스트 키 지문 확인, 로컬 터널, 스트리밍 백업/복원
- 기본 TUI와 자동화용 JSON 출력

## 요구 사항

| 환경 | 필요 항목 |
| --- | --- |
| macOS 13+ | Docker Desktop, OpenSSH, 80×24 이상 터미널 |
| Linux (Ubuntu 22.04+ x86_64) | Docker CLI/daemon, OpenSSH, Secret Service 또는 파일 기반 secret 저장소, 80×24 이상 터미널 |
| 원격 Target | SSH 접근 권한과 대상 호스트의 Docker CLI 권한 |

`psql`, `pg_dump`, `pg_restore`, `mc`를 호스트에 설치할 필요는 없습니다. 엔진 컨테이너 안의 도구를 사용합니다.

Docker daemon은 실행 중이어야 합니다. `linf doctor`가 Docker CLI와 daemon 상태를 먼저 검사합니다. 원격 Docker 사용자는 대상 서버에서 Docker를 실행할 권한이 있어야 합니다.

## 설치

### One-command install — 권장

```sh
curl -fsSL https://apps.nomadable.io/local-infra/install | bash
```

installer는 운영체제와 CPU를 감지하고, archive와 SHA-256을 검증한 뒤
`${LINF_INSTALL_DIR:-$HOME/.local/bin}/linf`에 설치합니다. root로 실행하지 않으며,
macOS Apple Silicon/Intel과 Linux x86_64 (Ubuntu 22.04+)를 지원합니다.

특정 버전을 설치하거나 기본 설치 경로를 바꾸려면 installer 인자를 전달하세요.

```sh
curl -fsSL https://apps.nomadable.io/local-infra/install | bash -s -- \
  --version vX.Y.Z --install-dir "$HOME/.local/bin"
```

### 최신 버전으로 업데이트

공식 installer로 설치한 `linf`는 현재 설치 경로에 최신 GitHub Release를 다시 설치합니다.

```sh
linf update
linf --version
```

소스 빌드(`target/debug` 또는 `target/release`)는 실수로 덮어쓰지 않도록 거부합니다.
그 경우에는 `cargo install --path . --locked`로 다시 설치하세요.

### GitHub Release archive — 수동 설치

[Releases](https://github.com/nomadable/local-infra/releases)에서 운영체제와 CPU에 맞는 archive를 받습니다.

| 시스템 | asset 이름 |
| --- | --- |
| macOS Apple Silicon | `linf-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `linf-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 (Ubuntu 22.04+) | `linf-x86_64-unknown-linux-gnu.tar.gz` |

```sh
# 예: macOS Apple Silicon
TARGET=aarch64-apple-darwin
ASSET="linf-${TARGET}"

tar -xzf "${ASSET}.tar.gz"
mkdir -p "$HOME/.local/bin"
install -m 0755 ./linf-*-"${TARGET}"/linf "$HOME/.local/bin/linf"

# ~/.local/bin 이 PATH에 없다면 셸 설정에 한 번 추가
export PATH="$HOME/.local/bin:$PATH"
linf --version
```

각 archive 옆의 `.sha256` 파일로 다운로드 무결성을 확인할 수 있습니다.

### 소스에서 설치

Rust 1.88 이상이 있다면 저장소에서 재현 가능한 lockfile 설치를 할 수 있습니다.

```sh
cargo install --path . --locked
linf --version
```

crates.io 배포가 시작된 뒤에는 다음 설치 경로도 지원합니다.

```sh
cargo install local-infra --locked
```

### Agent Skill로 로컬 인프라 구성

`linf`를 설치한 뒤 Agent Skill을 프로젝트에 등록하면, agent에게 로컬 개발용
PostgreSQL DB와 MinIO 버킷 구성을 요청할 수 있습니다.

```sh
# 현재 프로젝트: ./.agents/skills/local-infrastructure/SKILL.md
linf skill install

# 현재 사용자 전역: ~/.agents/skills/local-infrastructure/SKILL.md
linf skill install -g
```

`.agent`가 아니라 `.agents/skills`(복수형)를 사용합니다. 이는 Agent Skills 호환 도구가
프로젝트·사용자 전역에서 함께 탐색하는 경로입니다. 특정 도구가 자체 skill 경로만
탐색한다면 그 경로를 `--dir`로 명시할 수 있습니다.

```sh
linf skill install --dir .claude/skills
```

등록 뒤에는 새 agent 세션에서 다음처럼 요청합니다.

```text
acme용 로컬 PostgreSQL과 MinIO를 구성해줘.
```

Skill은 바로 Docker 명령을 조합하지 않습니다. 먼저 `linf doctor`, Target·엔진 목록을
읽고, 사용할 로컬 Target을 선택합니다. Target이 없으면 등록 명령을 먼저 보여 주고
확인을 받습니다. 이후 엔진과 DB·버킷의 `--plan` 결과 및 실제 생성 명령을 보여 준 뒤,
명시적 확인 후에만 엔진 → 프로젝트 리소스 순서로 생성하고 실제 접속을 검증합니다.

기본적으로 로컬 Docker만 대상으로 하며, 원격 Target을 추론하지 않습니다. 비밀값은
명령 인자로 전달하지 않고, 요청하지 않은 `.env` 값도 채팅·저장소·로그에 출력하지
않습니다. raw Docker/Compose, `psql`, `mc` 대신 항상 `linf` CLI를 사용합니다.

기존 Skill은 덮어쓰지 않습니다. 새 `linf` 버전의 지침으로 명시적으로 교체할 때만
`--force`를 사용하세요.

```sh
linf skill install --force
```

## 5분 시작

```sh
# 1. 실행 환경 확인
linf doctor

# 2. 현재 컴퓨터의 Docker를 Target으로 등록
linf target add-local --name local
linf target test local
# 3. PostgreSQL 공유 엔진 준비 — 이미 있으면 재사용
linf engine ensure local postgres 17

# 4. 프로젝트 DB와 전용 계정 생성
linf db create --target local --project acme

# 5. 앱이나 셸에서 쓸 연결 정보 출력
linf db env acme_dev
```

MinIO 버킷도 같은 Target에 만듭니다.

```sh
linf engine ensure local minio latest
linf bucket create --target local --project acme
linf bucket env acme-dev
```

명령 없이 실행하면 TUI가 열립니다.

```sh
linf
```

## 원격 Docker Target

원격 호스트의 지문은 서버 운영자가 확인한 값과 대조한 뒤 등록합니다. 비대화형 사용에서는 `--fingerprint`가 필수입니다.

```sh
# 지문을 먼저 조회하고, 신뢰할 수 있는 경로로 값을 대조
linf target verify vps.example.com

# 대조한 SHA256 지문을 명시해 등록
linf target add-ssh \
  --name prod-vps \
  --host vps.example.com \
  --user deploy \
  --fingerprint 'SHA256:…'

# SSH와 원격 Docker 권한을 각각 검사
linf target test prod-vps
```

`linf`는 SSH 암호를 저장하지 않습니다. 기본적으로 `ssh-agent` 또는 `~/.ssh/config`와 개인키 경로를 사용합니다.

## 자주 쓰는 명령

```sh
# 현재 상태
linf target list
linf engine list
linf db list
linf bucket list

# 앱에 넣을 값
linf db url acme_dev
linf db env acme_dev
linf bucket endpoint acme-dev
linf bucket env acme-dev

# 검증과 운영
linf db test acme_dev
linf bucket test acme-dev
linf engine logs local postgres 17

# 계획만 미리 확인
linf db create --target local --project demo --plan
linf engine ensure local postgres 17 --plan
```

파괴적 작업은 기본적으로 명시적인 확인이 필요합니다. 자동화에서만 내용을 확인한 뒤 `--yes`를 사용하세요.

```sh
linf db drop acme_dev --yes
linf engine rm local postgres 17 --volume --yes
```

`reset`은 등록 정보와 앱이 만든 Docker 리소스를 제거합니다. 실제 사용 중인 데이터가 없는지 반드시 확인하세요.

## 셸 자동완성

```sh
# zsh
linf completions zsh > "${fpath[1]}/_linf"

# bash — 시스템별 completion 디렉터리 또는 직접 source
linf completions bash > /tmp/linf.bash
source /tmp/linf.bash

# fish
linf completions fish > ~/.config/fish/completions/linf.fish
```

## 설정과 상태 파일

설정은 TUI에서 편집하지 않습니다. `config.toml`은 의도적으로 파일 기반 설정이며, Doctor 화면은 적용된 진단 결과만 보여줍니다.

| 플랫폼 | 설정 | 상태·백업·터널 PID |
| --- | --- | --- |
| macOS | `~/Library/Application Support/local-infra/config.toml` | `~/Library/Application Support/local-infra/` |
| Linux | `${XDG_CONFIG_HOME:-~/.config}/local-infra/config.toml` | `${XDG_STATE_HOME:-~/.local/state}/local-infra/` |

`LINF_STATE_DIR`을 설정하면 상태, 설정, 백업, PID 파일을 모두 한 디렉터리에 격리합니다. CI, 테스트, 일회성 실험에 유용합니다.

Linux에서 `secrets.mode = "file"`을 사용할 때는 시작 전에 `LINF_PASSPHRASE`를 제공해야 합니다. 기본 `keyring` 모드는 Secret Service를 사용하며, 사용할 수 없으면 제한 모드로 전환됩니다.

```toml
# config.toml
[secrets]
# keyring | file | none
mode = "keyring"

[tunnel]
keep_alive_on_exit = true
port_range_start = 15432
port_range_span = 200

[ui]
osc52 = true
reduced_motion = false
ascii = false
clipboard_clear_seconds = 45

[general]
# backup_dir = "~/Backups/local-infra"
# image_prefix = "docker.io/library"
```

## 자동화

데이터를 읽거나 조작하는 headless 명령은 `--json`을 지원합니다. bare `linf`는 TUI를 열고, `completions`는 셸 스크립트를 출력하므로 JSON 대상이 아닙니다. 스크립트에서는 사람이 읽는 표 대신 JSON과 종료 코드를 사용하세요.

```sh
linf db list --json
linf tunnel status --json
```

## 개발과 검증

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked

# Docker가 실행 중인 로컬 환경에서만: 실제 엔진·DB·버킷·백업 E2E
scripts/e2e-local.sh
```

E2E 스크립트는 전용 임시 상태 디렉터리를 쓰고, 기본적으로 종료 시 정리합니다. `linf-postgres-17`, `linf-minio-latest` 컨테이너나 같은 이름의 volume이 이미 있으면 **삭제하지 않고 즉시 중단**합니다. 공유 Docker daemon이나 실제 리소스가 있는 환경에서는 실행하지 마세요. 결과를 남겨 조사하려면 `KEEP=1 scripts/e2e-local.sh`를 사용합니다.

## 배포

`vX.Y.Z` annotated tag를 push하면 GitHub Actions가 검증, 플랫폼별 release archive, SHA-256 파일, GitHub Release를 생성합니다. 릴리스 전에는 다음을 확인합니다.

1. `Cargo.toml` 버전을 변경한다.
2. `cargo fmt --check`, clippy, test를 통과시킨다.
3. 사용자에게 보이는 변경 사항을 GitHub Release notes에서 검토한다.
4. `git tag -a vX.Y.Z -m "vX.Y.Z" && git push origin vX.Y.Z`를 실행한다.

crates.io publish와 Homebrew tap은 각각 crate 소유권·저장소 URL 및 tap 관리자가 정해진 뒤 별도로 추가합니다.

## License

MIT. [LICENSE](LICENSE)를 참고하세요.
