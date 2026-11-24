## QA 리포트 — P7 전체 (clockcal)

model : sonnet

범위: 정적 리뷰 + 명령 실행 (git diff 6611bf0..HEAD, 설치본 로그·설정 파일 읽기 전용). 실제 작업표시줄 클릭, 실행 중 앱 종료·시작, Run 키·%APPDATA% 쓰기, Google API 호출은 하지 않음.

### 1. 자동 테스트 결과 (측정)

| 항목 | 결과 |
|---|---|
| `cargo test` (src-tauri) | 129 passed / 0 failed / 1 ignored (2.12s). doc/bin 테스트 0건 |
| `cargo clippy --all-targets` | 경고·오류 0건 (Finished) |
| `node dev\logic.test.cjs` | 27 passed (TZ=system) |

### 2. 결함 (사용자 영향 순)

**Medium**

- M-1 알림: 오프라인(stale)·재로그인 필요 상태에서 알림 누락. `notify_runner.rs` 60~85행, `calendar.rs` 884~889행.
  - 재현 (코드 경로): stale 캐시를 쓰면 `retry_at = now + 300s`로 설정됨. 이후 `now < retry_at`이면 due 판정 자체를 건너뜀 (`continue`, 약 69행). 판정 창은 `lead*60 + 60s`, 점검 주기는 오프라인 시 300s.
  - 기대: 캐시가 있으면 오프라인에서도 20초 주기로 알림. 실제: 설정 시트가 제공하는 1·3·5분 전은 창이 120/240/360s라 300s 점검 주기에 놓칠 수 있음 (1분 전 기준 약 60% 확률로 누락). 10분 이상은 안전.
  - `NeedLogin`(리프레시 토큰 만료·철회)은 캐시가 있어도 `Err`로 반환되므로 (`calendar.rs` 884행) 알림이 전부 중단되고, 5분마다 재시도 로그만 남음. 사용자에게 표시되는 신호 없음.
  - 제안: backoff는 네트워크 재시도에만 적용하고 due 판정은 매 tick 캐시로 수행.
  - 미검증: 실제 오프라인 재현은 하지 않음 (코드 근거만).

- M-2 재설치 시 자동 실행 기본값이 복구되지 않음. `lib.rs` 169~172행(`first_run_autostart`), `get_settings`(147~151행).
  - 재현: 설치본 사용(settings.json에 `autostart:true`) → 제거 (앱 데이터 삭제 체크 안 함) → 재설치. 제거 시 NSIS가 Run 값만 지우고 settings.json은 남음 (`installer.nsi` 863행, 880행은 체크 시에만 삭제).
  - 기대(C-20): 설치본 첫 실행에서 자동 실행 등록. 실제: settings.json이 존재하므로 `p.exists()`로 조기 반환 → Run 값 없음. UI는 레지스트리를 읽어 "꺼짐"으로 표시하고, 아무 항목이나 바꾸면 `autostart:false`로 저장됨.
  - 제안: "settings.json 없음" 대신 "Run 값 없음 + autostart 설정이 true"일 때 복구하거나, 설치 시 첫 실행 플래그를 별도로 둘 것.

**Low**

- L-1 트레이 "시계 클릭 가로채기" 토글이 settings.json에 반영되지 않음. `lib.rs` 340~350행(`toggle_hook`).
  - 트레이에서 끈 뒤 설정 시트를 열면 체크박스는 저장값(켬)을 표시해 실제 상태(끔)와 불일치. 재시작하면 다시 켜짐 (안전 장치로는 의도일 수 있으나 문서화되지 않음).

- L-2 `set_settings` 동시 호출 경합. `lib.rs` 159~176행, `settings.rs` 59행.
  - UI는 체크박스 `change`마다 즉시 저장하고(`main.js` 573행), 저장 임시 파일명이 `tmp<pid>` 고정임. 빠르게 두 번 누르면 같은 임시 파일을 두 태스크가 쓰다 rename 실패 가능("설정을 저장하지 못했습니다" 오표시). 또 spawn_blocking 순서가 보장되지 않아 마지막 클릭과 저장·훅 상태가 다를 수 있음. 단일 사용자 빈도는 낮음 (런타임 미재현).

- L-3 `set_settings`가 훅 활성화 실패를 보고하지 않음. `lib.rs` 165~169행. `clockhook::enable()`의 bool 반환을 무시하므로 설치 실패 시에도 UI에 "저장됨"이 표시됨. 또 `autostart::set_enabled` 실패 시 (161행) 같은 요청의 다른 설정(훅·알림)은 저장되지 않음. 부분 적용 순서(레지스트리 변경 후 save 실패 시 롤백 없음)도 있음.

- L-4 자동 실행 Run 값이 `current_exe()` 기준으로 쓰임. `autostart.rs` 74~78행. 개발 빌드에서 설정 시트의 자동 실행을 켜면 `target\debug\clockcal.exe`로 Run 값을 덮어씀 (설치본 경로 소실). `is_enabled()`도 값 존재만 확인하고 경로가 현재 exe와 같은지는 검사하지 않음.

- L-5 플러그인 순서가 주석과 불일치. `lib.rs` 523~524행. "Must be the first plugin" 주석 바로 아래에 `tauri_plugin_notification::init()`이 single-instance보다 먼저 등록됨. 현재 증상은 없으나 두 번째 실행에서 알림 플러그인이 먼저 초기화됨. 순서를 되돌리는 것이 안전.

**Info**

- I-1 로그 `[hook] enabled (start) timeout=0s`는 "무제한"을 0초로 표기해 오해 소지 (`clockhook.rs` 322행).
- I-2 `decisions.md` C-20 행에 `\t`가 탭 문자로 깨져 `target\` 경로 표기가 손상됨 (docs 오탈자).
- I-3 알림은 `work` 프로필 고정 (`lib.rs` WORK_PROFILE, C-12와 일치). 개인 프로필 일정 알림 없음.
- I-4 `%APPDATA%\clockcal\profiles\work.json`에 OAuth 클라이언트 설정(`installed` 키, 405자)이 평문으로 있음. 설치형 앱 표준이며 사용자 토큰은 아님. 토큰 문자열 패턴(access/refresh)은 이 파일 외 profiles·cache에서 검색되지 않음.
- I-5 `capabilities/default.json`에 notification 권한 없음. Rust 측 `show()`만 쓰므로 동작에는 문제 없음. 설치본에서 토스트가 실제로 뜨는지는 미확인.

### 3. 요청 항목별 결과

**3-1. clock_hook 설정 / UNLIMITED 경로 (정적)**
- ✅ `settings.rs` Default는 `clock_hook:true`, `#[serde(default)]`로 구버전 settings.json(필드 없음)도 true. 테스트 `partial_and_unknown_fields`(알림 true 확인)가 있으나 clock_hook 직접 단언은 없음. 구조상 true가 되는 것은 확인했으나 전용 단위 테스트는 부재 (보강 권장).
- ✅ `UNLIMITED`: `init`에서 `Duration::ZERO` → `enable`에서 `deadline=None`. 디스패처 WM_TIMER의 만료 검사는 `deadline`이 None이면 건너뜀. 워치독 블록은 deadline과 무관하게 매 틱 실행되므로 유지됨 (`clockhook.rs` 903~940행).
- ✅ 데드락: `CTRL`은 `enable`/`disable`/`init`/디스패처 타이머에서만 잠그고, 훅 콜백에는 없음. `disable`이 락을 쥔 채 훅 스레드를 join하지만 훅 스레드는 CTRL을 요구하지 않아 상호 대기 없음.
- ✅ 트레이 동기화: `set_settings`는 `run_on_main_thread`로 `sync_hook_menu` 호출, 타임아웃 만료 경로도 동일.
- ⏸️ spawn_blocking 경합(enable/disable 동시)은 런타임 미재현. enable은 락을 쥔 채 최대 5초 대기하므로 그 동안 타이머 틱이 지연될 수 있음 (경미).
- 실제 설치본 로그 (15:11): `[hook] installed WH_MOUSE_LL ... [hook] enabled (start) timeout=0s`. 시작 시 훅 상시화 동작 증거. rects=2 (주 모니터와 보조 작업표시줄 둘 다 탐지).

**3-2. first_run_autostart**
- ✅ settings.json 없음 + 설치 경로: 등록 후 저장 (로그 `settings: first run -> autostart enabled`, 이후 settings.json `autostart:true` 확인).
- ✅ 손상된 settings.json: 파일이 존재하므로 건너뜀 (등록 안 함, `parse`는 기본값으로 대체).
- ✅ 개발 경로(`\target\`) 제외 로직 확인 (소문자화 후 비교).
- ✅ 실패 처리: 등록 실패 시 로그만 남기고 settings 미저장이라 다음 실행에서 재시도. 등록 성공 후 저장만 실패해도 다음 실행에서 재등록되어 무해.
- ❌ 재설치 시나리오는 M-2 참조.

**3-3. 알림 수정사항**
- ✅ 거절 복사본 억제 / 같은 id 중복 1회 / 재일정 재알림 / 장기 슬립 후 재생 없음 / 60초 유예 경계: 단위 테스트(`notifier.rs`)로 확인, cargo test 통과.
- ✅ 캐시 적중 로그 억제: `calendar.rs` 905~913행, 실제 로그에서 20초마다의 get_month 행이 없고 Fetched만 기록됨.
- ❌ stale backoff는 M-1 참조.
- ⏸️ DST·타임존 변경: `start_secs`가 rfc3339 → UTC 타임스탬프를 쓰므로 DST 영향이 구조적으로는 없음. 실측은 하지 않음. 월 경계(다음 달 조회)는 코드 확인만.

**3-4. 설정 라운드트립 / UI**
- ✅ `index.html` 62행 `#set-clockhook`, `main.js` 549행 로드(`v.clock_hook !== false`), 555행 저장 payload에 clock_hook 포함, 573행 change 바인딩, `strings.js` `setClockHook` 존재.
- ✅ 설정 파일 손상(`parse` 실패) → 기본값, 리드 타임 1~120 클램프 테스트 통과. 현재 설치본 settings.json은 4개 필드 모두 정상.
- ✅ notified.json 손상: `load_fired`가 파싱 실패 시 빈 맵으로 대체(`unwrap_or_default`). 이 경우 지난 24시간 이내 이미 발송한 알림이 유예 창(60초) 안일 때만 중복될 수 있어 영향은 매우 작음. 쓰기는 임시 파일 + rename.
- ✅ 단일 인스턴스: single-instance 플러그인이 2차 실행을 팝업 토글로 전달 (L-5의 순서 주의).
- ⏸️ 2차 실행 실측은 사용자 앱을 건드리지 않기 위해 생략.

**3-5. 설치 / 제거**
- ✅ `tauri.conf.json`: NSIS, `installMode: currentUser` (관리자 권한 불필요), identifier `com.clockcal.app`, 언어 한국어·영어. 빌드 산출물 `clockcal_0.1.0_x64-setup.exe`(약 2.75MB, 15:06) 존재.
- ✅ 제거 시 Run 값 삭제: 생성된 `installer.nsi` 863행에서 `DeleteRegValue HKCU ...\Run "${PRODUCTNAME}"` (업데이트 모드가 아닐 때만). PRODUCTNAME=`clockcal`이 `autostart.rs`의 VALUE_NAME `clockcal`과 일치하므로 제거됨. 업데이트 시에는 값이 보존됨. 설치/제거 시 실행 중 앱 종료(`CheckIfAppIsRunning`, 683·800행) 포함.
- ⏸️ 실제 제거 후 훅·프로세스 잔존 여부는 사용자 설치본을 건드리지 않기 위해 미실행.

**3-6. 보안**
- ✅ CSP: `default-src 'self'; style-src 'self' 'unsafe-inline'; script-src 'self'`. 외부 접속 불가. `innerHTML`/`eval` 사용 없음 (grep 0건). `style-src 'unsafe-inline'`은 Low 수준 완화 요소.
- ✅ 캐퍼빌리티: `core:default`만 부여, 창 `popup`에 한정 (fs/network 권한 없음).
- ✅ `open_url` 허용 목록: https 한정, calendar.google.com·meet.google.com 전체, www.google.com은 `/calendar` 하위만. 사용자정보·포트·공백·백슬래시·대문자 호스트·2048자 초과 거부. Rust 측에서 재검증(프런트 신뢰 안 함). 관련 단위 테스트 통과.
- ✅ 토큰: Credential Manager 저장 (`token_store.rs`), `Debug`는 마스킹. 디스크의 `profiles`·`cache` 에서 access/refresh 토큰 패턴 검색 결과 없음. (`work.json`이 `refresh_token|access_token` 검색에 걸린 것은 OAuth 클라이언트 설정 파일 내용 때문으로 보임. 키 이름만 확인 → `installed` 한 키, 값 출력 안 함.)
- ✅ 로그 3개 파일(총 약 8KB) 패턴 검색: `ya29.`, `1//0`, `refresh_token`, `access_token`, `Bearer`, `client_secret`, `GOCSPX`, `code_verifier` 모두 0건. 이메일 형식 0건. URL 0건. 사용자 프로필 경로(`C:\Users\...`)가 시작 줄에 1건씩 있음 (로그 파일 경로, 비밀 아님).
- ✅ 훅 범위: 클릭 처리는 UIA에서 얻은 시계 rect(`HookRects`)에 한정하며 그 외는 통과. P1 QA에서 통과한 항목이고 이번 커밋의 변경은 deadline 계산뿐이라 재검증하지 않음. 훅 disable 시 rect를 비워 통과 상태로 복귀.
- Info: 로그 파일은 `applog`에서 회전/삭제하지 않는 것으로 보임 (calendar.rs 주석 "unrotated"). 장기 사용 시 누적. 파일명이 실행마다 새로 생성되어 개수 증가. Low-Info.

### 4. 미검증 목록
- 실제 토스트 표시 (설치본, 알림 권한/Focus Assist 영향), 알림 클릭 동작 (C-17에 따라 없음).
- 재부팅 후 자동 실행, 제거 후 프로세스 잔존, 재설치 시나리오 (M-2는 코드·파일 구조 근거).
- 오프라인 상태 알림 동작 (M-1은 코드 근거).
- set_settings 동시 호출 경합 및 훅 on/off 연타 (L-2).
- DST·타임존 변경, 슬립/재개 직후 실제 동작.
- 두 번째 인스턴스 실행 (사용자 앱 보호를 위해 생략).
- Google JSON 실제 응답 파싱: 이번 diff 범위 밖 (이전 P3 QA에서 통과).

### 5. 요약
High 0 / Medium 2 / Low 5 / Info 5. 사용자가 체감할 가능성이 높은 순서는 M-2 (재설치 후 자동 실행 소실), M-1 (오프라인·재로그인 시 알림 중단), L-1 (트레이 토글 상태 불일치). 테스트·clippy·로직 테스트는 모두 통과.

QA DONE

model : sonnet
