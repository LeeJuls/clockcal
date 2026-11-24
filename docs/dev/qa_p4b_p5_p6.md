## QA 리포트 — clockcal P4-b / P5 / P6 (미커밋 코드)

### 실행 결과 (실측)
- `cargo test` (src-tauri): 128 passed / 0 failed / 1 ignored (doc/bin 0)
- `cargo clippy --all-targets`: 경고·에러 0건 (출력 없음)
- `node dev\logic.test.cjs`: 27 passed (TZ=system)
- `tauri.conf.json`: JSON 파싱 OK (node JSON.parse)

### 결함 표
| ID | 심각도 | 위치 | 증거 | 수정 제안 |
|---|---|---|---|---|
| D1 | med | notify_runner.rs:53-55 + calendar.rs:906 | 20초마다 `get_month`가 호출되고, 성공 경로에서 무조건 `[cal] get_month ... source=Cache` 로그를 남김(calendar.rs:906). applog.rs에는 로테이션·크기 제한이 없음(프로세스당 파일 1개 append). 알림 켜짐 기준 하루 약 4,320줄(현재월), 월말 앞당김 조회 때는 더 많음. 기존 로그(`app-20261124-142437.log`)는 구 빌드라 `[notify]` 0건이라 실측은 못 했고 코드로만 근거함. | 스케줄러 경로는 로그 생략(캐시 히트 로그 제거 또는 verbose 플래그). applog에 크기 상한/로테이션 추가. |
| D2 | med | notify_runner.rs:53, calendar.rs:897-905 | 틱마다 `Cache::for_profile` + `ReqwestHttp::new()`(TLS 클라이언트 생성)를 새로 만듦. 캐시 TTL 300초가 지나면 틱마다 네트워크 시도. 오프라인이고 캐시가 만료된 경우 `get_month_with`가 `Ok(Stale)`를 반환하므로(calendar.rs:~872-878) `ERROR_BACKOFF_S` 백오프를 타지 않고 20초마다 재시도하며 재시도마다 로그가 남음. | 스케줄러는 `Source::Stale`/`error.is_some()`도 백오프 대상으로 처리. HTTP 클라이언트는 재사용. |
| D3 | med | notifier.rs:45 | 선언 거부 판정이 `e.response == "declined"` 하나뿐. 같은 id가 두 캘린더에 있으면 `self_attendee`는 primary 사본에만 신뢰(calendar.rs:339)하는데, 알림은 사본별 `response`를 그대로 씀. primary 사본은 declined이고 공유 캘린더 사본은 `self_response`가 다른 값이면 거부한 일정에 토스트가 뜸. 중복 제거도 (id,start) 첫 사본 기준이라 순서에 의존. UI에는 dedupe가 있고(node 테스트 "dedupe: respond via primary copy") 알림에는 없음. 실제 응답 JSON으로 재현하지는 못함(가설). | 같은 id 묶음에서 primary 사본의 응답을 우선 적용하고, 어느 사본이든 declined이면 제외. |
| D4 | low | notify_runner.rs:68-108 | 스레드 본문에 `catch_unwind` 없음. 패닉하면 스레드가 로그 없이 죽고 이후 알림이 전부 사라짐. spawn 실패도 `.ok()`로 삼킴. | 루프 본문을 `catch_unwind`로 감싸 로그 후 계속, spawn 오류 로그. |
| D5 | low | notify_runner.rs:71, 101 | `notified.json`이 손상되면 `unwrap_or_default()`로 빈 맵이 되어 재시작 직후 창 안에 있는 일정(최대 lead+60s)에 토스트가 한 번 더 뜸. 토스트 표시와 저장 사이(L92~L104)에서 종료되면 같은 중복이 생김. 쓰기는 temp+rename이라 원자적. | 토스트 직전에 키를 먼저 저장(at-most-once). 손상 시 로그 남김. |
| D6 | low | notify_runner.rs:77-79 | `settings::load()`가 20초마다 디스크를 읽음. 파싱 실패는 조용히 기본값(알림 켜짐·10분)으로 돌아가므로, settings.json이 손상되면 사용자가 끈 알림이 다시 켜짐. | 읽기 실패와 파일 없음을 구분하고, 손상 시 마지막 유효값을 유지. |
| D7 | low | notify_runner.rs:53-63 | 갱신 지연: 월 캐시 TTL이 300초이므로 시작 5분 이내로 새로 만들었거나 옮긴 일정은 최대 5분 늦게 반영됨. lead 1~3분을 쓰면 놓칠 수 있음(GRACE 60초). | 알림 임박 구간(`now`~`now+lead`)에서는 TTL을 짧게 쓰거나 강제 갱신. |
| D8 | low | main.js:545-549, 555-560 | 설정 저장 경합. `change` 이벤트마다 `set_settings`가 별도 `spawn_blocking`으로 나가 순서가 보장되지 않음(빠르게 두 번 바꾸면 이전 값이 나중에 저장될 수 있음). 저장 실패 시 체크박스를 되돌리지 않아 화면과 저장 상태가 어긋남. `get_settings` 응답 전에 바꾸면 기본값이 덮어씀. | 직렬화(요청 큐 또는 in-flight 플래그), 실패 시 직전 값으로 되돌림, 로드 전에는 컨트롤 비활성. |
| D9 | low | lib.rs `set_settings` | 자동 시작 레지스트리를 먼저 쓰고 그다음 파일을 저장. 파일 저장이 실패하면 레지스트리만 바뀌고 오류가 반환되어 불일치. 반대(레지스트리 실패)는 파일이 안 바뀌어 일관됨. `get_settings`가 레지스트리를 우선하므로 UI상 영향은 작음. | 순서를 바꾸거나 파일 실패 시 레지스트리를 롤백. |
| D10 | low | autostart.rs:42-63 | `RegQueryValueExW`의 타입을 확인하지 않음(REG_SZ 외 값이면 쓰레기 문자열). `is_enabled`는 값 존재만 보고 경로 일치는 안 봄. 설치 경로가 바뀌거나 삭제된 exe를 가리켜도 "켜짐"으로 표시됨. | 타입 확인, 현재 exe 경로와 비교해 불일치 시 재기록. |
| D11 | info | autostart.rs:2, plan | 자동 시작 명령줄에 `--hook`이 없음(의도된 설계). 재부팅 뒤에는 시계 클릭 가로채기가 꺼진 상태로 시작해 트레이/핫키로만 열림. P1 후크가 핵심 가치라면 plan과 일치하는지 사용자 확인 필요. | 결정 사항 확인. |
| D12 | info | main.js:482 주석 / lib.rs | "Esc는 Rust가 처리한다"는 주석이 있으나 src-tauri/src에 Esc 처리가 없음(grep 0건). 이번 변경으로 설정 시트 닫기만 동작하고 팝업 Esc 숨김은 없음. 주석이 낡았거나 기존 기능 누락. | 확인 후 주석 정리 또는 Esc 숨김 추가. |

### 미확인 (실행 불가 · 사유)
- 레지스트리 라운드트립: 사전 조건은 충족함(`HKCU\...\Run`에 `clockcal` 값 없음, 값 목록 확인). 다만 `set_enabled`를 호출하려면 크레이트에 `#[ignore]` 테스트를 추가해야 하는데(= 소스 수정 필요), 소스 수정 금지와 cargo build 금지 지침 때문에 하지 않았음. reg.exe로는 이 코드 경로를 검증할 수 없어 생략. 코드 리뷰상 `data.len()*2`는 NUL 포함 바이트 수로 맞고, `Key` Drop으로 핸들 누수 없음.
- 토스트 실제 표시(Windows 알림 센터, 설치되지 않은 exe의 AppUserModelID 문제), 슬립/재개, DST 전환: GUI·시간 조작 필요로 미실시. 알림 판정 로직은 절대 타임스탬프 기반이라 DST 영향은 코드상 없음(`start_secs`는 RFC3339 → epoch).
- 아크릴 실제 렌더(Win10/11 빌드별), 설정 시트 가독성, `<select>` 드롭다운이 열릴 때 `Focused(false)`로 팝업이 숨겨지는지(lib.rs:575): 실행 불가로 미확인. 위험은 있음(RSVP 메뉴가 커스텀인 이유와 같은 맥락). 별도 GUI QA 권장.

### 통과 항목 (근거)
- 알림 규칙 순수 로직: 창 경계(-600s 포함, +59s 포함, +60s 제외), 1회 발화, 일정 이동 시 재발화, 종일·취소·거부 제외, 두 캘린더 같은 id 1회, 장기 슬립 후 재생 없음, 잘못된 시작값 무시, prune — notifier.rs 테스트 8건 통과.
- 월 경계: `load_events`가 lead만큼 앞선 시각이 다음 달이면 다음 달 캐시도 합침(notify_runner.rs:57-62). 다음 달 조회 실패는 무시되어 현재 달 알림은 유지.
- 로그아웃(NeedLogin): `get_month_with`가 NeedLogin을 그대로 반환(calendar.rs:~875) → 300초 백오프, 패닉 없음. 로그는 5분당 1줄.
- 시계 역행: 발화 키가 24시간 동안 유지되어 중복 발화 없음.
- 저장 원자성: notified.json/settings.json 모두 pid 붙은 temp 파일 후 rename, 실패 시 temp 삭제.
- 설정: lead 클램프 1..120(테스트 4건), 손상·타입 오류 시 기본값, `set_settings`는 `sanitized()` 적용 후 저장하고 반환(범위 밖 값 불가).
- 레지스트리: 쿼트된 exe 경로, `--hook` 없음, REG_SZ 길이에 NUL 포함, 값 삭제 시 없음(FILE_NOT_FOUND)을 성공 처리, 핸들 RAII.
- UI: `settingsUI`는 `const`로 L536에 선언되고 `init()` 호출은 L586이며, keydown 핸들러(L482)와 show(L517)는 런타임 이벤트로만 참조하므로 TDZ 위험 없음. 목록에 없는 lead 값은 option을 추가해 표시(L546-550). 설정 열림 중 달력 단축키 비활성, Esc로 닫기, 팝업을 다시 열 때 시트를 닫음. `.settings[hidden]` 규칙으로 hidden 동작 보장.
- CSP: `script-src 'self'`, index.html 신규 마크업에 인라인 스크립트·`on*` 핸들러 없음(Rust `win.eval`은 CSP 대상 아님).
- 아크릴: 설정 시트 불투명도 0.82/0.85, RSVP 메뉴는 불투명 기본색, 실패 시 `ACRYLIC_ON=false`로 불투명 폴백, ThemeChanged에서 재적용(lib.rs:587).
- tauri.conf.json: `bundle.active=true`, nsis `installMode: currentUser`, `languages: ["Korean","English"]`, `displayLanguageSelector`는 Tauri 2 NSIS 유효 키이고 `icons/icon.ico` 존재. 실제 번들 빌드는 exe 잠금 때문에 하지 않음.

QA DONE
