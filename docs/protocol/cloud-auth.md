# Autenticação e descoberta de dispositivos na nuvem Intelbras

Como o SIMPlay 1.4.1 obtém a lista de dispositivos e, com ela, as credenciais que destravam o RTSP. Código de
`references/decompiled/` (ILSpy sobre as DLLs 1.4.1).

## Fluxo ponta a ponta

1. `[O]` **Login OAuth2/OIDC com PKCE.** O app abre o browser do sistema num endpoint do IdP WSO2
   `idp.conta.intelbras.com/.../login.do` com `client_id`, `code_challenge` (`S256`), `response_type=code`,
   `scope=openid` e `redirect_uri=http://127.0.0.1:<porta-efêmera>/` (fluxo de app nativo, RFC 8252 — redirect em
   loopback). Observado na URL real capturada pelo usuário em 2026-10-27.
2. `[O]` O usuário loga no browser; o IdP redireciona para o loopback com `code`; o app troca `code` +
   `code_verifier` por tokens em `AuthBaseUrl`. O `ClientId` de produção está em `ServiceConstants.cs`, o mesmo da
   URL capturada; o Intel SIAM o lê de `INTEL_SIAM_CLIENT_ID`.
3. `[O]` Com o token, `GET {APIBaseUrl}v1/video-devices` devolve os dispositivos
   (`IntelbrasAccountService.cs:93`).
4. `[O]` Cada dispositivo vem com `UserName`, `Password`, `ServicePort`, `HttpPort`, `Serial` e endereços; o app
   copia direto para o modelo (`UserService.cs:334-335`: `device2.UserName = x.UserName; device2.Password =
   x.Password;`). **É daqui que sai a credencial local do DVR** — o usuário nunca a digita.
5. `[O]` De posse de `Serial` + `UserName`/`Password`, o app abre o túnel (`libt2u`, ver [libt2u.md](libt2u.md)) e
   autentica no DVR. O RTSP na 554 exige essa credencial local, não a conta de nuvem (teste em libt2u.md).

## URLs (produção)

`ServiceConstants.Initialize(isStaging: false)`:

| Nome | URL |
|---|---|
| `AuthBaseUrl` | `https://api.conta.intelbras.com` |
| IdP (browser) | `https://idp.conta.intelbras.com` |
| `APIBaseUrl` | `https://api.simplay.isec.intelbras.com.br/api/` |
| `ISICAPIUrl` | `https://api.isic.isec.intelbras.com.br/api/` |
| `ClientId` | em `ServiceConstants.cs`; no Intel SIAM, `INTEL_SIAM_CLIENT_ID` |

Endpoints de dispositivo em `IntelbrasAccountService.cs`: `/video-devices` (GET lista, POST/PUT atualiza).

## Endpoints exatos (lidos de SimPlay.Web/Authorization.cs)

- `[O]` **authorize**: `GET {AuthBaseUrl}/auth/authorize?response_type=code&client_id={id}&redirect_uri={loopback}&scope=openid&code_challenge={cc}&code_challenge_method=S256`.
- `[O]` **code_verifier**: 128 chars de `[a-zA-Z0-9]`. **code_challenge**: `base64url(sha256(verifier))` sem padding.
- `[O]` **redirect_uri**: `http://127.0.0.1:{porta-livre}/`, servido por um `HttpListener` local que captura `?code=`.
- `[O]` **token**: `POST {AuthBaseUrl}/auth/token`, `application/x-www-form-urlencoded`, campos `grant_type=authorization_code`,
  `client_id`, `redirect_uri`, `code`, `code_verifier`, `scope=openid`. Resposta: `access_token`, `id_token`,
  `refresh_token`, `expires_in`. Renovação: `grant_type=refresh_token`.
- `[O]` **session**: `POST {APIBaseUrl}v1/auth/login` com header `Authorization: {access_token}` e
  `X-SIMPLAY-APP-VERSION: {major.minor}`; devolve o header `x-simplay-session-token`.
- `[O]` **devices**: `GET {APIBaseUrl}v1/video-devices?take=&page=` com headers `Authorization`,
  `X-SIMPLAY-APP-VERSION` e `X-SIMPLAY-SESSION-TOKEN`.
- `[O]` Campos do dispositivo (`DeviceDataModel`): `serial_number`, `username`, `password`, `service_port`,
  `http_port`, `channels[]`, `model`, `useCloudConnection`.

## Teste ao vivo (2026-10-27)

- `[O]` `tools/probe/oauth_login.py` rodou o fluxo inteiro. Login no browser → `access_token` (expires_in 86400) →
  session-token → `GET v1/video-devices` retornou 1 dispositivo (modelo `MHDX 3108-C`, `service_port=37777`,
  8 canais) com usuário e senha locais do gravador.
- `[O]` **Cadeia completa provada**: com essa credencial e o túnel na porta 554, `ffprobe` decodificou o canal 1 em
  HEVC 1920x1080 @ 15 fps, e `ffmpeg` extraiu um frame. OAuth → credencial → túnel `libt2u` → RTSP → vídeo.

## Consequência para o cliente nativo

- `[I]` O login é OIDC padrão com PKCE e redirect em loopback. Um cliente Rust replica isso com uma lib OAuth2 e um
  listener HTTP em `127.0.0.1`, abrindo o browser do sistema. Não precisa de engenharia reversa: é protocolo aberto.
- `[I]` Depois do token, `GET v1/video-devices` entrega serial, porta e credencial de cada DVR. Isso substitui
  qualquer leitura do banco cifrado do app.
- `[H]` A forma exata do JSON de `video-devices` (nomes dos campos) ainda não foi lida; confirmar no
  `IntelbrasAccountService.cs` ou por captura de uma resposta real.

## Em aberto

- Esquema de resposta de `/video-devices` e do endpoint de token.
- Escopos e tempo de vida do token; se há refresh.
