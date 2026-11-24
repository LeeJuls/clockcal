# 구글 연결 정보 만들기 (한 번만)

clockcal 은 설치 파일에 구글 로그인 정보(OAuth 클라이언트)를 담고 있지 않습니다. 각자 본인 구글 계정으로 아래를 한 번만 하면 됩니다. 비용은 없고, 내 일정은 내 PC 와 구글 사이에서만 오갑니다.

## 절차

1. [Google Cloud 콘솔](https://console.cloud.google.com/projectcreate)에 구글 계정으로 접속해 **새 프로젝트**를 만듭니다 (이름 예: `clockcal`).
2. **API 및 서비스 → 라이브러리**에서 [**Google Calendar API**](https://console.cloud.google.com/apis/library/calendar-json.googleapis.com)를 찾아 **사용**을 누릅니다.
3. [**API 및 서비스 → OAuth 동의 화면**](https://console.cloud.google.com/apis/credentials/consent) (Google Auth platform)
   - 사용자 유형(대상): **외부(External)**
   - 앱 이름 `clockcal`, 지원 이메일·개발자 이메일은 본인 Gmail
   - **데이터 액세스(범위)** 에 아래 두 개를 추가합니다.
     - `https://www.googleapis.com/auth/calendar.events`
     - `https://www.googleapis.com/auth/calendar.readonly`
4. [**대상(Audience)**](https://console.cloud.google.com/auth/audience) 에서 게시 상태를 **프로덕션으로 게시(Publish app)** 합니다. 검증 신청은 하지 않습니다.
   - "테스트" 상태로 두면 **7일 뒤 로그인이 풀립니다.** 꼭 프로덕션으로 바꾸세요.
5. [**사용자 인증 정보**](https://console.cloud.google.com/apis/credentials) → **사용자 인증 정보 만들기 → OAuth 클라이언트 ID** → 애플리케이션 유형 **데스크톱 앱** → 만들기.
6. 만들어진 클라이언트의 **JSON 다운로드**를 누릅니다.
7. clockcal 을 열면 "내 구글 캘린더를 연결하세요" 화면이 나옵니다. **JSON 파일 가져오기**로 받은 파일을 선택합니다(내용을 붙여넣어도 됩니다).
8. **구글 캘린더 연결**을 누르고 브라우저에서 로그인합니다.
   - "Google에서 확인하지 않은 앱" 경고가 뜨면 **고급 → clockcal(안전하지 않음)으로 이동**을 누릅니다. 내가 만든 앱이라 나오는 정상 화면입니다.
   - 권한 화면에서 캘린더 항목을 허용합니다.

가져온 파일은 `%APPDATA%\clockcal\profiles\personal.json` 에 저장되고, 로그인 토큰은 Windows 자격 증명 관리자(`clockcal/personal`)에 저장됩니다.

## 문제 해결

| 증상 | 원인·해결 |
|---|---|
| 가져오기에서 "올바른 '데스크톱 앱' 연결 정보 JSON이 아닙니다" | 5단계에서 유형을 "웹 애플리케이션"으로 만든 경우입니다. **데스크톱 앱**으로 다시 만드세요. |
| 로그인 때 `access_denied` | 동의 화면에서 취소했거나, 3단계 범위가 빠졌습니다. 범위 두 개를 확인하세요. |
| 일주일 뒤 다시 로그인하라고 나옴 | 4단계 게시 상태가 "테스트"입니다. 프로덕션으로 게시한 뒤 다시 로그인하세요. |
| 일정 응답(예/아니요/미정)이 안 됨 | 예전에 `calendar.events` 없이 로그인한 경우입니다. 설정에서 로그아웃 후 다시 로그인하세요. |
| 연결 정보를 바꾸고 싶음 | 설정 → 구글 연결 정보 → JSON 파일 가져오기. 새 정보로 다시 로그인하게 됩니다. |

## 연결 해제

설정에서 **로그아웃**을 누르면 구글에 토큰 폐기를 요청하고 저장된 토큰을 지웁니다. 구글 계정의 "서드 파티 앱 및 서비스" 에서도 직접 제거할 수 있습니다. 만든 클라우드 프로젝트를 삭제하면 완전히 끝납니다.
