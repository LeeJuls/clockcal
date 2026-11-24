# clockcal

Windows 11 작업표시줄의 **시계를 클릭하면 구글 캘린더**(월 달력 + 일정 목록)가 뜨는 작은 앱입니다. 기본 달력 대신 내 구글 캘린더를 바로 봅니다. (Tauri 2, Rust)

*English: a tiny Windows 11 app that opens your Google Calendar popup when you click the taskbar clock. You bring your own Google OAuth client (free, one-time setup, see `docs/google_setup.md`); nothing is sent anywhere except to Google.*

## 기능

- 시계 클릭 시 월 그리드, 일정 점, 날짜별 아젠다, 캘린더 색 표시 (트레이 아이콘·전역 단축키는 폴백)
- Google Meet 참여 링크, 참석 응답(예/아니요/미정), 종일·다일 일정
- 일정 시작 전 알림(토스트), 자동 실행, 오프라인이면 캐시 표시
- 구글 계정 1개 연결

## 시작하기

1. 설치 파일(릴리스) 또는 소스 빌드로 앱을 설치합니다.
2. 앱을 처음 열면 안내가 나옵니다. **구글 연결 정보를 한 번만 만들어** 가져옵니다 → [docs/google_setup.md](docs/google_setup.md)
3. "구글 캘린더 연결"을 눌러 브라우저에서 로그인하면 끝입니다.

## 소스에서 빌드

필요: Windows 11, Rust(stable, MSVC), Node 18+.

```powershell
cd app
npm install
npx tauri dev                       # 개발 실행 (시계 클릭 가로채기를 켜려면 -- --hook)
npx tauri build --bundles nsis      # 설치 파일 생성
cd src-tauri; cargo test            # 테스트
```

자세한 구조와 튜닝 포인트는 [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## 개인정보·보안

- 로그인 토큰은 Windows 자격 증명 관리자에만 저장하고 파일로 남기지 않습니다.
- 통신 상대는 구글(OAuth·Calendar API)뿐이며 별도 서버나 분석 도구는 없습니다.
- 연결 정보(OAuth 클라이언트 JSON)는 본인 PC 의 `%APPDATA%\clockcal\profiles\` 에만 있고, 저장소에는 절대 커밋하지 않습니다.
- 요청 범위: `calendar.events`(참석 응답), `calendar.readonly`(캘린더 목록·색).

## 알려진 한계

- 설치 파일이 코드 서명되어 있지 않으면 SmartScreen 경고가 뜹니다. 자체 서명 인증서 안내는 [docs/signing.md](docs/signing.md).
- 시계 클릭 판정은 Windows UI Automation 에 의존해서, Windows 업데이트로 작업표시줄 구조가 바뀌면 깨질 수 있습니다(Win11 build 26300 에서 확인). 깨지면 트레이 아이콘·단축키로 엽니다.
- 구글 앱 검증을 받지 않은 개인 클라이언트라 로그인 때 "확인되지 않은 앱" 경고가 한 번 뜹니다.
- 실기 미검증: 신규 PC 실로그인, 클릭 가로채기 끔 상태의 재시작 유지, 오프라인 알림, 재부팅 후 자동 실행.

## 라이선스

[MIT](LICENSE)
