# 설치 파일 자체 서명

- 인증서: `CN=clockcal (self-signed)`, 코드 서명, RSA 3072, 만료 2029-11-24, 개인키 내보내기 불가(이 PC의 `Cert:\CurrentUser\My`에만 존재)
- 지문: `E4B846ADBF807E4BD7112550D3AA7390922950FB` · 공개 인증서 `signing\clockcal-selfsigned.cer`
- 서명 빌드: `npx tauri build --bundles nsis --config <signCommand 오버라이드 json>` — signtool(`/sha1 지문`, SHA256, DigiCert 타임스탬프). 지문이 PC 종속이라 `tauri.conf.json`에는 넣지 않음
- 효과: 게시자가 `clockcal (self-signed)`로 표시되고 변조 검증 가능. 이 인증서를 신뢰 저장소에 등록한 PC에서만 "알 수 없는 게시자"가 사라짐. SmartScreen 평판 경고는 자체 서명으로는 해결되지 않음
- 신뢰 등록(사용자 셸, 보안 확인 창 1회): `Import-Certificate -FilePath <저장소>\signing\clockcal-selfsigned.cer -CertStoreLocation Cert:\CurrentUser\Root` 및 `...\TrustedPublisher`

## 배포용 빌드
- OAuth 클라이언트는 설치 파일에 포함되지 않는다(C-24). 사용자는 첫 실행 때 안내에 따라 본인 클라이언트 JSON을 `%APPDATA%\clockcal\profiles\personal.json` 에 둔다.
- 서명 빌드: `npx tauri build --bundles nsis --config <서명 오버라이드>`
