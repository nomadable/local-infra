# local-infra 제품 요구사항 문서(PRD)

- **문서 상태:** Draft v0.2
- **제품명:** local-infra
- **실행 명령:** `linf`
- **제품 유형:** 로컬·원격 개발용 인프라 관리 터미널 애플리케이션(TUI + 헤드리스 CLI)
- **작성일:** 2026-09-01
- **주요 대상:** 여러 프로젝트를 동시에 개발하는 개인 개발자 및 소규모 개발팀
- **기반 문서:** 데스크톱 앱 안의 개발용 DB 관리 PRD — 동일한 문제 정의를 터미널 환경으로 재설계하고, 관리 범위를 데이터베이스에서 오브젝트 스토리지까지 넓혔다

---

## 1. 목적

local-infra는 프로젝트마다 별도의 데이터베이스·스토리지 컨테이너를 만들면서 발생하는 로컬·VPS 리소스 낭비와 운영 복잡도를 줄인다.

데스크톱 GUI가 아닌 **터미널 애플리케이션**으로 제공한다. 개발자는 이미 터미널에 있고, 관리 대상인 Docker와 SSH도 터미널 도구다. GUI 창을 띄우지 않고 셸에서 즉시 실행하며, 원격 VPS에 SSH로 들어가 동일한 바이너리를 쓸 수 있어야 한다.

사용자는 하나의 TUI에서 다음 환경을 동일한 방식으로 관리할 수 있어야 한다.

1. 자신의 컴퓨터에서 실행되는 로컬 Docker
2. SSH로 접근 가능한 개발용 VPS의 원격 Docker
3. 필요 시 Tailscale 네트워크 안에 있는 VPS

제품은 서비스 종류와 메이저 버전별로 공유 컨테이너를 하나씩 운영하고, 그 안에 프로젝트별 독립 리소스를 생성한다. PostgreSQL 엔진에서는 프로젝트마다 데이터베이스와 로그인 계정을, MinIO 엔진에서는 프로젝트마다 버킷과 그 버킷만 접근하는 액세스 키를 만든다.

```text
local-infra (linf)
├── Local Docker
│   ├── PostgreSQL 17
│   │   ├── letsbid_dev
│   │   └── tamche_dev
│   └── MinIO
│       ├── letsbid-assets
│       └── tamche-uploads
└── Development VPS over SSH/Tailscale
    ├── PostgreSQL 17
    │   ├── parantica_dev
    │   └── dalbit_editor_dev
    └── MinIO
        ├── parantica-media
        └── dalbit-editor-assets
```

---

## 2. 배경과 문제 정의

### 2.1 현재 문제

- 프로젝트 수에 비례해 DB·스토리지 프로세스와 컨테이너가 증가한다.
- 유휴 프로젝트의 인프라도 메모리와 디스크를 계속 사용한다.
- 포트 충돌을 피하기 위해 프로젝트마다 다른 포트를 기억해야 한다.
- 상태, 로그, 볼륨과 접속 정보를 여러 Compose 프로젝트에서 찾아야 한다.
- 로컬과 VPS의 관리 방식이 달라 같은 작업을 반복한다.
- 프로젝트를 정리할 때 컨테이너, 볼륨, 네트워크가 고아 리소스로 남기 쉽다.
- 원격 DB나 오브젝트 스토리지를 쓰기 위해 공인 포트에 노출하면 보안 위험이 커진다.

### 2.2 터미널 제품이어야 하는 이유

- 작업 맥락이 이미 터미널이다. GUI 전환은 컨텍스트 스위칭 비용이다.
- 관리 대상 인터페이스(`docker`, `ssh`, `psql`, `pg_dump`, `mc`)가 모두 CLI다.
- SSH로 접속한 헤드리스 VPS에서도 같은 도구를 그대로 쓸 수 있다.
- 접속 문자열과 S3 엔드포인트 생성은 셸·`.env`·direnv와 파이프로 연결되어야 가치가 크다.
- 단일 바이너리 배포가 가능하고 상시 리소스 사용이 사실상 없다.

### 2.3 핵심 문제

> 개발자가 Docker와 인프라 관리 명령을 직접 조합하지 않아도, 터미널을 벗어나지 않고 로컬 또는 원격 개발 서버에서 공유 인프라 엔진을 안전하게 만들고 프로젝트별 데이터베이스와 버킷을 독립적으로 운영할 수 있어야 한다.

### 2.4 제품 원칙

1. 서버 프로세스는 공유하고 데이터 경계는 분리한다.
2. 프로젝트별 리소스와 전용 자격 증명을 기본 단위로 사용한다. PostgreSQL은 DB + 계정, MinIO는 버킷 + 버킷 전용 액세스 키다.
3. 로컬과 원격을 같은 UX로 제공한다.
4. 원격 Docker API 및 서비스 포트를 공인 인터넷에 노출하지 않는다.
5. 앱이 생성한 Docker 리소스만 수정한다.
6. 삭제보다 복구 가능성을 우선한다.
7. **모든 TUI 동작에는 대응하는 비대화형 CLI 명령이 있다.** TUI는 CLI 코어의 표현 계층이다.
8. **키보드만으로 완결한다.** 마우스는 선택적 보조 수단이다.
9. 초기 버전은 PostgreSQL과 MinIO에 집중하고, 다른 엔진은 그 뒤에 추가한다.

---

## 3. 목표와 비목표

### 3.1 MVP 목표

- 단일 바이너리 `linf`로 TUI를 실행한다.
- 로컬 Docker와 SSH 기반 원격 Docker를 Target으로 등록한다.
- Target마다 PostgreSQL 엔진과 MinIO 엔진 컨테이너를 생성하고 관리한다.
- PostgreSQL 컨테이너 하나에 여러 프로젝트의 DB와 계정을 생성한다.
- MinIO 컨테이너 하나에 여러 프로젝트의 버킷을 생성하고, 버킷마다 해당 버킷만 접근하는 전용 액세스 키를 발급한다.
- 프로젝트별 접속 문자열과 S3 엔드포인트를 생성하고, `.env` 블록·클립보드 복사·stdout 출력을 모두 지원한다.
- 원격 DB와 원격 버킷은 SSH 터널을 통해 로컬 앱, IDE, S3 SDK에서 안전하게 사용한다.
- DB 단위 백업·복원과 버킷 단위 오브젝트 아카이브 백업·복원을 수행한다.
- 컨테이너, 볼륨 및 리소스 작업의 영향 범위를 실행 전에 계획으로 보여준다.
- 모든 P0 동작을 `linf <subcommand>` 형태로 스크립트에서 실행한다.

### 3.2 MVP 비목표

- 프로덕션 DB·스토리지 운영 및 고가용성 관리
- Kubernetes 및 다중 VPS 클러스터
- PostgreSQL 복제와 자동 장애 조치, MinIO 분산 모드와 사이트 복제
- 기존 프로젝트 컨테이너의 무인 자동 병합
- 범용 SQL 편집기 및 결과 그리드, 범용 오브젝트 브라우저
- Docker 자체 설치 및 VPS 프로비저닝
- 팀 실시간 협업과 중앙 계정 시스템
- 웹 UI 및 데스크톱 GUI
- 마우스 중심 인터랙션, 드래그 앤 드롭
- MySQL, MongoDB, Redis 정식 지원

---

## 4. 대상 사용자

### 4.1 다중 프로젝트 개인 개발자

- 여러 서비스와 사이드 프로젝트를 동시에 개발한다.
- 터미널·tmux·에디터 안에서 하루 작업이 끝난다.
- Docker Compose 경험은 있지만 DB·스토리지 운영 명령을 반복하고 싶지 않다.
- 로컬 자원이 부족하거나 외부에서 동일한 환경을 쓰기 위해 VPS를 사용한다.
- 데이터 격리를 유지하면서 컨테이너 수를 줄이고 싶다.

### 4.2 소규모 팀 리더

- 팀 개발 VPS에서 여러 프로젝트의 DB와 버킷을 운영한다.
- VPS에 SSH로 들어가 상태를 직접 확인하는 일이 많다.
- 팀원이 관리자 계정이 아닌 프로젝트별 제한 계정과 버킷 전용 키를 사용하기를 원한다.
- 사용 중인 리소스, 연결 상태와 백업 상태를 한 화면에서 확인하고 싶다.

### 4.3 자동화 사용자(부차)

- 프로젝트 셋업 스크립트나 Makefile에서 DB·버킷 생성·삭제를 호출한다.
- TUI를 쓰지 않고 `linf db create --json`, `linf bucket create --json` 결과를 파싱한다.

---

## 5. 핵심 사용자 작업(JTBD)

1. 새 프로젝트 시작 시 터미널에서 몇 번의 키 입력으로 개발 DB와 업로드 버킷을 만들고 싶다.
2. 기존 엔진을 재사용해 컨테이너를 불필요하게 늘리고 싶지 않다.
3. 로컬과 VPS 중 원하는 위치를 선택해 같은 방식으로 관리하고 싶다.
4. 원격 서비스 포트를 공개하지 않고 로컬 IDE와 앱에서 접속하고 싶다.
5. 접속 URL과 S3 엔드포인트를 `.env`로 바로 흘려보내고 싶다.
6. 삭제 전에 영향 범위를 확인하고 백업하고 싶다.
7. 프로젝트의 DB와 버킷만 정리하면서 공유 엔진의 다른 리소스는 유지하고 싶다.
8. 문제가 생기면 상태, 로그와 해결 방법을 같은 화면에서 확인하고 싶다.

---

## 6. 권장 기술 구조

### 6.1 애플리케이션

| 영역 | 선택 | 설명 |
|---|---|---|
| 배포 형태 | 단일 정적 바이너리 `linf` | 설치 부담 없음, VPS 복사 배포 가능 |
| 언어 | Rust | 단일 바이너리, 프로세스·SSH 제어에 적합 |
| TUI | `ratatui` + `crossterm` | 위젯 기반 렌더링, 크로스 플랫폼 터미널 제어 |
| 비동기 런타임 | `tokio` | Docker/SSH 명령 및 터널 동시 실행 |
| CLI 파서 | `clap` | 헤드리스 서브커맨드와 TUI 진입점 공유 |
| 메타데이터 | SQLite(`rusqlite`) | Target, 엔진, DB, 버킷, 터널, 백업 및 활동 이력 |
| 비밀 저장 | OS Keyring(`keyring`) + 폴백 | Keychain/Secret Service, 불가 시 암호화 파일 |
| Docker | Docker CLI 호출 | 로컬 및 원격 명령 실행 |
| 엔진 클라이언트 | 컨테이너 안의 `psql`·`pg_dump`·`mc` | 호스트에 DB·S3 클라이언트를 설치하지 않는다 |
| 원격 연결 | 시스템 `ssh` | Docker 제어, 터널, 백업 스트림 |
| 클립보드 | OS 클립보드 + OSC 52 | SSH 원격 세션에서도 복사 지원 |

대안 검토: Go + Bubble Tea도 동일 요구를 만족한다. Rust를 택한 이유는 기반 문서의 시스템 계층 결정과의 연속성, 그리고 터널·프로세스 생명주기를 소유권 모델로 다루기 쉽다는 점이다. 이 선택은 §19.1에서 확정했다.

### 6.2 계층 구조

TUI는 코어 로직을 소유하지 않는다.

```text
linf binary
├── cli        clap 서브커맨드, --json, 종료 코드
├── tui        ratatui 화면, 키맵, 상태 머신
└── core       Target / Engine / Database / Bucket / Tunnel / Backup 유스케이스
    ├── docker  로컬·원격 명령 실행기
    ├── ssh     호스트 키 검증, 명령, 포트 포워딩
    ├── pg      컨테이너 안 psql·pg_dump 어댑터
    ├── minio   컨테이너 안 mc 어댑터
    └── store   SQLite + Keyring
```

CLI와 TUI는 동일한 `core` 유스케이스를 호출한다. 어떤 기능도 TUI에만 존재하지 않는다.

### 6.3 연결 추상화

```text
Target
├── LocalTarget
│   └── local docker command
└── SshTarget
    ├── SSH host verification
    ├── remote docker command
    ├── optional Tailscale address
    └── SSH local port forwarding
```

리소스 기능은 연결 방식과 분리한다. 엔진마다 어댑터가 하나씩 있고, 두 어댑터 모두 클라이언트 도구를 **엔진 컨테이너 안에서** 실행한다.

```text
pg  — docker exec … psql / pg_dump / pg_restore
├── create_database_and_role(x, engine, spec, password)
├── drop_database_and_role(x, engine, database, role)
├── verify_login(x, engine, database, user, password)
├── dump_argv(docker_bin, engine, database, format)
└── restore_argv(docker_bin, engine, database, format)

minio — docker exec … mc
├── create_bucket(x, engine, admin_pw, bucket)
├── create_scoped_user(x, engine, admin_pw, bucket, access_key, secret_key)
├── remove_bucket / remove_scoped_user
├── verify_access(x, engine, bucket, access_key, secret_key)
└── list_objects / cat_argv / pipe_argv
```

호스트에 `psql`, `pg_dump`, `pg_restore`, `mc`를 설치할 필요가 없다. 모든 클라이언트 도구는 엔진 이미지가 이미 제공하며, 원격 Target에서는 `ssh host -- docker exec …` 형태로 같은 명령이 실행된다. 백업 스트림은 원격 컨테이너의 stdout에서 로컬 파일로 곧바로 흐른다.

### 6.4 원격 연결 원칙

다음 방식은 금지한다.

- 인증 없는 Docker API를 `0.0.0.0:2375`에 공개
- PostgreSQL `5432`를 공인 인터페이스에 직접 공개
- MinIO S3 API `9000` 또는 콘솔 `9001`을 공인 인터페이스에 직접 공개
- SSH 호스트 키 검증 비활성화
- 설정 파일에 SSH 개인키 또는 비밀번호 평문 저장

원격 리소스는 외부에 공개하지 않고 앱이 SSH 터널을 연다.

```text
Local application
  └── 127.0.0.1:15432
        └── SSH or Tailscale tunnel
              └── VPS 127.0.0.1:5432
                    └── linf-postgres-17
```

```text
Local application (AWS SDK, mc, rclone)
  └── 127.0.0.1:19000
        └── SSH or Tailscale tunnel
              └── VPS 127.0.0.1:9000
                    └── linf-minio-latest
```

사용자는 원격 리소스도 로컬 주소로 이용한다.

```env
DATABASE_URL=postgresql://project_user:password@127.0.0.1:15432/project_dev
S3_ENDPOINT=http://127.0.0.1:19000
S3_BUCKET=project-assets
```

MinIO는 루프백 또는 SSH 터널 뒤에서 평문 HTTP로 제공한다. 터널이 이미 제공하는 보호를 인증서 관리 비용으로 다시 사지 않는다(§19.12).

### 6.5 터널 프로세스 소유권

TUI는 장기 실행 프로세스가 아니다. 터널이 TUI 종료와 함께 죽으면 개발이 끊긴다.

- 터널은 TUI 프로세스의 자식으로 두지 않고, 분리된 백그라운드 프로세스로 실행한다.
- 터널 상태는 SQLite와 PID 파일(상태 디렉터리의 `run/`)에 기록한다.
- TUI 시작 시 기록된 터널의 생존 여부를 확인해 실제 상태와 조정한다.
- 사용자는 `앱 종료 시 터널 유지` 정책을 설정에서 선택한다(기본값 유지, §19.7).
- `linf tunnel status`는 TUI 없이도 같은 상태를 보고한다.

---

## 7. 정보 구조와 화면 설계

### 7.1 레이아웃 원칙

- 단일 전체 화면 애플리케이션이다. 창 분할은 앱이 관리한다.
- 상단 상태 바 + 좌측 내비게이션 + 본문 + 하단 힌트 바로 구성한다.
- 최소 지원 크기는 80×24다. 그 이하에서는 좌측 내비게이션을 숨기고 본문만 남긴다.
- 목록은 좌측, 상세는 우측에 두는 마스터-디테일을 기본으로 한다.
- 폭이 100 미만이면 마스터-디테일을 단일 컬럼 스택으로 전환한다.
- 모든 파괴적 동작은 전용 모달에서 확인한다.

### 7.2 전역 내비게이션

숫자 키로 직접 이동한다.

| 키 | 화면 |
|---|---|
| `1` | Dashboard |
| `2` | Targets |
| `3` | Resources |
| `4` | Tunnels |
| `5` | Backups |
| `6` | Activity |
| `7` | Settings |

`Tab`/`Shift+Tab`은 화면 내 패널 포커스를 이동한다. `:` 는 커맨드 팔레트를 연다.

### 7.3 대시보드

```text
┌ local-infra ────────────────────── docker: ok · tunnels: 2 · 21:04 ┐
│ 1 Dashboard  2 Targets  3 Resources  4 Tunnels  5 Backups  6 Log   │
├────────────────────────────────────────────────────────────────────┤
│ TARGETS                                                            │
│                                                                    │
│ ● local          Local Machine            connected                │
│   ├ postgres 17   running   3 db       cpu 2%   mem 180MB          │
│   └ minio latest  running   2 bucket   cpu 1%   mem 120MB          │
│                                                                    │
│ ● dev-vps        vps.ts.net · tailscale   connected                │
│   ├ postgres 17   running   2 db       tunnels 1                   │
│   └ minio latest  running   2 bucket   tunnels 1                   │
│                                                                    │
│ ○ old-vps        203.0.113.10             ssh timeout              │
│                                                                    │
│ ALERTS                                                             │
│ ! backup failed  parantica_dev  2026-09-01 04:12                   │
├────────────────────────────────────────────────────────────────────┤
│ n new resource  t tunnel  r refresh  : command  ? help  q quit     │
└────────────────────────────────────────────────────────────────────┘
```

표시 항목:

- Target와 Docker 연결 상태
- 엔진별 실행 상태와 버전
- 엔진별 리소스 개수(DB 수, 버킷 수)와 활성 SSH 터널 수
- CPU, 메모리 및 디스크 요약
- 백업 실패 또는 연결 오류 알림

### 7.4 Target 추가

`Targets` 화면에서 `a`를 누르면 폼 모달이 열린다. 필드 간 이동은 `Tab`, 제출은 `Ctrl+S`, 취소는 `Esc`다.

#### 로컬 Target

- 표시 이름
- Docker CLI 자동 진단 결과(인라인)
- Docker Engine 버전
- 기본 데이터 저장 정책

#### 원격 Target

- 표시 이름
- SSH 호스트 또는 Tailscale 주소
- SSH 포트와 사용자명
- SSH Agent 또는 개인키 경로
- Docker 실행 명령
- 연결 테스트(`Ctrl+T`)

최초 연결에서는 호스트 키 지문을 모달에 표시하고 사용자가 명시적으로 승인해야 저장한다. 승인 없이 진행하는 경로는 없다.

```text
┌ SSH 호스트 키 확인 ────────────────────────────────┐
│ 호스트  vps.ts.net:22                              │
│ 타입    ed25519                                    │
│ 지문    SHA256:9pL2...q4Xk                         │
│                                                    │
│ 이 지문이 서버에서 확인한 값과 같습니까?           │
│                                                    │
│   [y] 승인하고 저장     [n] 취소                   │
└────────────────────────────────────────────────────┘
```

### 7.5 리소스 생성 흐름

전체 화면 마법사 대신 **단일 폼 + 실행 계획 미리보기**를 사용한다. 터미널에서는 다단계 페이지 전환보다 한 화면에서 값과 결과를 동시에 보는 편이 빠르다.

폼의 첫 필드는 리소스 종류다. 종류를 바꾸면 그 아래 필드와 실행 계획이 함께 바뀐다. 화면과 키는 두 종류에서 동일하다.

```text
┌ 새 리소스 · 데이터베이스 ────────────────────────────────────┐
│ 종류          [ 데이터베이스 ▾ ]                             │
│ Target        [ local ▾ ]                                    │
│ 엔진          [ postgres 17 ▾ ]   (없으면 생성)              │
│ 프로젝트명    [ Letsbid                    ]                 │
│ DB명          [ letsbid_dev                ]  ✓ 사용 가능    │
│ 사용자명      [ letsbid_user               ]  ✓ 사용 가능    │
│ 비밀번호      [ 자동 생성 ▾ ]                                │
│ 인코딩/로케일 [ UTF8 / C ▾ ]                                 │
│ 터널 자동시작 [ 해당 없음 (local) ]                          │
│                                                              │
│ 실행 계획                                                    │
│   1. postgres:17 이미지 확인                                 │
│   2. 컨테이너 linf-postgres-17 생성 (신규)                   │
│   3. 볼륨 linf-pg17-data 생성 (신규)                         │
│   4. 포트 5432 → 5432 바인딩 (127.0.0.1 전용)                │
│   5. DB letsbid_dev 및 계정 letsbid_user 생성                │
│   6. 접속 테스트                                             │
│                                                              │
│ Ctrl+S 실행   Ctrl+T 검증   Esc 취소                         │
└──────────────────────────────────────────────────────────────┘
```

```text
┌ 새 리소스 · 버킷 ────────────────────────────────────────────┐
│ 종류          [ 버킷 ▾ ]                                     │
│ Target        [ local ▾ ]                                    │
│ 엔진          [ minio latest ▾ ]   (없으면 생성)             │
│ 프로젝트명    [ Letsbid                    ]                 │
│ 버킷명        [ letsbid-assets             ]  ✓ 사용 가능    │
│ 액세스 키     [ 자동 생성 ▾ ]                                │
│ 시크릿 키     [ 자동 생성 ▾ ]                                │
│ 리전          [ us-east-1 ▾ ]                                │
│ 터널 자동시작 [ 해당 없음 (local) ]                          │
│                                                              │
│ 실행 계획                                                    │
│   1. minio/minio:latest 이미지 확인                          │
│   2. 컨테이너 linf-minio-latest 생성 (신규)                  │
│   3. 볼륨 linf-minio-latest-data 생성 (신규)                 │
│   4. 포트 9000 → 9000, 콘솔 9001 → 9001 (127.0.0.1 전용)     │
│   5. 버킷 letsbid-assets 생성                                │
│   6. 정책 linf-letsbid-assets 및 전용 액세스 키 생성·연결    │
│   7. 접근 테스트                                             │
│                                                              │
│ Ctrl+S 실행   Ctrl+T 검증   Esc 취소                         │
└──────────────────────────────────────────────────────────────┘
```

- DB명·사용자명은 입력 중 실시간으로 PostgreSQL 제약과 중복을 검증한다.
- 버킷명은 입력 중 실시간으로 S3 버킷명 규칙(3–63자, 소문자·숫자·`-`, 처음과 끝은 영숫자, IP 주소 형태 불가)과 중복을 검증한다.
- 실행 계획은 입력 변경에 따라 즉시 갱신되며, `신규`/`재사용` 여부를 명시한다.
- 실행 중에는 단계별 진행 로그를 같은 자리에 스트리밍하고 `Ctrl+C`로 취소 가능 여부를 표시한다.
- 생성 중 실패하면 그 전까지 만든 DB·계정 또는 버킷·정책·키를 되돌리고, 활동 로그에 롤백을 기록한다. 고아 리소스를 남기지 않는다.

### 7.6 리소스 목록과 상세

한 화면에서 데이터베이스와 버킷을 함께 본다. `KIND` 컬럼이 둘을 구분하고, 상세 패널은 종류에 따라 다른 항목을 보여준다.

```text
┌ Resources ────────────────────────────────────────────────────────────┐
│ TARGET    NAME               KIND    ENGINE         SIZE    TUNNEL    │
│ local     letsbid_dev        db      postgres 17    84 MB   -         │
│ local     letsbid-assets     bucket  minio latest   12 MB   -         │
│ local     tamche_dev         db      postgres 17    12 MB   -         │
│>dev-vps   parantica_dev      db      postgres 17    240 MB  :15432 ●  │
│ dev-vps   parantica-media    bucket  minio latest   1.2 GB  :19000 ●  │
│ dev-vps   dalbit_editor_dev  db      postgres 17    36 MB   stopped   │
├───────────────────────────────────────────────────────────────────────┤
│ parantica_dev                                        db · postgres 17 │
│ target     dev-vps (ssh · tailscale)                                  │
│ engine     linf-postgres-17 · running · healthy                       │
│ owner      parantica_user                                             │
│ url        postgresql://parantica_user:****@127.0.0.1:15432/...       │
│ created    2026-08-14   last backup  2026-08-31 03:00 ok              │
│ tunnel     active · 127.0.0.1:15432 → 5432 · pid 48122                │
│                                                                       │
│ y url복사  Y env복사  t 터널  c 접속테스트  b 백업  R 복원            │
│ p 비밀번호교체  d 복제  x 삭제  l 로그                                │
└───────────────────────────────────────────────────────────────────────┘
```

버킷을 선택하면 상세 패널이 S3 엔드포인트, 마스킹된 액세스 키, 오브젝트 수로 바뀐다.

```text
┌ Resources · 버킷 상세 ───────────────────────────────────────────────────┐
│ parantica-media                                     bucket · minio latest│
│ target     dev-vps (ssh · tailscale)                                     │
│ engine     linf-minio-latest · running · healthy                         │
│ endpoint   http://127.0.0.1:19000  (터널)      console :9001             │
│ bucket     parantica-media   region us-east-1                            │
│ access key AKIA****************   policy linf-parantica-media            │
│ objects    12,480개 · 1.2 GB                                             │
│ created    2026-08-14   last backup  2026-08-31 03:10 ok                 │
│ tunnel     active · 127.0.0.1:19000 → 9000 · pid 48311                   │
│                                                                          │
│ y url복사  Y env복사  t 터널  c 접근테스트  b 백업  R 복원               │
│ p 키교체  x 삭제  l 로그                                                 │
└──────────────────────────────────────────────────────────────────────────┘
```

- 비밀번호와 시크릿 키는 기본적으로 마스킹하고, `s`로 일시 표시한다. 액세스 키는 접두부만 남기고 마스킹한다.
- `y`는 접속 URL 또는 S3 연결 문자열을, `Y`는 `.env` 블록 전체를 복사한다. SSH 세션에서는 OSC 52로 전달한다.
- 통계(크기, 연결 수, 오브젝트 수)는 조회 실패 시 오류가 아니라 빈 값으로 표시한다. 목록은 엔진이 멈춰 있어도 열린다.
- `/`로 필터, `Target`·종류·상태별 정렬을 지원한다.

### 7.7 터널 화면

- 활성·중지·실패 터널 목록
- 리소스 이름과 종류, 로컬 포트, 원격 포트, Target, PID, 연결 시각
- `s` 시작, `S` 중지, `r` 재연결, `a` 모든 터널 시작
- 연결 중단은 즉시 상태와 알림 배지로 반영한다.

### 7.8 활동 로그

- 시간, Target, 리소스, 동작, 결과를 시간순으로 표시한다.
- 각 항목을 펼치면 실행된 단계와 롤백 여부를 확인한다.
- 비밀 값은 항상 마스킹된 형태로만 기록한다.
- `Y`로 진단 정보를 복사해 이슈에 붙일 수 있다.

### 7.9 삭제 확인

| 작업 | 영향 범위 |
|---|---|
| 앱 등록 해제 | 메타데이터만 제거, 실제 DB·버킷 유지 |
| DB 삭제 | 선택 DB와 전용 계정 제거 |
| 버킷 삭제 | 선택 버킷의 모든 오브젝트, 전용 액세스 키와 정책 제거 |
| 엔진 컨테이너 삭제 | 해당 엔진의 모든 리소스 중단 |
| 볼륨 삭제 | 해당 엔진의 모든 데이터 영구 삭제 |

```text
┌ 볼륨 삭제 ───────────────────────────────────────┐
│ ! 이 작업은 되돌릴 수 없습니다.                  │
│                                                  │
│ 볼륨   linf-pg17-data (dev-vps)                  │
│ 영향   3개 DB의 모든 데이터가 영구 삭제됩니다    │
│        parantica_dev, dalbit_editor_dev, ...     │
│ 백업   최근 백업 2026-08-31 (parantica_dev만)    │
│                                                  │
│ [b] 먼저 전체 백업하기 (권장)                    │
│                                                  │
│ 삭제하려면 볼륨 이름을 입력하세요                │
│ > [                                    ]         │
│                                                  │
│ Ctrl+S 삭제(입력 일치 시 활성)   Esc 취소        │
└──────────────────────────────────────────────────┘
```

DB, 버킷, 볼륨 삭제는 이름을 직접 입력해야 하며, 확인 모달에서는 기본 포커스를 취소에 둔다. `Enter` 단독으로 파괴적 동작이 실행되는 경로는 없다.

### 7.10 커맨드 팔레트

`:`로 열고 모든 동작을 이름으로 실행한다. 키맵을 외우지 않아도 기능 전체에 접근할 수 있어야 한다.

```text
: db create
: db copy-url letsbid_dev
: bucket create
: bucket copy-env letsbid-assets
: bucket rotate-key parantica-media
: tunnel start parantica_dev
: engine restart local minio latest
: backup run parantica-media
: target test dev-vps
```

팔레트 명령 이름은 CLI 서브커맨드와 1:1로 일치한다. 항목의 내부 식별자는 서브커맨드 경로를 `.`로 이은 이름(`db.create`, `bucket.rotate-key`, `engine.restart`)이며, 설정 파일에서 키맵을 재정의할 때 쓰는 이름과 같다.

### 7.11 기본 키맵

| 키 | 동작 |
|---|---|
| `q` / `Ctrl+C` | 종료(진행 중 작업이 있으면 확인) |
| `?` | 도움말 오버레이 |
| `:` | 커맨드 팔레트 |
| `1`–`7` | 화면 전환 |
| `Tab` / `Shift+Tab` | 패널 포커스 이동 |
| `j` / `k` / `↓` / `↑` | 목록 이동 |
| `g` / `G` | 목록 처음·끝 |
| `Enter` | 상세 열기 |
| `/` | 필터 |
| `r` | 현재 화면 새로 고침 |
| `n` | 새 리소스(DB 또는 버킷) |
| `a` | 현재 화면의 추가 동작 |
| `y` / `Y` | 복사 / 확장 복사 |
| `x` | 삭제(확인 모달) |
| `Esc` | 모달·필터 취소 |

- `vim` 스타일과 화살표 키를 동시에 지원한다.
- 키맵은 설정 파일에서 재정의할 수 있다.
- 하단 힌트 바는 현재 포커스에서 유효한 키만 표시한다.

---

## 8. 기능 요구사항

- **P0:** MVP 필수
- **P1:** MVP 직후
- **P2:** 향후 확장

요구사항 ID는 한 번 부여하면 바뀌지 않는다. 새 요구사항은 해당 표의 끝에 추가한다.

### 8.1 Target 관리

| ID | 우선순위 | 요구사항 |
|---|---:|---|
| TAR-001 | P0 | 로컬 Docker를 Target으로 등록한다. |
| TAR-002 | P0 | Docker CLI 설치 및 Engine 상태를 진단한다. |
| TAR-003 | P0 | SSH VPS를 Target으로 등록한다. |
| TAR-004 | P0 | 원격 SSH와 Docker 권한을 각각 테스트한다. |
| TAR-005 | P0 | 최초 연결에서 SSH 호스트 키 지문 승인을 받는다. |
| TAR-006 | P0 | SSH Agent와 개인키 경로 인증을 지원한다. |
| TAR-007 | P0 | 비밀 값은 OS Keyring에 저장한다. |
| TAR-008 | P0 | Target을 수정하거나 등록 해제한다. |
| TAR-009 | P1 | Tailscale IP 및 MagicDNS 이름을 SSH 호스트로 사용한다. |
| TAR-010 | P1 | Target별 Docker 리소스와 디스크 사용량을 표시한다. |
| TAR-011 | P1 | 기존 `~/.ssh/config` 호스트 항목을 불러와 Target 등록을 채운다. |

### 8.2 엔진 관리

| ID | 우선순위 | 요구사항 |
|---|---:|---|
| ENG-001 | P0 | Target에 PostgreSQL 엔진 컨테이너를 생성한다. |
| ENG-002 | P0 | 메이저 버전별 컨테이너와 볼륨을 분리한다. |
| ENG-003 | P0 | 엔진 시작, 중지, 재시작을 지원한다. |
| ENG-004 | P0 | 로그와 healthcheck 상태를 표시한다. |
| ENG-005 | P0 | 생성한 리소스에 관리 label을 부여한다. |
| ENG-006 | P0 | 관리 label 없는 리소스를 임의 변경하지 않는다. |
| ENG-007 | P0 | 포트 충돌을 검사하고 대체 포트를 제안한다. |
| ENG-008 | P0 | 엔진 포트를 기본적으로 `127.0.0.1`에만 바인딩한다. |
| ENG-009 | P1 | CPU와 메모리 제한을 설정한다. |
| ENG-010 | P1 | 안전한 마이너 이미지 업데이트를 제안한다. |
| ENG-011 | P2 | MySQL, MariaDB 및 Redis 어댑터를 지원한다. |
| ENG-012 | P0 | Target에 MinIO 엔진 컨테이너를 생성한다. |
| ENG-013 | P0 | MinIO 콘솔 포트(`9001`)를 S3 API 포트와 별도로 발행하고, 두 포트 모두 기본적으로 `127.0.0.1`에만 바인딩한다. |

필수 labels:

```yaml
labels:
  local-infra.managed: "true"
  local-infra.target-id: "<target-id>"
  local-infra.engine: "postgres"        # 또는 "minio"
  local-infra.major-version: "17"       # MinIO는 "latest"
```

기본 리소스 이름 규칙:

```text
container  linf-postgres-17     linf-minio-latest
volume     linf-pg17-data       linf-minio-latest-data
admin      linf_admin           (PostgreSQL 슈퍼유저 / MinIO 루트 사용자)
port       5432                 9000 (S3 API) · 9001 (console)
```

### 8.3 DB 관리

| ID | 우선순위 | 요구사항 |
|---|---:|---|
| DB-001 | P0 | 프로젝트별 독립 DB와 로그인 계정을 생성한다. |
| DB-002 | P0 | 프로젝트 계정에는 해당 DB에 필요한 권한만 부여한다. |
| DB-003 | P0 | DB명과 사용자명의 PostgreSQL 제약을 입력 중 검증한다. |
| DB-004 | P0 | 동일 Target의 중복 DB명 및 계정명을 방지한다. |
| DB-005 | P0 | 생성 완료 후 실제 접속 테스트를 수행한다. |
| DB-006 | P0 | DB URL과 분리형 환경변수를 생성한다. |
| DB-007 | P0 | DB를 유지한 채 앱 등록만 해제한다. |
| DB-008 | P0 | DB 하나를 삭제해도 다른 DB와 엔진은 유지한다. |
| DB-009 | P1 | DB 비밀번호를 교체한다. |
| DB-010 | P1 | DB 복제 기능을 제공한다. |
| DB-011 | P2 | Prisma, Django, Spring 등 설정 형식을 제공한다. |

### 8.4 버킷 관리

| ID | 우선순위 | 요구사항 |
|---|---:|---|
| BKT-001 | P0 | 프로젝트별 독립 버킷과 전용 액세스 키를 생성한다. |
| BKT-002 | P0 | 액세스 키에는 자기 버킷만 접근하는 최소 권한 정책을 부여한다. |
| BKT-003 | P0 | 버킷명과 액세스 키의 S3 제약을 입력 중 검증한다. |
| BKT-004 | P0 | 동일 엔진의 중복 버킷명 및 액세스 키를 방지한다. |
| BKT-005 | P0 | 생성 완료 후 발급한 키로 실제 접근 테스트를 수행한다. |
| BKT-006 | P0 | S3 엔드포인트와 `.env` 블록을 생성한다. |
| BKT-007 | P0 | 버킷을 유지한 채 앱 등록만 해제한다. |
| BKT-008 | P0 | 버킷 하나를 삭제해도 다른 버킷과 엔진은 유지한다. |
| BKT-009 | P1 | 버킷의 액세스 키를 교체한다. |
| BKT-010 | P0 | 버킷 사용량(오브젝트 수, 총 크기)을 표시한다. |

- 최소 권한이 이 표의 핵심이다. 프로젝트 정책은 `arn:aws:s3:::<bucket>`과 `arn:aws:s3:::<bucket>/*`만 참조하고 그 밖의 리소스는 포함하지 않는다.
- 정책 이름은 `linf-<bucket>` 규칙을 따르며, MinIO 내장 `readwrite`/`readonly` 정책은 사용하지 않는다.
- 버킷 삭제는 오브젝트, 버킷, 전용 사용자, 정책을 함께 제거한다. 등록 해제는 메타데이터와 저장된 시크릿만 지운다.

### 8.5 SSH 터널

| ID | 우선순위 | 요구사항 |
|---|---:|---|
| TUN-001 | P0 | 원격 PostgreSQL에 로컬 포트 포워딩을 시작한다. |
| TUN-002 | P0 | 포트 충돌 시 대체 포트를 제안한다. |
| TUN-003 | P0 | 터널 상태, 로컬 포트 및 Target을 표시한다. |
| TUN-004 | P0 | TUI 종료와 터널 생명주기를 분리하고 유지 정책을 설정한다. |
| TUN-005 | P0 | 연결 중단을 감지하고 재연결 동작을 제공한다. |
| TUN-006 | P0 | 터널 준비 후 접속 URL을 사용 가능 상태로 표시한다. |
| TUN-007 | P0 | 앱 시작 시 기록된 터널의 실제 생존 여부를 조정한다. |
| TUN-008 | P1 | 선택 프로젝트의 터널을 앱 시작 시 연결한다. |
| TUN-009 | P1 | 프로젝트별 고정 로컬 포트를 예약한다. |
| TUN-010 | P0 | 원격 MinIO의 S3 API 포트(`9000`)로 가는 버킷 터널을 시작하고, DB 터널과 같은 목록·상태·재연결 동작을 제공한다. |

터널은 리소스 단위로 열린다. 대상이 DB면 원격 `5432`, 버킷이면 원격 `9000`으로 연결하며, 로컬 포트는 리소스가 예약한 고정 포트를 우선 사용하고 없으면 설정된 범위(기본 `15432`부터)에서 비어 있는 포트를 고른다. 원격 Target의 버킷은 활성 터널이 없으면 연결 정보를 내주지 않고 터널을 먼저 시작하라고 알린다.

### 8.6 백업 및 복원

| ID | 우선순위 | 요구사항 |
|---|---:|---|
| BAK-001 | P0 | PostgreSQL DB 단위 수동 백업을 지원한다. |
| BAK-002 | P0 | 로컬 DB 백업을 사용자 지정 폴더에 저장한다. |
| BAK-003 | P0 | 원격 DB 백업을 SSH 스트림으로 로컬에 저장한다. |
| BAK-004 | P0 | 파일, 리소스, 생성 시각과 결과를 기록한다. |
| BAK-005 | P0 | 백업을 새 DB 또는 기존 DB에 복원한다. |
| BAK-006 | P0 | 복원 전 영향 및 덮어쓰기 여부를 확인한다. |
| BAK-007 | P0 | 장시간 백업의 진행률과 취소를 화면에 표시한다. |
| BAK-008 | P1 | 자동 백업과 보관 정책을 제공한다. |
| BAK-009 | P1 | 백업 무결성 검증 결과를 표시한다. |
| BAK-010 | P0 | 버킷 단위 오브젝트 아카이브 백업을 로컬 단일 파일로 저장한다. 원격 버킷도 SSH 스트림으로 로컬에 받는다. |
| BAK-011 | P0 | 오브젝트 아카이브를 버킷에 복원한다. 대상 버킷이 비어 있지 않으면 덮어쓰기 확인 없이 진행하지 않고, 아카이브가 잘려 있으면 일부만 복원하지 않고 실패한다. |

백업 형식은 리소스 종류에 대응한다. DB는 `custom`(`pg_dump -Fc`, 확장자 `.dump`) 또는 `plain`(`.sql`)이고, 버킷은 `objects`(`.objects`)다. 버킷 아카이브 형식은 §19.11에서 정의한다. 모든 백업 파일은 `0600`으로 생성하고 SHA-256 체크섬을 기록에 남긴다.

### 8.7 기존 리소스와 마이그레이션

| ID | 우선순위 | 요구사항 |
|---|---:|---|
| MIG-001 | P0 | 기존 컨테이너를 읽기 전용으로 탐색한다. |
| MIG-002 | P0 | 승인 없이 기존 리소스의 관리권을 인수하지 않는다. |
| MIG-003 | P1 | 프로젝트별 컨테이너를 공유 엔진으로 이전하는 흐름을 제공한다. |
| MIG-004 | P1 | 이전 전 원본 백업과 대상 충돌 검사를 수행한다. |
| MIG-005 | P1 | 검증 전까지 원본 컨테이너와 볼륨을 삭제하지 않는다. |
| MIG-006 | P1 | 이전·신규 접속 URL을 비교해 보여준다. |

### 8.8 TUI 셸

| ID | 우선순위 | 요구사항 |
|---|---:|---|
| TUI-001 | P0 | 모든 P0 동작을 키보드만으로 완료한다. |
| TUI-002 | P0 | 80×24 터미널에서 정보 손실 없이 동작한다. |
| TUI-003 | P0 | 터미널 리사이즈에 즉시 재배치한다. |
| TUI-004 | P0 | 커맨드 팔레트로 모든 동작에 접근한다. |
| TUI-005 | P0 | 하단 힌트 바에 현재 포커스의 유효 키를 표시한다. |
| TUI-006 | P0 | 장기 작업 중 UI가 멈추지 않고 취소 방법을 제공한다. |
| TUI-007 | P0 | 비정상 종료 시에도 터미널 상태를 복원한다(raw mode, alt screen). |
| TUI-008 | P0 | `?` 도움말에서 전체 키맵을 확인한다. |
| TUI-009 | P1 | 설정 파일로 키맵과 색상을 재정의한다. |
| TUI-010 | P1 | 마우스 클릭과 스크롤을 보조 입력으로 지원한다. |
| TUI-011 | P2 | 화면 상태를 텍스트로 덤프해 이슈에 첨부한다. |

### 8.9 헤드리스 CLI

| ID | 우선순위 | 요구사항 |
|---|---:|---|
| CLI-001 | P0 | 인자 없이 실행하면 TUI를 연다. |
| CLI-002 | P0 | 모든 P0 동작에 대응하는 서브커맨드를 제공한다. |
| CLI-003 | P0 | `--json`으로 기계 판독 출력을 제공한다. |
| CLI-004 | P0 | 성공 0, 사용자 오류 2, 실행 실패 1로 종료 코드를 구분한다. |
| CLI-005 | P0 | 파괴적 명령은 `--yes` 없이 실행되지 않는다. |
| CLI-006 | P0 | 비대화형(TTY 없음) 환경에서 프롬프트 없이 동작하거나 명확히 실패한다. |
| CLI-007 | P0 | 접속 URL과 S3 엔드포인트를 stdout으로 출력해 파이프에 사용한다. |
| CLI-008 | P1 | `.env` 형식과 direnv 스니펫을 출력한다. |
| CLI-009 | P1 | 셸 자동완성 스크립트를 생성한다. |

주요 서브커맨드:

```bash
linf                                   # TUI 실행
linf doctor                            # 환경 진단

linf target add-local --name local
linf target add-ssh --name dev-vps --host vps.ts.net --user dev
linf target ssh-config                 # ~/.ssh/config 호스트 목록
linf target list
linf target test dev-vps
linf target verify vps.ts.net          # 등록 전 호스트 키 지문 확인
linf target forget dev-vps             # 등록만 해제

linf engine ensure local postgres 17
linf engine ensure local minio latest
linf engine list
linf engine start|stop|restart local minio latest
linf engine logs local postgres 17 --tail 200
linf engine rm local minio latest --volume --plan

linf db create --target local --project letsbid --name letsbid_dev
linf db list
linf db url letsbid_dev                # stdout으로 접속 URL
linf db env letsbid_dev >> .env
linf db copy-url letsbid_dev
linf db copy-env letsbid_dev
linf db test letsbid_dev
linf db drop letsbid_dev --yes
linf db forget letsbid_dev
linf db rotate-password letsbid_dev
linf db duplicate letsbid_dev letsbid_stage

linf bucket create --target local --project letsbid --name letsbid-assets
linf bucket list
linf bucket url letsbid-assets         # 자격 증명 포함 S3 연결 문자열
linf bucket endpoint letsbid-assets    # 엔드포인트 주소만
linf bucket env letsbid-assets >> .env
linf bucket copy-url letsbid-assets
linf bucket copy-env letsbid-assets
linf bucket test letsbid-assets
linf bucket drop letsbid-assets --yes
linf bucket forget letsbid-assets
linf bucket rotate-key letsbid-assets

linf tunnel start parantica_dev        # DB 또는 버킷 이름
linf tunnel stop parantica-media
linf tunnel restart parantica_dev
linf tunnel status --json

linf backup run parantica_dev --out ./backups --format custom
linf backup run parantica-media --out ./backups
linf backup list parantica_dev
linf backup restore ./backups/x.dump --into new_dev
linf backup verify <backup-id>

linf discover dev-vps                  # 관리 대상이 아닌 컨테이너 탐색
linf completions zsh
```

`--json`과 `--yes`는 전역 플래그이며 서브커맨드 뒤에도 붙일 수 있다. 어떤 서브커맨드도 비밀번호나 시크릿 키를 인자로 받지 않는다.

---

## 9. 대표 사용자 흐름

### 9.1 로컬 DB 생성 (TUI)

1. 셸에서 `linf`를 실행한다.
2. `n`으로 새 리소스 폼을 열고 종류를 `데이터베이스`로 둔다.
3. Target `local`, 엔진 `postgres 17`을 선택한다.
4. 프로젝트명을 입력하면 DB명·사용자명이 자동 제안된다.
5. 실행 계획에서 `컨테이너 생성(신규)`을 확인한다.
6. `Ctrl+S`로 실행하고 단계별 진행 로그를 본다.
7. 접속 테스트 성공 후 `Y`로 `.env` 블록을 복사한다.

### 9.2 로컬 DB 생성 (CLI)

```bash
linf db create --target local --project letsbid --name letsbid_dev --json
linf db env letsbid_dev >> .env
```

### 9.3 VPS DB 생성과 로컬 접속

1. `Targets` 화면에서 `a`로 원격 Target을 등록한다.
2. 호스트 키 지문을 확인하고 승인한다.
3. `Ctrl+T`로 SSH와 Docker 권한 테스트를 통과한다.
4. VPS에 PostgreSQL 엔진을 생성한다.
5. 프로젝트 DB와 계정을 생성한다.
6. `Resources`에서 `t`로 터널을 시작한다.
7. 앱이 로컬 포트를 선택해 SSH 포워딩을 시작하고 상태를 `active`로 바꾼다.
8. 터널을 통한 DB 연결을 테스트한다.
9. `127.0.0.1:<local-port>` 기반 URL을 IDE와 앱에서 사용한다.

### 9.4 로컬 버킷 생성

TUI에서는 §9.1과 같은 폼에서 종류만 `버킷`으로 바꾼다.

1. `n`으로 새 리소스 폼을 열고 종류를 `버킷`으로 바꾼다.
2. Target `local`, 엔진 `minio latest`를 선택한다.
3. 프로젝트명을 입력하면 버킷명이 자동 제안된다(`letsbid` → `letsbid-assets`).
4. 실행 계획에서 컨테이너·볼륨 생성 여부와 정책 이름을 확인한다.
5. `Ctrl+S`로 실행한다. 앱이 버킷, 최소 권한 정책, 전용 액세스 키를 만들고 그 키로 접근을 검증한다.
6. `Y`로 `.env` 블록을 복사한다.

CLI에서는 다음과 같다.

```bash
linf bucket create --target local --project letsbid --name letsbid-assets --json
linf bucket env letsbid-assets >> .env
```

### 9.5 VPS 버킷 생성과 터널 접속

1. 등록한 원격 Target에 MinIO 엔진을 생성한다.

   ```bash
   linf engine ensure dev-vps minio latest
   ```

2. 프로젝트 버킷과 전용 액세스 키를 만들고, 이 버킷의 터널이 항상 쓸 로컬 포트를 예약한다.

   ```bash
   linf bucket create --target dev-vps --project parantica \
     --name parantica-media --tunnel-port 19000
   ```

3. 터널을 시작한다. 로컬 `19000`이 VPS의 `127.0.0.1:9000`으로 연결된다.

   ```bash
   linf tunnel start parantica-media
   linf tunnel status
   ```

4. 접근을 테스트한다. 활성 터널이 없으면 앱은 연결 정보를 내주지 않고 터널을 먼저 시작하라고 알린다.

   ```bash
   linf bucket test parantica-media
   ```

5. 엔드포인트와 `.env` 블록을 프로젝트로 넘긴다. VPS의 `9000`과 `9001`은 공인 인터페이스에 열지 않는다.

   ```bash
   linf bucket endpoint parantica-media     # http://127.0.0.1:19000
   linf bucket env parantica-media >> .env
   ```

### 9.6 기존 컨테이너 통합(P1)

1. 관리 대상이 아닌 컨테이너를 읽기 전용으로 탐색한다.
2. 원본 리소스와 공유 엔진을 선택한다.
3. 원본을 백업한다.
4. 공유 엔진에 새 DB와 제한 계정, 또는 새 버킷과 전용 키를 생성한다.
5. 백업을 복원하고 접속을 검증한다.
6. 신규 URL과 엔드포인트를 제공하며 원본은 그대로 보존한다.
7. 사용자가 검증한 뒤 원본을 별도로 정리한다.

---

## 10. 오류와 복구 UX

모든 오류는 다음을 포함한다.

1. 무엇이 실패했는지
2. 가능한 원인
3. 사용자가 할 수 있는 다음 행동

TUI 표시:

```text
┌ 원격 Docker 접근 실패 ───────────────────────────┐
│ SSH 연결에는 성공했지만 dev 사용자가 Docker      │
│ 명령을 실행할 권한이 없습니다.                   │
│                                                  │
│ 시도한 명령                                      │
│   ssh dev-vps -- docker version                  │
│ 반환                                             │
│   permission denied on /var/run/docker.sock      │
│                                                  │
│ 다음 행동                                        │
│   VPS에서 해당 사용자에게 Docker 실행 권한을     │
│   부여한 뒤 다시 시도하세요.                     │
│                                                  │
│ r 다시 테스트   Y 진단 정보 복사   Esc 닫기      │
└──────────────────────────────────────────────────┘
```

CLI 표시: 같은 내용을 stderr에 단문 3줄로 출력하고 종료 코드로 구분한다. 진단에 실린 명령은 항상 마스킹된 형태다.

필수 오류 상태:

- Docker CLI 미설치 또는 Engine 중지
- SSH 연결 실패 또는 호스트 키 변경
- 원격 Docker 권한 부족
- 포트 충돌
- 컨테이너 healthcheck 실패
- DB 생성 또는 권한 설정 실패
- 버킷 생성, 정책 부여 또는 액세스 키 발급 실패
- 발급한 액세스 키의 접근 검증 실패
- SSH 터널 중단
- 원격 버킷에 활성 터널이 없어 엔드포인트를 확정할 수 없음
- 디스크 공간 부족
- 백업 또는 복원 실패, 오브젝트 아카이브 절단
- DB명 또는 버킷명 충돌
- Keyring 접근 불가(헤드리스 서버 등)
- 터미널 크기 부족 또는 TERM 미지원

부분 실패 시 수행된 단계와 롤백 여부를 활동 로그에 기록한다.

---

## 11. 보안 및 권한

### 11.1 자격 증명

- DB 비밀번호와 버킷 시크릿 키는 OS Keyring에 저장한다.
- Keyring 사용 불가 시 비밀 값을 저장하지 않는 제한 모드를 제공하고, 선택적으로 사용자 암호로 암호화한 파일 저장을 허용한다(§19.8).
- SSH 개인키 내용은 앱 DB로 복사하지 않는다. 경로만 보관한다.
- 로그, 화면, 진단 복사 결과에서 비밀번호, 시크릿 키와 전체 URL을 마스킹한다.
- 클립보드 비밀 값 자동 삭제 옵션을 제공한다.
- 관리자 계정(`linf_admin`)은 내부 운영에만 사용한다. 프로젝트에는 절대 노출하지 않는다.

### 11.2 터미널 환경 고유 위험

- 비밀 값을 셸 히스토리에 남기지 않는다. CLI는 비밀번호와 시크릿 키를 인자로 받지 않고 stdin 또는 자동 생성만 허용한다.
- **관리자 자격 증명은 argv에 실리지 않는다.** 컨테이너로 전달할 때는 값 없는 `--env NAME` 형태로 docker 클라이언트의 환경에서 읽게 하거나, 값을 stdin으로 넘겨 컨테이너 안 셸이 읽게 한다. 로컬 실행과 `ssh` 경유 실행 모두 같은 규칙을 따른다.
- 접속 URL과 S3 연결 문자열의 stdout 출력은 사용자가 명시적으로 요청한 경우에만 수행한다.
- 터미널 스크롤백에 비밀 값을 남기지 않는다. 표시는 일시적이며 alt screen 안에서만 이루어진다.
- OSC 52 클립보드 전송은 설정에서 끌 수 있으며, 신뢰하지 않는 중계 환경에 대한 경고를 제공한다.
- 상태 파일과 SQLite는 `0600`, 상태 디렉터리는 `0700` 권한으로 생성한다. 백업 파일도 `0600`이다.

### 11.3 원격 서버

- Docker API 공개를 요구하지 않는다.
- SSH 호스트 키 검증을 강제한다. 비활성화 옵션을 제공하지 않는다.
- DB 포트와 S3 API·콘솔 포트는 공인 인터페이스에 바인딩하지 않는다.
- SSH 또는 Tailscale을 기본 접근 경로로 사용한다.
- 전용 SSH 계정 사용을 권장한다.
- Docker 그룹 권한이 사실상 서버 관리자 권한임을 안내한다.

### 11.4 리소스 소유권

- 앱이 생성한 컨테이너와 볼륨에 관리 label을 붙인다.
- label 없는 리소스는 읽기 전용으로만 표시하고 목록에서 시각적으로 구분한다.
- 변경 경로는 실행 직전에 살아 있는 컨테이너의 label을 다시 읽어 확인한다. 확인에 실패하면 거부한다.
- 관리권 인수는 별도 확인과 사전 검증을 요구한다.
- 볼륨 삭제는 DB·버킷 삭제보다 강한 확인을 요구한다.

### 11.5 프로젝트 권한 격리

공유 엔진에서 데이터 경계를 지키는 장치는 엔진마다 하나씩 있고, 둘은 같은 원칙의 서로 다른 구현이다.

| 엔진 | 프로젝트 자격 증명 | 권한 범위 |
|---|---|---|
| PostgreSQL | DB 소유 로그인 롤 | 자기 DB에만 접근한다. 다른 프로젝트 DB에는 접속하지 못한다. |
| MinIO | 버킷 전용 액세스 키 | 정책이 `arn:aws:s3:::<bucket>`과 `arn:aws:s3:::<bucket>/*`만 허용한다. 버킷 목록 조회로도 다른 버킷이 드러나지 않는다. |

- 두 경우 모두 생성 직후 발급한 자격 증명으로 실제 연결·접근을 검증한다. 검증에 실패하면 만든 것을 되돌린다.
- 정책과 롤은 리소스와 함께 생성되고 함께 삭제된다. 리소스만 지우고 자격 증명을 남기는 경로는 없다.
- 자격 증명 교체(`db rotate-password`, `bucket rotate-key`)는 권한 범위를 바꾸지 않는다.

---

## 12. 비기능 요구사항

### 12.1 호환성

- macOS, Linux를 지원 대상으로 한다. Windows는 Phase 4로 미룬다(§19.10).
- `xterm-256color`, `screen`, `tmux`, `alacritty`, `ghostty` TERM 환경에서 동작한다.
- 256색과 truecolor를 감지해 팔레트를 조정하고, 흑백 터미널에서도 정보가 유지된다.
- Docker Desktop과 일반 Docker Engine CLI를 지원한다.
- OpenSSH 호환 서버를 지원한다.
- MVP 엔진은 PostgreSQL 17과 MinIO(`latest`)를 기준으로 한다.
- 엔진 클라이언트 도구는 컨테이너 이미지가 제공하는 것을 사용한다. 호스트에 `psql`이나 `mc`가 없어도 모든 기능이 동작해야 한다.

### 12.2 성능

- 콜드 스타트에서 첫 화면 렌더까지 200ms 이내를 목표로 한다.
- 키 입력 반응은 16ms 이내로 처리하고, 느린 작업은 비동기로 넘긴다.
- 유휴 상태에서 고빈도 `docker stats` 호출을 피한다. 폴링 주기는 화면 가시성에 따라 조정한다.
- 크기·연결 수·오브젝트 수 같은 통계는 첫 프레임에서 조회하지 않고 뒤이어 채운다.
- 유휴 CPU 사용률은 1% 미만을 유지한다.
- 렌더링은 변경된 영역만 갱신한다.
- 긴 목록과 로그는 가상 스크롤로 처리한다.

### 12.3 신뢰성

- 변경 작업에 실행 전 계획과 실행 후 검증이 있어야 한다.
- 앱이 중단되어도 컨테이너와 볼륨은 유지되어야 한다.
- 패닉이나 강제 종료 시에도 raw mode와 alternate screen을 반드시 복원한다.
- 작업을 활동 로그에 단계별로 기록한다.
- 재실행 시 실제 Docker 상태와 메타데이터를 조정한다.
- SSH 터널 비정상 종료를 감지해 UI에 반영한다.
- 동일 상태 디렉터리에 대해 여러 인스턴스가 동시에 쓰지 않도록 잠금을 사용한다. 두 번째 인스턴스는 읽기 위주로 동작하고 잠금 보유자를 알린다.

### 12.4 접근성 및 UX

- 마우스 없이 모든 작업을 수행한다.
- 포커스는 색상이 아닌 테두리와 커서 위치로도 식별된다.
- 상태는 색상뿐 아니라 기호와 텍스트로 표현한다(`●` running, `○` stopped, `!` error).
- `NO_COLOR` 환경변수를 존중한다.
- 유니코드 미지원 환경에서 ASCII 대체 문자를 사용한다.
- 리소스명, 종류, 포트, 컨테이너명은 정렬된 컬럼에 표시한다.
- 파괴적 동작은 별도 모달과 기본 취소 포커스로 분리한다.
- 로딩, 성공, 실패 상태를 즉시 피드백한다.
- 애니메이션은 스피너 수준으로 제한하고 `reduced motion` 설정 시 정적 표시로 바꾼다.
- 한글·CJK 폭 계산을 정확히 처리해 컬럼이 깨지지 않는다.

### 12.5 개인정보 및 텔레메트리

- 계정 가입 없이 로컬에서 동작한다.
- 서버 주소, 리소스명과 로그를 기본적으로 외부 전송하지 않는다.
- 향후 진단 데이터 전송은 명시적 opt-in으로 제공한다.

---

## 13. 데이터 모델 초안

상태 위치: `$XDG_STATE_HOME/local-infra/` (macOS는 `~/Library/Application Support/local-infra/`), 설정은 `$XDG_CONFIG_HOME/local-infra/config.toml`. 상태 디렉터리 안에 `state.db`(SQLite), `run/*.pid`, `instance.lock`, 기본 백업 폴더가 있다. `LINF_STATE_DIR`을 지정하면 모든 경로가 그 아래로 모인다. 암호화 파일 모드의 암호는 `LINF_PASSPHRASE`로 전달한다.

```text
Target
- id
- kind: local | ssh
- displayName
- host
- sshPort
- sshUsername
- authType: agent | key
- identityPath
- dockerCommand
- hostKeyFingerprint
- createdAt
- lastConnectedAt

EngineInstance
- id
- targetId
- engine: postgres | minio
- majorVersion
- image
- containerName
- volumeName
- bindAddress
- hostPort               서비스 주 포트 (postgres 5432 / minio 9000)
- consolePort            보조 포트, MinIO 콘솔 9001. PostgreSQL은 없음
- adminUser              linf_admin
- credentialRef
- managed
- createdAt

ManagedDatabase
- id
- engineInstanceId
- projectName
- databaseName
- username
- credentialRef
- preferredLocalTunnelPort
- createdAt
- lastConnectionTestAt
- lastBackupAt

ManagedBucket
- id
- engineInstanceId
- projectName
- bucketName
- accessKey
- credentialRef
- preferredLocalTunnelPort
- createdAt
- lastConnectionTestAt
- lastBackupAt

TunnelSession
- id
- resourceId             DB id 또는 버킷 id
- resourceKind: database | bucket
- localHost
- localPort
- remoteHost
- remotePort
- pid
- pidFilePath
- status
- startedAt
- stoppedAt

BackupRecord
- id
- resourceId             DB id 또는 버킷 id
- resourceKind: database | bucket
- storageLocation        항상 로컬 경로 (§19.6)
- fileName
- format: custom | plain | objects
- size
- checksum               SHA-256
- status
- createdAt

ActivityRecord
- id
- targetId
- resourceType
- resourceId
- action
- origin: tui | cli
- status
- redactedSummary
- steps
- startedAt
- completedAt
```

엔진의 실행 상태와 리소스 통계는 저장하지 않는다. 목록을 열 때마다 Docker와 엔진에서 읽어 메타데이터와 조정한다. 터널과 백업은 두 리소스 테이블을 공유하므로 참조 정합성은 외래 키가 아니라 삭제 유스케이스가 지킨다.

---

## 14. 디자인 방향

### 14.1 제품 성격

- 터미널 도구답게 정보 밀도가 높고 장식이 없다.
- 상태, 영향 범위, 다음 행동을 항상 화면에 둔다.
- 로컬과 원격 Target을 기호와 라벨로 즉시 구분한다.
- 데이터베이스와 버킷을 같은 표에서 다루되 종류를 한 컬럼으로 분명히 구분한다.
- 파괴적 동작은 화면상 위치와 확인 절차로 분리한다.

### 14.2 시각 및 인터랙션 원칙

- 터미널 기본 배경을 존중하고 배경을 칠하지 않는다. 다크·라이트 테마 모두에서 읽힌다.
- 색은 상태 강조에만 쓰고, 항상 기호·텍스트와 병행한다.
- 테두리는 얇은 단선 하나만 사용한다.
- 정렬은 고정 컬럼 기반이며 값의 폭 변화로 레이아웃이 흔들리지 않는다.
- 화면당 주요 동작은 하단 힌트 바 왼쪽 첫 항목으로 강조한다.
- 스피너와 진행률 외의 모션은 사용하지 않는다.
- 폼은 라벨을 항상 표시하고 오류를 필드 바로 아래에 둔다.

### 14.3 핵심 컴포넌트

- Status bar (Docker·터널·시각)
- Nav bar (번호 기반 화면 전환)
- Target list / Engine status block
- Resource table (종류 컬럼, 정렬·필터 가능)
- Detail pane (DB용·버킷용 두 형태)
- Connection URL field (마스킹·복사)
- S3 endpoint field (마스킹·복사)
- Tunnel status indicator
- Activity timeline
- Form modal
- Plan preview panel
- Destructive confirmation modal (이름 입력 요구)
- Command palette
- Help overlay
- Hint bar
- Toast / alert badge

---

## 15. MVP 출시 기준

### 터미널 셸

- [ ] 80×24 터미널에서 모든 P0 흐름을 완료한다.
- [ ] 마우스를 쓰지 않고 모든 P0 동작을 수행한다.
- [ ] 리사이즈, `Ctrl+Z` 후 복귀, 강제 종료 후에도 터미널이 정상 상태로 남는다.
- [ ] `NO_COLOR`와 흑백 터미널에서 상태를 식별할 수 있다.
- [ ] 커맨드 팔레트로 모든 P0 동작에 접근한다.
- [ ] 장기 작업 중에도 UI가 응답하고 취소가 가능하다.
- [ ] 하나의 `Resources` 화면에서 DB와 버킷을 함께 다룬다.

### 로컬 · 데이터베이스

- [ ] 로컬 Docker 연결 진단이 성공한다.
- [ ] PostgreSQL 엔진과 영구 볼륨을 생성한다.
- [ ] 서로 다른 프로젝트 DB를 같은 엔진에 만든다.
- [ ] 각 계정은 다른 프로젝트 DB에 접근하지 못한다.
- [ ] 앱과 컨테이너 재시작 후 데이터가 유지된다.
- [ ] DB 하나를 삭제해도 다른 DB와 엔진은 유지된다.
- [ ] 호스트에 `psql`이 없어도 위 항목이 모두 동작한다.

### 로컬 · 오브젝트 스토리지

- [ ] MinIO 엔진과 영구 볼륨을 생성하고, S3 API와 콘솔 포트가 루프백에만 열린다.
- [ ] 서로 다른 프로젝트 버킷을 같은 엔진에 만든다.
- [ ] 각 액세스 키는 자기 버킷만 접근하고 다른 버킷은 조회조차 되지 않는다.
- [ ] 앱과 컨테이너 재시작 후 오브젝트가 유지된다.
- [ ] 버킷 하나를 삭제해도 다른 버킷과 엔진은 유지된다.
- [ ] 생성 실패 시 버킷, 정책, 사용자가 고아로 남지 않는다.
- [ ] 호스트에 `mc`나 AWS CLI가 없어도 위 항목이 모두 동작한다.

### 원격 VPS

- [ ] SSH 호스트 키를 검증해 Target을 등록한다.
- [ ] 원격 Docker 상태를 조회하고 PostgreSQL·MinIO 엔진을 생성한다.
- [ ] DB 포트와 S3 API·콘솔 포트를 인터넷에 공개하지 않고 사용할 수 있다.
- [ ] SSH 터널로 로컬 애플리케이션이 원격 DB에 접속한다.
- [ ] SSH 터널로 로컬 S3 SDK가 원격 버킷을 읽고 쓴다.
- [ ] 활성 터널이 없는 원격 버킷은 엔드포인트를 내주지 않고 터널을 먼저 시작하라고 알린다.
- [ ] TUI를 종료해도 정책에 따라 터널이 유지되고, 재시작 시 상태가 일치한다.
- [ ] 터널 중단을 감지하고 재연결 방법을 제공한다.
- [ ] Tailscale 주소를 SSH 호스트로 사용할 수 있다.

### CLI

- [ ] `linf db create`와 `linf db url`로 TUI 없이 DB를 만들고 URL을 얻는다.
- [ ] `linf bucket create`와 `linf bucket endpoint`로 TUI 없이 버킷을 만들고 엔드포인트를 얻는다.
- [ ] `--json` 출력이 스키마에 맞고 파싱 가능하다.
- [ ] 파괴적 명령이 `--yes` 없이는 실행되지 않는다.
- [ ] 비밀번호와 시크릿 키가 셸 히스토리에 남지 않는다.

### 백업 및 안전

- [ ] 로컬 및 원격 DB를 로컬 파일로 백업한다.
- [ ] 백업을 새 DB에 복원한다.
- [ ] 로컬 및 원격 버킷을 오브젝트 아카이브 한 파일로 백업한다.
- [ ] 아카이브를 버킷에 복원하고, 비어 있지 않은 대상은 확인 없이 덮어쓰지 않는다.
- [ ] 잘린 아카이브는 일부만 복원하지 않고 명시적으로 실패한다.
- [ ] 비밀 값이 SQLite와 로그, 스크롤백, argv에 평문으로 남지 않는다.
- [ ] 관리 label 없는 Docker 리소스를 변경하지 않는다.
- [ ] DB, 버킷, 컨테이너, 볼륨 삭제의 영향 범위를 구분한다.

---

## 16. 성공 지표

- 신규 프로젝트가 새 컨테이너 대신 기존 엔진을 재사용하는 비율(DB·스토리지 각각)
- 리소스 생성부터 접속 URL·엔드포인트 확보까지의 완료 성공률과 소요 키 입력 수
- 원격 SSH 터널 연결 성공률(DB 터널·버킷 터널 각각)
- 앱 외부에서 `docker`, `psql` 또는 `mc` 명령을 직접 실행한 빈도
- 백업 후 복원 검증 성공률(DB 덤프·오브젝트 아카이브 각각)
- 앱이 생성하지 않은 Docker 리소스를 잘못 수정한 사고 건수
- 커맨드 팔레트 대비 키맵 사용 비율(키맵 학습 여부 판단)
- CLI 서브커맨드가 프로젝트 셋업 스크립트에 채택된 비율

절대 사용자 수보다 파일럿 사용자가 실제 프로젝트 여러 개를 공유 엔진으로 통합하고 일상적으로 재사용하는지를 우선 평가한다.

---

## 17. 개발 단계

스토리지는 별도 단계가 아니다. 각 단계에서 데이터베이스와 함께 같은 깊이로 올라간다.

### Phase 0: 코어와 CLI

- `core` 유스케이스 계층과 SQLite·Keyring 저장소
- 로컬 Docker 진단(`linf doctor`)
- PostgreSQL·MinIO 엔진 준비(`linf engine ensure`)
- `linf db create` / `db url` / `db drop` 비대화형 경로
- `linf bucket create` / `bucket endpoint` / `bucket drop` 비대화형 경로
- 실행 계획(`--plan`)과 활동 로그 기록

### Phase 1: TUI 셸과 로컬 엔진

- ratatui 앱 골격, 화면 전환, 힌트 바, 도움말
- 터미널 복원 및 패닉 훅
- PostgreSQL·MinIO 엔진 관리 화면
- DB와 버킷을 함께 보여주는 `Resources` 화면
- 새 리소스 폼과 실행 계획 미리보기(두 종류 모두)
- 안전한 삭제 모달과 활동 로그 화면
- 커맨드 팔레트

### Phase 2: VPS 및 SSH 터널

- SSH Target과 호스트 키 검증 모달
- 원격 Docker 명령 어댑터
- 원격 PostgreSQL·MinIO 엔진
- 분리 프로세스 기반 SSH 로컬 포트 포워딩(DB `5432`, 버킷 `9000`)
- 터널 생명주기, 상태 조정, 재연결 UX
- 활성 터널 기준 엔드포인트 결정
- Tailscale 주소 연결 검증

### Phase 3: 백업 및 이전

- 로컬·원격 `pg_dump` 백업과 진행률
- 로컬·원격 버킷 오브젝트 아카이브 백업과 진행률
- 두 형식의 복원과 무결성 검증
- 기존 컨테이너 읽기 전용 탐색
- 공유 엔진 이전 흐름
- 원본 보존과 전환 체크리스트

### Phase 4: 확장

- MySQL/MariaDB 어댑터
- Redis 관리
- PostGIS 이미지 템플릿
- 다른 S3 호환 스토리지 엔진
- 키맵·테마 사용자 설정
- 셸 자동완성과 direnv 통합
- 자동 백업 및 보관 정책
- Windows 지원

---

## 18. 리스크와 대응

| 리스크 | 영향 | 대응 |
|---|---|---|
| 공유 엔진 장애 | 여러 프로젝트 중단 | 버전별 분리, 상태 표시, 리소스별 백업 |
| Docker 그룹 권한 | VPS 보안 위험 | 전용 계정/서버 권장, 권한 경고 |
| SSH 터널 포트 충돌 | 앱 연결 실패 | 사전 검사, 대체 포트, 고정 포트 예약 |
| TUI 종료 시 터널 소실 | 개발 중 연결 끊김 | 분리 프로세스, PID 파일, 시작 시 상태 조정 |
| 메타데이터 불일치 | 잘못된 상태 표시 | 실제 Docker를 기준으로 시작 시 조정 |
| 명령 부분 실행 | 고아 DB/계정/버킷/정책 | 단계별 기록, 실패 시 롤백, idempotent 재시도, 사후 검증 |
| 버킷 정책 과다 권한 | 프로젝트 간 데이터 노출 | 버킷 단위 정책 고정, 정책 문서 단위 테스트, 발급 키로 접근 검증 |
| 오브젝트 아카이브 손상 | 복원 실패 또는 부분 복원 | 매니페스트 검증, 오브젝트별 정확한 바이트 수 소비, 절단 시 명시적 실패 |
| MinIO 평문 HTTP | 자격 증명 노출 | 루프백 전용 바인딩, SSH 터널 경유 접근, 공인 인터페이스 노출 경고 |
| 터미널 렌더링 파손 | 사용 불가 | 최소 크기 처리, 폭 계산, TERM 폴백, 패닉 훅 |
| 비밀 값 스크롤백·히스토리·argv 잔존 | 자격 증명 노출 | alt screen 표시, 인자 금지, `--env NAME` 패스스루·stdin·자동 생성 |
| Keyring 사용 불가(헤드리스) | 비밀 저장 실패 | 비저장 제한 모드, 암호화 파일 옵션 |
| 자동 이전 중 데이터 손실 | 높은 사용자 피해 | 백업 우선, 원본 보존, 명시적 확인 |
| VPS 포트 오노출 | 외부 공격 | 기본 `127.0.0.1` 바인딩, SSH 터널 전용, 위험 경고 |
| TUI/CLI 기능 격차 | 자동화 신뢰 하락 | 팔레트 명령과 서브커맨드 1:1 유지 |

---

## 19. 결정 사항

| # | 질문 | 결정 | 근거 |
|---|---|---|---|
| 1 | 구현 언어: Rust + ratatui 대 Go + Bubble Tea | Rust 2021 + `ratatui` + `crossterm` | §20의 파일럿 결정과 이어지고, 툴체인이 이미 준비되어 있다. 터널·프로세스 생명주기를 소유권 모델로 다룬다. |
| 2 | SSH를 시스템 `ssh`로 실행할지 라이브러리로 내장할지 | 시스템 `ssh` 바이너리 | §6.1 표. `~/.ssh/config`, ssh-agent, `known_hosts`, Tailscale MagicDNS를 그대로 물려받는다. |
| 3 | 원격 Docker를 `docker --context`로 통일할지 SSH 명령 계층으로 만들지 | 명시적 SSH 명령 계층(`ssh host -- docker …`) | §6.3이 Target과 리소스 작업을 분리한다. 사용자의 docker context 상태를 바꾸지 않는다. |
| 4 | 터널 백그라운드 프로세스를 자체 데몬으로 둘지 `ssh -f -N`에 위임할지 | `ssh -N -L`을 `setsid(2)`로 분리 실행하고 PID 파일은 앱이 소유한다. 별도 데몬은 두지 않는다. | `-f`는 fork 후 PID를 숨긴다. TUN-004/007은 앱이 아는 PID를 실제 상태와 조정해야 한다. |
| 5 | 관리자 자격 증명의 수명과 Keyring 저장 정책 | 엔진 생성 시 생성해 Keyring `engine:<engine-id>`에 저장한다. 출력하지 않고 argv에도 넣지 않는다. | §11.1, §11.2. PostgreSQL 관리 SQL은 컨테이너 로컬 소켓 신뢰 연결로, MinIO 관리 호출은 `MC_HOST_linf` 환경변수로 처리한다. |
| 6 | 원격 백업을 항상 로컬에 둘지 VPS 저장도 지원할지 | 항상 로컬 파일로 스트리밍한다. VPS 측 저장은 보류한다. | BAK-003, BAK-010. 원격 호스트에 임시 사본을 만들지 않는다. |
| 7 | 앱 종료 후 SSH 터널 유지의 기본값 | 유지한다(`tunnel.keep_alive_on_exit = true`). | §6.5. TUI 종료가 개발 루프를 끊어서는 안 된다. |
| 8 | 헤드리스 서버에서 Keyring 대체 저장 방식 | 기본은 비저장 제한 모드, 선택적으로 암호화 파일 모드(AES-256-GCM, 암호에서 유도한 키, `0600`). | §11.1. 평문을 조용히 기록하지 않는다. 암호는 `LINF_PASSPHRASE`로 받는다. |
| 9 | 기존 컨테이너 읽기 전용 탐색의 MVP 포함 범위 | MVP에 포함한다(MIG-001/002는 P0). 목록 조회만 하고 관리권 인수는 없다. | §8.7. 관리 label 없는 리소스에 대한 모든 변경은 거부한다. |
| 10 | Windows 지원을 MVP에 포함할지 | Phase 4로 미룬다. macOS와 Linux만 지원한다. | `setsid`, `0600`, Keyring 경로가 유닉스 전용이다. |
| 11 | 버킷 백업 아카이브 형식 | 자체 단일 파일 형식. `LINFBKT1` 헤더 한 줄, JSON 매니페스트 한 줄, 그다음 오브젝트 원시 바이트를 순서대로 이어 붙인다. 확장자는 `.objects`. | MinIO 이미지에 `tar`가 없다. `mc ls --json`·`mc cat`·`mc pipe` 스트림만으로 만들고 되돌릴 수 있어야 하며, 매니페스트의 크기 정보가 절단 감지의 근거가 된다. |
| 12 | MinIO에 TLS를 적용할지 | 적용하지 않는다. 루프백 또는 SSH 터널 뒤에서 평문 HTTP로 제공한다. | §6.4. 터널이 이미 제공하는 보호를 인증서 발급·갱신 비용으로 다시 살 이유가 없다. 공인 인터페이스에는 애초에 바인딩하지 않는다. |

---

## 20. 최종 제품 결정

**결정: Pilot**

터미널 애플리케이션으로 공유 개발 인프라 파일럿을 우선 구현한다. `core` 계층과 비대화형 CLI를 먼저 세우고, 그 위에 ratatui TUI를 얹는다. 엔진은 PostgreSQL과 MinIO 두 종류로 시작하며, 두 엔진은 같은 모델을 공유한다. Target당 엔진 하나, 엔진 안에 프로젝트별 리소스와 전용 자격 증명이다. 로컬 관리 기능을 완성한 뒤 같은 명령 계층에 SSH Target과 터널 기능을 추가한다. 원격 개발은 Docker API나 서비스 공인 포트를 열지 않고 SSH 또는 Tailscale 위에서 동작한다.

> 터미널을 벗어나지 않고 로컬과 VPS 어디서든 공유 개발 인프라 엔진을 만들고, 프로젝트별 DB와 버킷을 분리해 안전하게 연결·백업·정리하는 TUI 도구.
