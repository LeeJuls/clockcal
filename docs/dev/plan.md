# clockcal — 시계 클릭 구글 캘린더 팝업 (청사진·개발 plan)

상태: 계획 단계 (코드 없음) · 결정 로그 = `decisions.md` (같은 폴더)
위치: 코드 `poc/`·`app/` · 문서 `docs/`

## 1. 목표

- Windows 11 작업표시줄 시계를 클릭하면, 시계 위에 월 달력 + 날짜별 일정 목록 팝업을 띄운다 (스샷: 네이티브 플라이아웃 모양).
- 구글 캘린더 읽기 전용. 계정 2개(회사 Workspace / 개인 Gmail), PC는 서로 다름 → 설정으로 계정 선택.
- 일정 시작 전 알림.
- 스택: Tauri 2 (Rust + 웹 UI).

## 2. 조사 결과 요약

### GitHub (2026-11-17 조회)

- 같은 조합(Win11 플라이아웃 + 구글 캘린더 + Tauri)은 없음. 직접 구현.
- 부품으로 쓸 것: `FabianLars/tauri-plugin-oauth` (Apache-2.0, 루프백), `tauri-apps/tauri-plugin-positioner` (Apache-2.0, 창 위치).
- 참고만(라이선스 없음/GPL → 코드 복사 금지): `Pamplemousse00/Windows-11-Calendar-Flyout`(WinUI3, 가장 유사), Windhawk 모드 계열, `p32929/google-calender-widget`(MIT, 구글 데이터 흐름 참고).

### 시계 클릭 감지 (이 PC 실측: Win11 26300)

- 시계 = UIA `SystemTray.OmniButton` (AutomationId `SystemTrayIcon`). 구 `TrayClockWClass`는 24H2부터 없음. 이름은 현지화·분 단위로 바뀌므로 ClassName으로 매칭.
- 방식: `WH_MOUSE_LL` 훅 + UIA로 캐시한 시계 사각형 히트 테스트 + 클릭 삼키기(DOWN/UP 쌍).
- 미검증: "삼키면 네이티브 플라이아웃이 안 뜬다"는 것은 실험 전. → P1 go/no-go.
- 한계: 터치·Win+N·키보드 경로는 훅을 못 봄 → 트레이 아이콘·전역 단축키 폴백 상시 유지.

## 3. 아키텍처

```
[Rust 코어]
  ├ clock_hook   : 훅 전용 스레드(메시지 루프, 콜백은 rect 비교만) + 워치독 재설치
  ├ taskbar_rect : UIA 전용 MTA 스레드, rect 캐시 갱신(시작·TaskbarCreated·DISPLAYCHANGE·타이머)
  ├ popup        : 숨김 사전 생성 창, 시계 기준 위치, 포커스 아웃 시 숨김, 토글 디바운스
  ├ auth         : OAuth 데스크톱 루프백 + PKCE, 리프레시 토큰 = 자격 증명 관리자
  ├ gcal         : calendarList, events.list(singleEvents=true), syncToken 증분
  ├ cache        : 로컬 캐시(월 단위) — 오프라인에서도 표시
  ├ notifier     : 스케줄러(시작 N분 전), 중복 방지
  └ settings     : 계정 프로필, 알림 분, 자동 시작, 폴백 단축키
[웹 UI]  월 그리드 · 일정 점 · 일정 목록 · Meet 아이콘 · 다크/라이트 · 반투명(Acrylic/Mica)
```

- 계정: 설정에서 프로필(client_id/secret 파일 + 계정 라벨)을 고른다. 회사 PC는 회사 프로필, 개인 PC는 개인 프로필.
- OAuth 클라이언트는 2개: 회사(내부 Internal), 개인(External·프로덕션 게시, 미검증 경고 감수).
  - 이유: 개인 Gmail은 Internal 불가. External이 "테스트" 상태면 리프레시 토큰이 7일 만에 만료됨.
- client secret은 저장소에 커밋 금지(설정 파일 외부 경로).

## 4. 개발 방식 (스파이크 우선)

가장 위험한 부분(클릭 삼키기)을 UI보다 먼저 검증한다. 실패하면 설계가 바뀌므로 P1이 go/no-go.

각 단계: 개발 에이전트 → QA 에이전트 위임 테스트 → 수정 → 사용자 확인 후 다음 단계.
커밋은 `[C_AI]` 태그, push는 사용자 확인 후.

| 단계 | 내용 | 완료 기준 |
|---|---|---|
| P0 | 환경: Rust·MSVC 빌드 도구·WebView2 확인, GCP 프로젝트·OAuth 클라이언트 2종(사용자 작업) | `cargo tauri dev` 빈 창 뜸, client_id 2개 확보 |
| P1 | **PoC(Tauri 밖 작은 Rust 바이너리)**: UIA로 시계 rect 열거 → 훅 → 클릭 삼키기 | 시계 클릭 시 네이티브 안 뜸, 이웃 버튼 정상. **no-go면 폴백만으로 진행(결정 로그 갱신)** |
| P2 | Tauri 골격: 팝업 창(위치·포커스·토글), 트레이 아이콘·단축키 폴백, P1 훅 이식, rect 갱신 스레드 | 시계 클릭→팝업 토글, 보조 모니터·DPI·explorer 재시작 대응 |
| P3 | 구글 연동: OAuth+PKCE, 토큰 저장, calendarList·events, 캐시 | 두 프로필 각각 로그인·일정 조회, 토큰 재사용 |
| P4 | UI: 월 그리드, 점, 날짜 클릭 목록, 색 막대, Meet 아이콘, 반투명·테마 | 스샷 수준 외형, 한국어 |
| P5 | 알림: 스케줄러, 토스트, 시작 N분 전 | 중복 없음, 슬립 복귀 후에도 정상 |
| P6 | 설정 UI, 자동 시작, 단일 인스턴스, NSIS 설치 파일, 서명·AV 대응 | 설치→재부팅→자동 실행 |
| P7 | 전체 QA·릴리즈 | QA 리포트 통과 |

### 알려진 위험

1. 클릭 삼키기 미동작 → P1에서 판정. 대안: 삼키지 않고 우리 창을 위에 띄움(깜빡임 감수) 또는 트레이만.
2. 훅 지연 시 Windows가 조용히 제거 → 워치독.
3. 회사 AV가 전역 마우스 훅을 의심 → 서명, 필요 시 IT 허용 목록.
4. Win 빌드 변경으로 UIA 클래스명 바뀜 → 폴백 매칭(가장 오른쪽 OmniButton).
5. 자동 숨김 작업표시줄에서 rect 부정확 → 무효 rect면 클릭을 통과시킴.
6. 비패키지 앱 토스트는 AppUserModelID 필요(개발 중엔 출처가 다르게 보일 수 있음).

## 5. 에이전트·모델 배분

기존 에이전트(`developer`, `qa-tool`)는 Python/Streamlit·엑셀 도구 중심이라 Rust/Win32에는 안 맞음. 신규 3종을 `~/.claude/agents/`에 만들 것을 제안(승인 대기).

| 에이전트 | 맡을 일 | 모델 | 이유 |
|---|---|---|---|
| clockcal-core (신규) | P1 훅/UIA, P2 창·스레드, P3 OAuth·토큰 | **opus** | unsafe Win32·동시성·보안 |
| clockcal-core (신규) | P3 캐시·gcal 조회, P5 스케줄러, P6 패키징 | **sonnet** | 정형 구현 |
| clockcal-ui (신규) | P4 UI, 설정 화면 | **sonnet** | 프런트 구현 |
| clockcal-qa (신규) | 각 단계 TC 실행·리포트 | **sonnet** | 판정이 섞인 검증 |
| Explore/general | 외부 조사·단순 검색 | **haiku** | 단순 수집 |
| 메인(나) | 단계별 설계·결과 대조·사용자 소통 | — | 위임 프롬프트에 기준 포함 |

- 위임 시 "재위임 금지", "종료 조건·10분 상한" 명시, 완료는 파일 mtime/행수로 확인.
- 라이브 PC 영향(훅·자동 시작)이 있는 테스트는 사용자 확인 후, 훅은 반드시 해제 경로(Ctrl+C·워치독 종료) 포함.

## 6. 저장소·문서

- 코드: 저장소 루트. 하위: `poc/`(P1), `app/`(Tauri, P2~).
- 문서: `docs/` (plan·decisions).
- 설계 결정은 `docs/dev/decisions.md` 에 기록.
