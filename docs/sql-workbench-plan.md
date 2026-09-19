---
meta:
  contentType: Conceptual
  title: PostgreSQL 작업공간을 linf에 내재화하는 방법
  category: Product planning
  navLabel: Native SQL Workbench
status: Implemented v1
product: local-infra
command: linf
date: 2026-09-18
---

# PostgreSQL 작업공간을 `linf`에 내재화하는 방법

이 문서는 `linf` 안에 PostgreSQL 전용 SQL 작업공간을 구현하는 제품 범위와 기술 구조를 정의한다. 작업공간은 관리 중인 로컬·SSH 데이터베이스와 외부 데이터베이스에 연결하며, SQL 편집·실행·탐색·내보내기를 단일 Rust 바이너리에서 처리한다.

## 문서 계획

이 절은 문서가 답해야 할 질문과 결정 대상을 고정한다.

- **목표**: PostgreSQL 작업공간의 제품 범위, 안전 경계, 구조, 구현 순서, 검증 조건을 정의한다
- **대상 독자**: `local-infra` 구현자와 제품 결정을 검토하는 유지보수자
- **콘텐츠 계획**: 제품 결정, 사용자 흐름, 요구사항, 구조, 보안, 단계별 출시 조건, 검증 계획 순서로 설명한다
- **구현 결정**: 외부 비밀번호 미저장, query text opt-in, client certificate 보류, internal editor와 safe-default formatter

## 제품 결정

`linf`는 Harlequin을 실행하거나 포함하지 않는다. PostgreSQL 전용 SQL 작업공간을 Rust로 구현하고 기존 TUI, vault, SSH 터널, CLI 계약에 연결한다.

이 결정은 다음 제품 가치를 우선한다:

- 별도 Python 런타임이나 외부 SQL 클라이언트가 필요 없는 단일 바이너리
- `linf`가 만든 데이터베이스를 추가 입력 없이 여는 흐름
- 기존 SSH 터널과 암호화된 자격 증명 저장소 재사용
- 외부·프로덕션 연결에 대한 일관된 안전 정책
- TUI와 헤드리스 CLI가 공유하는 하나의 실행 코어

전체 Harlequin 호환성은 목표가 아니다. `linf`는 PostgreSQL 데이터베이스를 만들고 사용하는 흐름만 완결한다.

## 현재 제품과의 관계

현재 PRD는 범용 SQL 편집기와 결과 그리드를 MVP 비목표로 둔다. 이 계획은 PostgreSQL 관리 기능이 안정화된 다음 단계로 해당 범위를 확장한다.

기존 원칙은 그대로 유지한다:

1. TUI는 비즈니스 로직을 소유하지 않는다
2. 모든 TUI 동작에는 대응하는 CLI 기능이 있다
3. 비밀번호는 argv, 로그, 활동 기록에 남기지 않는다
4. 로컬과 SSH Target은 같은 사용자 흐름을 사용한다
5. 장기 작업은 UI event loop를 막지 않는다
6. 단일 `linf` 바이너리 배포를 유지한다

SQL 작업공간은 인프라 관리용 `psql` 실행 경로와 분리한다. `pg.rs`는 컨테이너 관리 작업을 계속 담당하고, 새 SQL 모듈은 PostgreSQL wire protocol로 사용자 세션을 유지한다.

## 목표와 비목표

이 절은 첫 정식 릴리스가 책임질 행동을 제한한다.

### 목표

- 관리 중인 PostgreSQL 데이터베이스를 Resources 화면에서 연다
- SSH Target 데이터베이스에 기존 터널로 연결한다
- 외부 PostgreSQL 연결 프로필을 추가·수정·검사·삭제한다
- 여러 SQL buffer를 작성하고 복구한다
- 선택 영역, 현재 statement, 전체 buffer를 실행한다
- 결과를 키보드로 탐색하고 CSV 또는 JSON으로 내보낸다
- schema, table, view, column을 catalog에서 탐색한다
- catalog 정보를 SQL 자동완성에 사용한다
- 쿼리를 취소하고 timeout을 적용한다
- read-only 연결과 쓰기 연결을 명확히 구분한다
- TUI 기능에 대응하는 헤드리스 SQL 실행 명령을 제공한다

### 비목표

- PostgreSQL 이외 데이터베이스 adapter
- 웹 UI 또는 데스크톱 GUI
- 테이블 셀 직접 편집
- ER 다이어그램
- PostgreSQL 서버 관리와 고가용성 운영
- SQL 문자열 검사만으로 보안 경계를 제공하는 기능
- 공유 쿼리, 팀 협업, 중앙 계정 시스템
- Harlequin 플러그인·테마·키맵 호환
- 쿼리 결과의 영구 저장
- 첫 릴리스의 Parquet 또는 Arrow export

## 핵심 사용자 흐름

이 절은 연결 종류별 진입점과 완료 상태를 정의한다.

### 관리 중인 로컬 데이터베이스 열기

1. Resources 화면에서 PostgreSQL 데이터베이스를 선택한다
2. `o`를 눌러 SQL 작업공간을 연다
3. `linf`가 엔진 상태와 자격 증명을 확인한다
4. 작업공간이 기존 프로젝트 계정으로 연결한다
5. catalog가 준비되는 동안 editor는 즉시 입력을 받는다

사용자는 host, port, database, username, password를 다시 입력하지 않는다.

### 관리 중인 SSH 데이터베이스 열기

1. Resources 화면에서 원격 데이터베이스를 선택한다
2. `o`를 눌러 SQL 작업공간을 연다
3. 활성 터널이 없으면 `linf`가 기존 터널 규칙으로 시작한다
4. 작업공간은 터널의 loopback endpoint에 연결한다
5. 터널이 종료되면 작업공간이 연결 손실과 복구 방법을 표시한다

`linf`는 원격 PostgreSQL 포트를 공인 네트워크에 노출하지 않는다.

### 외부 데이터베이스 추가하기

1. SQL Connections 화면에서 `n`을 누른다
2. 이름, host, port, database, username, TLS 설정을 입력한다
3. password를 대화형 필드에 입력한다
4. password 저장 여부와 기본 접근 모드를 선택한다
5. 연결 검사를 통과하면 프로필을 저장한다

프로필 이름은 TUI, CLI, 활동 메시지에서 연결을 식별한다. 화면과 로그는 password가 없는 endpoint만 표시한다.

### 프로덕션 연결 열기

외부 연결은 기본적으로 read-only로 시작한다. 작업공간 header는 `EXTERNAL`과 `READ ONLY`를 텍스트로 표시한다.

쓰기 세션은 별도 동작으로 연다:

1. **Open writable session**을 실행한다
2. 대상 endpoint와 영향을 확인한다
3. 프로필 이름을 다시 입력한다
4. 해당 작업공간이 종료될 때까지만 쓰기 상태를 유지한다

저장된 프로필의 기본값을 암묵적으로 쓰기 모드로 승격하지 않는다.

## 화면과 상호작용

이 절은 SQL 작업공간의 정보 구조와 focus 이동을 고정한다.

```text
┌ Connections / Catalog ─┬──────── SQL Editor ────────┐
│ local                   │ select id, email           │
│  └ acme_dev             │ from users                │
│     └ public            │ order by id desc;          │
│        ├ users          │                            │
│        └ sessions       ├──────── Results ───────────┤
│ prod · READ ONLY        │ id │ email         │ …     │
│  └ app                  │ 42 │ a@example.com │ …     │
└─────────────────────────┴────────────────────────────┘
```

작업공간은 세 pane을 가진다:

- **Connections / Catalog**: 연결, schema, relation, column 탐색
- **SQL Editor**: buffer tab, SQL 입력, 검색, 자동완성
- **Results**: row grid, command status, 오류, export 상태

`Tab`과 `Shift+Tab`은 pane focus를 이동한다. `F10`은 현재 pane을 전체 화면으로 전환한다. `Esc`는 자동완성, 검색, modal을 닫고 마지막 단계에서만 작업공간 종료를 요청한다.

### 기본 키 계약

키는 기존 TUI keymap override 구조를 사용한다.

| 동작 | 기본 키 | 조건 |
|---|---|---|
| SQL 작업공간 열기 | `o` | PostgreSQL resource 선택 |
| 현재 statement 실행 | `Ctrl+Enter` | editor focus |
| 선택 영역 실행 | `Ctrl+Enter` | 선택 영역 존재 |
| 전체 buffer 실행 | `F5` | editor focus |
| 실행 취소 | `Ctrl+C` | query 실행 중 |
| 새 buffer | `Ctrl+N` | 작업공간 |
| buffer 닫기 | `Ctrl+W` | 저장되지 않은 내용 확인 |
| buffer 전환 | `Alt+Left`, `Alt+Right` | 작업공간 |
| SQL format | `F4` | editor focus |
| catalog focus | `F6` | 작업공간 |
| query history | `F8` | 작업공간 |
| pane 전체 화면 | `F10` | 작업공간 |
| 결과 export | `Ctrl+E` | 결과 존재 |

터미널이 `Ctrl+Enter`를 구분하지 못하면 `F5`가 전체 실행 경로를 보장한다. 선택 영역 실행에는 keymap override를 설정할 수 있다.

## 기능 요구사항

이 절은 UI 모양과 무관하게 코어가 보장할 행동을 정의한다.

### SQL editor

- UTF-8 다중 줄 입력을 지원한다
- paste를 문자 단위 key event가 아닌 paste event로 처리한다
- undo와 redo를 buffer별로 유지한다
- 검색, 줄 이동, 선택, 들여쓰기를 지원한다
- SQL keyword, string, comment, number, identifier를 구분해 표시한다
- 작은따옴표, 큰따옴표, dollar-quoted string, line comment, block comment를 인식한다
- statement 경계는 문자열과 주석 안의 세미콜론을 무시한다
- catalog가 없어도 keyword 자동완성을 제공한다
- catalog가 준비되면 schema, relation, column 후보를 추가한다
- 열린 buffer를 crash-safe 임시 파일에 저장하고 정상 종료 시 정리한다

### Query execution

- 선택 영역이 있으면 선택 영역만 실행한다
- 선택 영역이 없으면 cursor가 속한 statement를 실행한다
- `F5`는 buffer 전체를 순서대로 실행한다
- statement별 status와 elapsed time을 반환한다
- 서버가 반환한 SQLSTATE, severity, message, detail, hint, position을 보존한다
- 실행 중인 query마다 cancel token을 유지한다
- connection profile의 timeout을 각 실행에 적용한다
- connection이 끊기면 명시적으로 reconnect한 뒤 새 session을 시작한다
- reconnect 후 transaction, temporary table, session setting이 사라졌음을 알린다

### Result viewer

- column header, row, NULL, command status를 구분한다
- NULL을 빈 문자열과 다른 표기로 표시한다
- column width를 viewport와 sample에 맞게 제한한다
- 수직·수평 스크롤과 긴 cell 상세 보기를 지원한다
- 여러 result set을 tab으로 구분한다
- preview row limit과 cell byte limit을 표시한다
- 메모리 제한에 도달하면 결과를 잘린 상태로 완료한다
- 결과를 CSV 또는 JSON으로 streaming export한다
- 결과를 clipboard에 복사할 때 현재 selection만 복사한다

### Catalog

- database, schema, table, partitioned table, view, materialized view, sequence를 구분한다
- column name, PostgreSQL type, nullable, default를 표시한다
- primary key와 foreign key 관계를 표시한다
- system schema는 기본적으로 접고 필요할 때 펼친다
- catalog refresh가 editor와 query 실행을 막지 않는다
- relation 이름 삽입 시 필요한 경우 identifier를 quote한다
- catalog 오류는 query session을 종료하지 않는다

### Query history

- 실행 시각, profile ID, 성공 여부, elapsed time, row count를 기록한다
- query text 저장은 profile별 정책을 따른다
- 외부 연결은 query text 기록을 기본적으로 끈다
- 결과 row와 `linf`가 관리하는 password는 기록하지 않는다
- `PASSWORD`, connection URI 등 알려진 credential 형태를 기록 전에 redact한다
- 기록된 query를 새 buffer로 열 수 있다
- 보존 기간과 최대 항목 수를 config에서 제한한다

문자열 redaction은 모든 SQL literal의 민감도를 판단할 수 없다. 프로덕션 profile에서 history를 끄는 기본값이 주된 보호 장치다.

## 연결 모델

이 절은 관리 리소스와 외부 연결을 하나의 실행 인터페이스로 결합한다.

```rust
pub enum SqlConnectionSource {
    ManagedDatabase { database_id: String },
    ExternalProfile { profile_id: String },
}

pub struct SqlEndpoint {
    pub label: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub tls: SqlTlsConfig,
    pub access: AccessMode,
}
```

`SqlEndpoint`는 password를 포함하지 않는다. 실행 직전에 `credential_ref`로 secret을 읽고 connector에 직접 전달한다.

### 관리 데이터베이스 해석

관리 데이터베이스는 기존 `DatabaseView`에서 endpoint를 해석한다:

- Local Target: engine의 loopback bind address와 host port
- SSH Target: active tunnel의 local host와 local port
- username: 프로젝트 전용 role
- password: `ManagedDatabase.credential_ref`
- access mode: read-write

SSH Target에 active tunnel이 없으면 연결 해석이 실패한다. TUI는 터널 시작 계획을 표시하고 성공 후 작업공간을 연다.

### 외부 프로필 해석

외부 프로필은 Docker Target과 관계없는 직접 PostgreSQL 연결이다. `Target`, `EngineInstance`, `ManagedDatabase` 테이블에 저장하지 않는다.

필수 필드:

- 고유한 profile 이름
- host와 port
- database와 username
- TLS mode
- 기본 access mode

선택 필드:

- password credential reference
- root CA path
- client certificate와 private key path
- connect timeout
- query timeout
- application name

## 영속 데이터 모델

이 절은 새 metadata와 기존 secret store의 경계를 정의한다.

```sql
CREATE TABLE sql_connection_profiles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    host TEXT NOT NULL,
    port INTEGER NOT NULL,
    database_name TEXT NOT NULL,
    username TEXT NOT NULL,
    tls_mode TEXT NOT NULL,
    access_mode TEXT NOT NULL,
    credential_ref TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

인증서 경로, timeout, history 정책은 별도 column으로 추가한다. private key 내용과 password는 metadata DB에 저장하지 않는다.

Query history는 profile 삭제와 독립적으로 보존하거나 삭제할 수 있어야 한다. 외래 키는 `ON DELETE SET NULL`을 사용하고 당시의 redacted profile label을 함께 저장한다.

## 모듈 구조

이 절은 기존 모듈 책임을 유지하면서 SQL 기능을 추가한다.

```text
src/
├── core/
│   └── sql/
│       ├── profile.rs
│       ├── connection.rs
│       ├── runner.rs
│       ├── statement.rs
│       ├── catalog.rs
│       ├── history.rs
│       └── export.rs
├── tui/
│   └── sql/
│       ├── workspace.rs
│       ├── editor.rs
│       ├── catalog.rs
│       ├── results.rs
│       └── render.rs
└── cli/
    └── mod.rs
```

기존 `tui/mod.rs`는 작업공간 진입과 종료만 조정한다. Editor, result grid, catalog 상태를 기존 `App`에 직접 추가하지 않는다.

### Core 책임

`core::sql`은 다음 기능을 소유한다:

- profile validation과 저장
- endpoint 해석
- TLS connector 구성
- connection과 session 수명주기
- query 실행, cancellation, timeout
- catalog 조회
- result streaming과 export
- query history 정책

### TUI 책임

`tui::sql`은 다음 기능만 소유한다:

- focus와 key dispatch
- editor buffer와 viewport
- catalog tree 펼침 상태
- result viewport와 selection
- modal과 toast
- core event를 화면 상태로 변환

### CLI 책임

CLI는 core 기능의 비대화형 표현을 제공한다. SQL 결과는 stdout으로, 진단과 진행 상태는 stderr로 출력한다.

## 실행 파이프라인

이 절은 query가 editor에서 PostgreSQL과 result viewer까지 이동하는 순서를 정의한다.

1. TUI가 실행 범위와 buffer revision을 고정한다
2. statement splitter가 실행 단위를 계산한다
3. workspace가 core runner에 request를 전달한다
4. runner가 connection과 access mode를 확인한다
5. PostgreSQL driver가 query stream을 시작한다
6. runner가 schema event, row batch, command status를 bounded channel에 보낸다
7. TUI가 event를 소비하며 viewport를 갱신한다
8. runner가 history policy에 따라 실행 metadata를 기록한다
9. 완료·오류·취소 상태가 같은 result tab에 남는다

TUI event loop는 database future를 직접 await하지 않는다. 기존 job channel 패턴을 확장하거나 SQL 전용 actor를 사용한다.

## 세션과 transaction 규칙

이 절은 숨은 transaction으로 인한 데이터 변경과 오해를 막는다.

### Read-write session

- 기본값은 PostgreSQL autocommit
- 사용자가 `BEGIN`을 실행하면 transaction 상태를 header에 표시
- 연결 종료 전에 열린 transaction이 있으면 rollback 여부를 확인
- reconnect는 열린 transaction을 복구하지 않음

### Read-only session

- 각 실행을 PostgreSQL read-only transaction에서 처리
- transaction-control statement는 read-only 작업공간에서 거부
- 쓰기 실패는 서버 오류를 그대로 표시
- read-only 상태를 header와 실행 결과에 표시

앱의 read-only 설정은 사고 방지 장치다. 실제 권한 경계에는 SELECT만 허용한 PostgreSQL role을 사용해야 한다.

## 프로덕션 안전 정책

이 절은 외부 연결의 기본 동작과 명시적 승격 절차를 정의한다.

- 외부 profile의 기본 access mode는 read-only
- TLS 기본값은 인증서와 hostname을 검증하는 모드
- password는 CLI 인자나 connection URI로 받지 않음
- redacted endpoint만 로그와 activity에 기록
- profile별 query timeout과 preview row limit 적용
- query history text는 외부 profile에서 기본 비활성
- 쓰기 session은 profile 이름 재입력 후 한 번만 생성
- 화면 전체에 `EXTERNAL`, `READ ONLY`, `WRITABLE` 상태를 텍스트로 표시
- read-write 외부 session의 header와 border는 local session과 다른 의미를 유지
- color를 보지 못해도 연결 종류와 쓰기 가능 여부를 구분 가능

TLS를 끄거나 인증서 검증을 약화하는 설정은 경고를 표시한다. 설정을 저장하기 전과 연결할 때 모두 동일한 경고를 사용한다.

## 자원 제한과 성능 기준

이 절은 큰 결과가 terminal과 프로세스 메모리를 고갈시키지 않도록 상한을 둔다.

초기 기본값:

| 항목 | 기본값 | 동작 |
|---|---:|---|
| Preview rows | 5,000 | 초과 시 truncated 표시 |
| Cell display | 16 KB | 상세 보기에서도 상한 표시 |
| Query timeout | 30s | local profile에서 해제 가능 |
| Catalog refresh | 30s timeout | 실패해도 session 유지 |
| In-memory result | 64 MB | 초과 시 row 수와 무관하게 중단 |
| History entries | 1,000 | 오래된 항목부터 삭제 |
| Recovered buffers | 20 | 오래된 임시 buffer부터 정리 |

Preview 제한은 서버의 실행 비용을 자동으로 제한하지 않는다. 사용자는 `LIMIT`, timeout, read-only role을 함께 사용해야 한다.

Result viewer는 전체 결과를 한 번에 `Vec<Vec<String>>`으로 만들지 않는다. Runner는 row batch를 bounded channel로 보내고 UI는 허용된 메모리만 유지한다.

## 오류와 복구

이 절은 실패 원인과 다음 행동을 같은 위치에서 보여준다.

오류는 다음 범주를 구분한다:

- profile validation
- secret unavailable
- tunnel unavailable
- DNS 또는 TCP connection
- TLS certificate
- PostgreSQL authentication
- server SQL error
- timeout
- user cancellation
- connection loss
- export file

모든 진단은 실패 내용, 확인된 원인, 다음 행동을 포함한다. SQLSTATE가 있으면 복사할 수 있는 상세 영역에 표시한다.

Buffer 복구 파일은 정상 종료 시 삭제하고 crash 후 다음 실행에서 제안한다. 복구 파일과 history DB는 `0600` 권한을 적용한다.

## CLI 표면

이 절은 TUI와 같은 core를 호출하는 명령 계약을 정의한다.

```text
linf sql open <database-or-profile>
linf sql exec <database-or-profile> --command <sql>
linf sql exec <database-or-profile> --file <path>
linf sql catalog <database-or-profile>
linf sql connection add <name>
linf sql connection list
linf sql connection test <name>
linf sql connection forget <name>
```

`sql exec`는 다음 출력을 지원한다:

- `table`
- `csv`
- `json`
- `jsonl`

명령 문자열은 argv에 포함될 수 있으므로 history와 process list에 노출될 수 있다. 민감한 SQL에는 `--file -`로 stdin을 사용하도록 문서화한다.

Password는 옵션으로 받지 않는다. 대화형 terminal에서는 prompt를 사용하고, 자동화에서는 전용 환경변수나 stdin secret channel을 정의한다.

## 의존성 방향

이 절은 새 crate를 선택할 때 지켜야 할 기준을 정한다.

### 권장 역할

- PostgreSQL protocol과 cancel token: `tokio-postgres`
- TLS: Rustls 기반 PostgreSQL connector
- 인증서 root: 운영체제 root store adapter
- SQL syntax tree: `tree-sitter-sql`
- SQL formatting: PostgreSQL 구문을 보존하는 formatter
- CSV export: `csv`

### 선택 기준

- Rust 1.88에서 빌드 가능
- Linux x86_64/aarch64와 macOS x86_64/aarch64 지원
- 시스템 `libpq` 또는 OpenSSL 설치를 요구하지 않음
- query cancellation과 streaming 지원
- password를 debug output에 포함하지 않음
- release archive의 크기 증가를 측정 가능
- 유지보수 상태와 라이선스를 검토 가능

Editor widget은 기존 Ratatui layout과 keymap에 통합 가능한지 먼저 검증한다. 요구사항을 만족하지 못하면 editor buffer와 viewport를 프로젝트 내부 모듈로 구현한다.

## 구현 단계와 승인 조건

각 단계는 다음 단계가 시작되기 전에 독립적인 동작 증거를 남긴다.

### 단계 1: Connection core와 외부 profile

구현 범위:

- metadata migration
- profile CRUD
- vault 연동
- TLS connection
- 관리 DB endpoint 해석
- `linf sql connection` 명령
- `linf sql exec`의 단일 statement 실행

승인 조건:

- 로컬 managed DB에 driver로 연결해 `select 1` 실행
- active tunnel을 가진 managed SSH DB에 연결
- 외부 TLS profile 추가·검사·삭제
- password가 argv, JSON, 오류, activity에 나타나지 않음
- timeout과 cancellation이 실제 server query를 종료

### 단계 2: Editor와 result viewer

구현 범위:

- SQL workspace 진입과 종료
- multi-buffer editor
- 현재·선택·전체 실행
- streaming result grid
- 오류 위치 표시
- crash-safe buffer 복구

승인 조건:

- Resources에서 선택한 DB가 한 동작으로 열림
- 입력과 scrolling이 query 실행 중에도 반응
- 5,000 row preview가 메모리 상한 안에서 완료
- cancel 후 같은 session에서 다음 query 실행
- TUI 종료 후 terminal mode 완전 복구

### 단계 3: Catalog, autocomplete, export

구현 범위:

- catalog tree
- schema·relation·column 자동완성
- SQL format
- CSV·JSON export
- query history

승인 조건:

- catalog refresh 실패가 editor와 query session을 종료하지 않음
- quoted identifier가 올바르게 삽입됨
- export가 UI preview row limit과 독립적으로 동작
- 외부 profile은 query text history가 기본 비활성

### 단계 4: 프로덕션 안전성과 출시 준비

구현 범위:

- read-only transaction 정책
- writable session 확인
- TLS 경고
- 장시간 session 복구
- 문서와 Agent Skill 갱신

승인 조건:

- read-only session에서 DDL과 DML이 실패
- writable external session은 profile 이름 확인 없이 열리지 않음
- 연결 손실 후 transaction 상실을 명시적으로 표시
- 키보드만으로 모든 SQL 작업 완료
- Linux와 macOS release archive에서 동일한 연결 흐름 검증

## 검증 계획

이 절은 기능별로 필요한 실제 동작 증거를 지정한다.

### 자동 검증

- statement splitter의 quote, comment, dollar quote 경계
- profile validation과 secret redaction
- result memory·row·cell 제한
- keymap 충돌과 override
- metadata migration과 rollback
- read-only 실행 정책
- SQLSTATE와 error position 보존

### 실제 PostgreSQL 검증

- 로컬 managed PostgreSQL 17
- SSH tunnel을 통한 PostgreSQL 17
- TLS hostname 검증을 사용하는 외부 PostgreSQL
- 큰 result streaming과 cancellation
- connection loss와 reconnect
- 열린 transaction 상태에서 작업공간 종료
- CSV와 JSON export의 NULL·Unicode·newline 처리

### TUI 검증

- 80×24 최소 terminal
- 넓은 terminal의 세 pane layout
- bracketed paste
- 한글 입력과 Unicode width
- mouse 없이 buffer, catalog, result 이동
- panic, cancellation, normal exit 후 terminal 복구

## 위험과 완화책

이 절은 구현 중 범위가 커지거나 안전 계약이 약해지는 지점을 기록한다.

| 위험 | 결과 | 완화책 |
|---|---|---|
| 기존 `App` 상태에 SQL 상태 추가 | TUI 변경이 서로 결합됨 | 독립 `SqlWorkspace` 상태 머신 사용 |
| 결과 전체를 메모리에 저장 | OOM 또는 입력 지연 | streaming, bounded channel, byte limit 적용 |
| SQL parser를 보안 필터로 사용 | 우회 가능한 read-only | PostgreSQL transaction과 제한 role 사용 |
| 외부 profile을 Target으로 저장 | Docker 관리 모델 오염 | 별도 `sql_connection_profiles` 사용 |
| query history에 민감한 literal 저장 | 로컬 secret 노출 | 외부 profile 기본 off, redaction, `0600` 적용 |
| TLS 설정을 생략 | 외부 연결 도청 위험 | hostname 검증을 기본값으로 설정 |
| reconnect를 자동 성공으로 표시 | transaction 지속 오해 | 새 session과 상태 손실을 명시 |
| 범용 adapter 요구 확장 | 출시 범위와 유지보수 비용 증가 | PostgreSQL 전용 계약 유지 |

## 문서와 기존 계약 변경

이 계획을 구현할 때 다음 문서를 함께 갱신한다:

- `local-infra-prd.md`: SQL 작업공간을 post-MVP 목표로 추가하고 기존 비목표를 좁힘
- `README.md`: SQL workspace quickstart와 외부 profile 보안 설명 추가
- Agent Skill command reference: `linf sql` 명령과 secret 규칙 추가
- CLI palette parity test: 새 SQL 명령과 TUI action을 연결
- release notes: metadata migration, 새 의존성, archive 크기 변화 기록

기존 `psql` 기반 관리 기능은 유지한다. SQL workspace가 실패해도 database 생성, 백업, 복원, 삭제가 영향을 받지 않아야 한다.

## 구현 결정

첫 릴리스는 다음 안전한 결정을 적용한다:

1. 외부 profile password 저장은 기본으로 끈다. prompt, stdin, 환경변수로 받은 값은 사용 후 저장하지 않는다.
2. read-only profile의 statement는 별도 문자열 차단 없이 PostgreSQL `READ ONLY` transaction에서 실행한다. `EXPLAIN ANALYZE`의 허용 여부도 server가 결정한다.
3. query history는 결과나 password를 저장하지 않는다. query text는 profile별 opt-in이고 알려진 credential 형태를 redact한 뒤 `0600` SQLite에 저장한다.
4. client certificate 인증은 첫 릴리스에서 거부한다. TLS `verify-full`과 root CA 추가만 지원한다.
5. managed DB와 외부 profile의 기본 query timeout은 30초다.
6. formatter는 `sqlformat`의 PostgreSQL dialect를 사용한다. PL/pgSQL과 psql meta-command에 대한 별도 변환은 하지 않는다.
7. editor는 내부 UTF-8 buffer, selection, undo/redo, crash recovery 구현을 사용한다.

## v0.6.0 release note

- SQLite startup migration이 `sql_connection_profiles`, `sql_query_history`와 history index를 추가한다.
- binary에 `tokio-postgres`, rustls/native roots, `sqlformat`, `csv`, `futures-util`이 추가된다. OpenSSL이나 외부 `psql` runtime은 추가하지 않는다.
- 외부 profile의 vault reference는 기존 SecretStore에만 저장하고 profile row, CLI JSON, history에는 password를 저장하지 않는다.
- 기존 `psql` 기반 관리·백업 경로는 유지하며 SQL subsystem 실패와 독립적이다.
- Linux x86_64 `cargo build --release --locked` 기준 stripped binary는 15,469,816 bytes다. 이전 release baseline이 없어 증분 크기는 다음 tagged release 비교에서 기록한다.

## 완료 정의

SQL 작업공간은 다음 조건을 모두 만족할 때 완료된다:

- 관리 로컬·SSH DB와 외부 PostgreSQL을 같은 작업공간에서 열 수 있음
- 별도 `psql`, Python, Harlequin 설치 없이 동작함
- query 실행, cancellation, result 탐색, catalog, export가 키보드로 완료됨
- `linf`가 관리하는 password와 client key가 argv, 로그, activity, query history에 노출되지 않음
- 외부 연결이 read-only와 검증된 TLS를 기본값으로 사용함
- TUI와 CLI가 같은 profile, runner, catalog, export 코어를 사용함
- 작업공간 실패가 기존 인프라 관리 명령을 손상시키지 않음
- 실제 PostgreSQL과 terminal에서 검증된 증거가 release 전에 남음
