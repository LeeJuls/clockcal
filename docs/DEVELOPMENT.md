# clockcal 개발 안내

Windows 11 작업표시줄 시계를 클릭하면 구글 캘린더 월 달력과 일정 팝업을 띄우는 Tauri 2 앱입니다. 상태는 1차 완성이며 설치본 실사용 확인까지 끝났습니다. 이 문서는 소스를 받아 고칠 사람을 위한 안내입니다.

## 1. 폴더 구성

| 경로 | 내용 |
|---|---|
| `app/` | Tauri 앱 본체 |
| `app/src/` | 프런트(바닐라 JS, 빌드 도구 없음): `index.html` `main.js` `logic.js` `strings.js` `style.css` |
| `app/src-tauri/src/` | Rust 코어 |
| `app/dev/` | 브라우저 미리보기용 목(mock) IPC와 JS 단위 테스트 |
| `poc/` | P1 시계 클릭 가로채기 PoC (앱 본체와 무관, 참고용) |
| `signing/` | 자체 서명 공개 인증서(.cer) |
| `docs/` | 계획·결정 로그·QA 리포트·서명/개인 계정 가이드 |

저장소에 없는 것: `node_modules`, `target`, `logs`, `gen`, OAuth 클라이언트 JSON(비밀). 아래 4장에서 만들어야 합니다.

## 2. 기능 요약

- 시계 클릭(저수준 마우스 훅 + UI Automation 으로 시계 영역 판정) 시 기본 달력 대신 팝업 표시. 트레이 아이콘과 전역 단축키는 폴백.
- 월 그리드, 일정 점, 날짜별 아젠다, 캘린더 색, Meet 참여 링크, 참석 응답(예/아니요/미정), 종일·다일 일정.
- 일정 시작 전 알림(토스트), 설정 시트(알림 on/off·분 전·자동 실행·시계 클릭 가로채기).
- 구글 계정 1개(내부 프로필 ID `personal`). 알림은 그 계정의 일정으로 울립니다.
- 오프라인이면 캐시 표시와 "오래된 데이터" 배너.

## 3. 아키텍처

### Rust 모듈 (`app/src-tauri/src/`)

| 파일 | 역할 |
|---|---|
| `lib.rs` | 앱 조립: 트레이, 단축키, IPC 커맨드, 팝업 창, 시작 시 설정 로드 |
| `clockhook.rs` / `clock_logic.rs` | WH_MOUSE_LL 훅, 시계 클릭 판정(순수 로직은 `clock_logic`) |
| `uia.rs` / `monitor.rs` | 작업표시줄 시계 사각형 조회, 모니터 정보 |
| `popup_logic.rs` | 팝업 위치 계산(순수 로직) |
| `oauth.rs` | Google OAuth + PKCE(루프백), 클라이언트 로드, 토큰 갱신 |
| `token_store.rs` | Windows 자격 증명 관리자 `clockcal/<profile>` |
| `calendar.rs` | Calendar API 호출, 캐시, 일정 병합, RSVP |
| `notifier.rs` / `notify_runner.rs` | 알림 판정(순수) / 20초 주기 스레드, `notified.json` |
| `settings.rs` / `autostart.rs` | `settings.json` / HKCU Run 등록 |
| `openurl.rs` / `applog.rs` | 외부 URL 열기 / 파일 로그(비밀 미기록) |
| `build.rs` | tauri 빌드 훅만 있음 |

IPC 커맨드: `auth_status` `auth_login` `auth_logout` `calendar_month` `calendar_list` `calendar_respond` `open_url` `ui_effects` `get_settings` `set_settings`.

### 데이터·설정 위치

- `%APPDATA%\clockcal\settings.json`: `notify_enabled`, `notify_lead_min`(1~120), `autostart`, `clock_hook`
- `%APPDATA%\clockcal\profiles\<profile>.json`: OAuth 클라이언트 JSON(구글 콘솔에서 받은 "데스크톱 앱" 형식)
- 토큰: 자격 증명 관리자 `clockcal/<profile>` (파일로 저장하지 않음)
- 프런트 상태: localStorage `clockcal.profile`
- 문자열은 전부 `app/src/strings.js` 한 곳(다국어 확장 대비)

### OAuth 클라이언트 규칙

- 클라이언트는 각 사용자가 본인 구글 클라우드 프로젝트에서 만든 "데스크톱 앱" JSON을 `%APPDATA%\clockcal\profiles\personal.json` 에 두는 방식입니다. exe 에 내장된 클라이언트는 없습니다(C-24).
- 외부(External)·프로덕션 게시·미검증 상태로 본인만 쓰므로 100명 상한은 문제 없고, 로그인 때 "확인되지 않은 앱" 경고만 한 번 넘기면 됩니다. 테스트 상태면 7일 뒤 로그인이 풀립니다.

## 4. 개발 환경 구성

필요한 것: Rust(stable, MSVC 툴체인), Node 18+, WebView2(Win11 기본), NSIS(설치 파일 빌드 때 tauri가 내려받음).

```powershell
cd clockcal\app
npm install
cd src-tauri
cargo test          # 130개 통과, 1개 ignored 가 정상
cargo clippy --all-targets
```

OAuth 클라이언트가 없으면 로그인이 안 됩니다. 구글 클라우드 콘솔에서 Calendar API를 켜고 범위 `calendar.events`, `calendar.readonly`로 데스크톱 앱 OAuth 클라이언트를 만들어 JSON을 `%APPDATA%\clockcal\profiles\personal.json`에 저장하세요. 절차는 `docs/google_setup.md`.

### 실행·확인

| 목적 | 명령 |
|---|---|
| 앱 개발 실행 | `cd app; npx tauri dev` (시계 클릭 훅 켜려면 `--hook` 인자) |
| UI만 브라우저 미리보기 | `node dev/serve.mjs 5179` 후 `http://localhost:5179/?s=noclient` 등(`dev/mock.js` 시나리오). 개인 화면은 localStorage `clockcal.profile=personal` |
| JS 테스트 | `node dev/logic.test.cjs` |
| 설치 파일 | `npx tauri build --bundles nsis` → `src-tauri\target\release\bundle\nsis\clockcal_0.1.0_x64-setup.exe` |

설치 파일에는 OAuth 클라이언트가 들어가지 않습니다. 서명 절차와 배포 방법은 `docs/signing.md`.

## 5. 튜닝 포인트

| 하고 싶은 것 | 위치 |
|---|---|
| 문구·언어 | `app/src/strings.js` |
| 색·간격·테마 | `app/src/style.css` (아크릴 틴트 값은 `lib.rs`의 `tint`) |
| 월 그리드·아젠다 렌더 | `app/src/main.js`, 날짜 계산은 `logic.js` |
| 팝업 크기(360x640)·위치 | `tauri.conf.json`, `popup_logic.rs` |
| 시계 클릭 판정 영역 | `clock_logic.rs`(알림 종 버튼 영역은 의도적으로 제외, C-11) |
| 알림 규칙(시작 N분 전~시작+60초, 일정당 1회) | `notifier.rs`, 주기·백오프는 `notify_runner.rs` |
| 오프라인 재시도 간격 | `notify_runner.rs`의 `STALE_BACKOFF_S`(40초) |
| 일정 표시 규칙(제목 숨김 일정은 "바쁨", 같은 일정 id는 첫 캘린더 색으로 병합) | `calendar.rs` |
| 새 설정 항목 | `settings.rs` + `get_settings/set_settings`(`lib.rs`) + 설정 시트(`main.js`) |
| 트레이 메뉴 | `lib.rs`의 `setup_tray` |

원칙: 판정 로직은 `*_logic.rs`·`notifier.rs`처럼 OS 호출이 없는 순수 함수로 두고 단위 테스트를 붙입니다. 이 구조를 유지하면 튜닝 후 회귀를 `cargo test`로 잡을 수 있습니다.

## 6. 설계 결정 요약

전체 목록은 `docs/dev/decisions.md`(C-01~C-24).

- 스택은 Tauri 2. 호출은 시계 클릭이 기본이고 트레이·단축키는 폴백.
- 알림 종 버튼(OmniButtonRight)은 가로채지 않음(C-11).
- 계정은 개인 1개, 회사 계정·내장 클라이언트는 제거(C-24).
- 일정별 colorId는 채택하지 않고 캘린더 색을 그대로 사용.
- 자체 서명 인증서로 설치 파일 서명(C-22).
- 연결 정보가 없으면 팝업에 7단계 안내를 표시합니다.

## 7. 알려진 한계·미확인

- 자체 서명이라 인증서(`signing/clockcal-selfsigned.cer`)를 CurrentUser의 Root와 TrustedPublisher에 등록한 PC에서만 "알 수 없는 게시자"가 사라집니다. SmartScreen 평판 경고는 해결되지 않습니다. 서명 인증서 개인키는 원 작성자 PC에만 있어(내보내기 불가) 다른 PC에서 서명 빌드를 하려면 새 인증서를 만들어야 합니다.
- Windows 업데이트로 작업표시줄 UI 구조가 바뀌면 시계 영역 판정(UIA)이 깨질 수 있습니다. 이 PC 실측 기준은 Win11 26300입니다.
- 실기 미검증: 클릭 가로채기 끔 상태의 재시작 유지와 재활성, 신규 PC 실로그인, 오프라인 알림, 재부팅 후 자동 실행.
- 알림 토스트는 비패키지 AUMID 제약이 있어 설치본에서만 확인했습니다.
- 목(mock) 미리보기는 UI 확인용이며 실제 WebView2·훅 동작은 검증하지 못합니다.

## 8. 주의

- OAuth 클라이언트 JSON, 토큰은 저장소에 올리지 않습니다(`.gitignore` 반영됨).
- 로그(`logs/`)에는 비밀을 기록하지 않지만 사용자 프로필 경로는 포함될 수 있어 공유 전에 확인하세요.
- 개발 이력(계획·결정·QA 기록)은 `docs/dev/`.
