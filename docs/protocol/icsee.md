# iCSee: avaliação para integrar câmeras XMeye/iCSee ao Intel SIAM

Alvo: `/Applications/iCSee.app` 2.6.7 (`com.XM.CSee`), app de iOS rodando no Mac (`Wrapper/iCSee.app`). Análise de
2026-10-27. Tags: `[O]` observado (com arquivo, símbolo ou endereço), `[I]` inferido, `[H]` hipótese.

## O que é o app

- `[O]` O executável principal `iCSee` é Mach-O arm64 com `LC_ENCRYPTION_INFO cryptid 1` (FairPlay). O código do
  próprio app não é legível sem decifrar o binário num dispositivo; por isso a análise foi no SDK.
- `[O]` Toda a lógica de câmera está em `Frameworks/FunSDK.framework/FunSDK` (arm64, `cryptid 0`,
  `LC_VERSION_MIN_IPHONEOS 11.0`, sha256 `31c72cd6…2be967`). É o **FunSDK da Xiongmai/JFTech**, o mesmo SDK
  documentado publicamente em `docs.jftech.com`.
- `[O]` O FunSDK exporta 283 funções `FUN_*` (C++, `nm -gU | c++filt`), entre elas `FUN_SysLoginToXM`,
  `FUN_SysGetDevList`, `FUN_DevLogin`, `FUN_MediaRealPlay`, `FUN_DevGetConnectType`, `FUN_DevGetLocalPwd`,
  `FUN_SysSaveDevToken`.
- `[O]` `FUN_DevLogin` (`0x100d14`, REA/Ghidra, evidência `ev_1e28be70…`) só registra o log
  `Fun_DevLogin[DevId,hUser,Seq,UserName]` e posta uma mensagem assíncrona em `CDataCenter`; o protocolo de fio
  fica nos handlers internos.
- `[O]` `FUN_MediaRealPlay(hUser, devId, canal, tipoStream, view, seq)` (`0x10b260`, evidência `ev_001ffdd6…`)
  delega a `MediaRealPlayEx` e desenha numa view do UIKit; o SDK renderiza o vídeo, não entrega URL.

## Como o vídeo chega

- `[O]` Três caminhos de conexão aparecem nas strings do FunSDK: P2P sobre KCP (`kcp handshake request…`),
  relay RPS (`rps agent init`) e DSS, o servidor de mídia em nuvem (`CSquareDSS`, `Fun_GetDSSAbility`,
  `pub-dss-hls.secu100.net`).
- `[O]` Servidores embutidos: `secu100.net` (status, upgrade, shadow, DSS), `xmeye.net`, `xmcsrv.net`,
  `jftechws.com` (`rs.jftechws.com`, `upgrade.jftechws.com`).
- `[O]` O FunSDK monta a URL `%s://%s/v2/device/token/%s/%s.rs`, o mesmo formato do endpoint público
  `tks.jftechws.com/v2/device/token/{timeMillis}/{signature}.rs` da OpenAPI da JFTech.
- `[O]` A lista de dispositivos da nuvem traz credencial do aparelho: o FunSDK contém os campos `devUserName`,
  `devPassWord`, `devUsername`, `devPassword` e `DevPassword`.
- `[O]` O app guarda localmente 4 dispositivos em `Documents/CSee_user.db`, tabela `devInfo`, com ids de 20
  caracteres e `iCodecType = 4`. Os valores não foram copiados para este repositório.

## Caminhos para o Intel SIAM

| Caminho | Como | Esforço | Funciona fora de casa | Risco |
|---|---|---|---|---|
| **A. OpenAPI JFTech** | REST: token do dispositivo → login → `/device/livestream` devolve URL **RTSP ou HLS** (válida por 10 h) | baixo; cabe no contrato atual (o `ffmpeg` abre a URL) | sim | exige conta no open platform (`uuid`, `appKey`, `appSecret`, `movecard`) |
| B. LAN direta | RTSP na 554 ou DVRIP na 34567 da câmera | mínimo | não | só funciona na mesma rede |
| C. Reimplementar o P2P XM | KCP/RPS/DSS em Rust, como fizemos com o `libt2u` | alto | sim | protocolo fechado e maior que o `libt2u` |
| D. Reusar o FunSDK do iOS | carregar o framework num processo macOS | inviável | — | depende de UIKit, OpenGLES e GLKit, só de iOS |

- `[O]` OpenAPI (`docs.jftech.com`): `POST tks.jftechws.com/v2/device/token/{timeMillis}/{signature}.rs` com
  headers `uuid` e `appKey` e corpo `{"sns": [...]}`; funções do aparelho em `rds.jftechws.com`; `/device/livestream`
  é "advanced feature" e pede `userToken`, obtido em `rs.jftechws.com/login/va1/…` com conta e senha cifradas em
  AES.
- `[I]` A rota A é a escolha natural: o FunSDK do iCSee fala com o mesmo backend (`jftechws.com`, mesmo formato de
  token), e uma URL RTSP entra no Intel SIAM sem helper nem túnel, também no Linux e no Windows.
- `[H]` O `userToken` exige "usuário RS do open platform". Não se sabe se a conta do iCSee serve com o `appKey` de
  outra aplicação; precisa de teste com uma conta do open platform.
- `[H]` `/device/livestream` pode ser cobrado ou exigir aprovação da aplicação; não verificado.
- `[H]` Câmeras iCSee novas podem não ter senha conhecida pelo usuário (o FunSDK tem `FUN_DevGetLocalPwd` e
  `FUN_SysSaveDevToken`); a credencial pode precisar vir da lista da nuvem (`devPassWord`).

## Referência externa

- `wire67/xmeye_cloud_to_rtsp` faz o mesmo que a rota C com o `NetSdk.dll` da Xiongmai (Windows), por CloudID +
  usuário + senha, e reemite o stream. Relata HEVC, main 3696×1080 e sub 1280×360 numa câmera dupla.

## Ler da conta própria do iCSee (rota E)

Reproduzir o login da conta do app e baixar a lista de dispositivos com as credenciais locais de cada câmera, como
fazemos hoje com a nuvem Intelbras. É HTTP, não o protocolo P2P binário.

- `[O]` O app principal (`iCSee`, Mach-O arm64) tem só 4096 bytes cifrados (`LC_ENCRYPTION_INFO cryptsize 0x1000`)
  e `flags=0x0` (sem hardened runtime). Quase todo o Objective-C e as strings são legíveis.
- `[O]` Login de conta é HTTP assinado em `https://rs.jftechws.com`: `{base}/ios/login/va1/{timeMillis}/{sign}.rs`
  (usuário/senha), além de `/mobilephone/`, `/wx/`, `/fb/`, `/line/login/va1/…`. É o mesmo `rs.jftechws.com/login/va1`
  da OpenAPI, mas com o `uuid`/`appKey` do próprio app, não de uma conta de open platform.
- `[O]` Classes de login no binário: `accountLoginWithAccount:pass:comletion:`, `cloudAccountLogin`,
  `AccountLoginResult`.
- `[O]` Criptografia observada no binário: `AESCryptoWithString:key:isECB:option:` (AES, modo ECB opcional) para
  conta/senha, e `hmacMd5:hmacKey:` para a assinatura; `uuid`, `appKey`, `appSecret`, `appMoveCard` são ivars de init
  (`SetParmeValue[_uuid,_appKey,_movedCard]`).
- `[O]` Hosts da nuvem do consumidor: `api.jftechws.com`, `boss22-api.xmcsrv.net`, `app-support.jftech.com`,
  `secu100.net`. A lista de dispositivos é um endpoint de nuvem sob o `userToken`, ainda não isolado.
- `[O]` SSL pinning só nas libs de anúncio (`AdScopeAFSecurityPolicy`, `AFLRSSLPinningMode…`); o networking do app
  usa `AFSecurityPolicy` padrão. Um proxy HTTPS com CA confiável deve capturar login e lista.
- `[H]` `uuid`/`appKey`/`appSecret` do iCSee: os campos existem, os valores não foram fixados (podem ser montados em
  runtime). Precisam vir da decompilação do init ou de uma captura ao vivo.
- `[H]` A conta de e-mail/telefone do usuário é obrigatória para o login; sem ela não há o que ler.

### Captura ao vivo (2026-10-27): o que a conta expõe por HTTP

Proxy HTTPS (mitmproxy) com CA local; o app auto-logou com o token salvo e abriu a lista. SSL pinning só nas libs de
anúncio; os endpoints XM passaram em claro.

- `[O]` O app manda em todo request os headers fixos `uuid: xmeye` e um `appKey` (header), mais `timemillis` e
  `signature` (MD5 hex). São as chaves da aplicação; valem para qualquer instalação do iCSee, não são do usuário.
- `[O]` Token de conta: `POST boss-api.jftechsoft.com/account/userToken/login` com o `accessToken` salvo (JWT, 30
  dias) devolve `accessToken` + `publicKey` RSA. O login por e-mail/senha (`/ios/login/va1`) não disparou porque o
  token ainda era válido.
- `[O]` Token de dispositivo: `POST api-sa.jftechws.com/gwp/v3/rtc/device/token` com `{"sns":[...],"accessToken":…}`
  devolve um token opaco por aparelho, usado nos serviços de nuvem.
- `[O]` Com esse token dá para `getDeviceAlarmList` (eventos de IA: detecção de pessoa, horário, canal) e `getPicUrl`
  (URL assinada de um **snapshot** JPEG do evento). Imagens periódicas, não vídeo ao vivo.
- `[O]` **Veredito: a conta por HTTP NÃO entrega vídeo ao vivo nem a credencial RTSP da câmera.** Não apareceu
  nenhuma URL `rtsp/hls/m3u8/livestream`, nenhuma senha de câmera e nenhuma lista de dispositivos com credencial. O
  vídeo ao vivo e a lista com senha trafegam pelo protocolo P2P binário da Xiongmai, fora do HTTPS.
- `[I]` Ler da conta serve para metadados (séries que a conta possui, alarmes de IA, snapshots). Para o Intel SIAM
  mostrar vídeo ao vivo das câmeras iCSee continua valendo a rota C (reimplementar o P2P, caro) ou a rota pelo DVR
  (ONVIF), se a câmera estiver no gravador.

Dois jeitos de ler o que a conta expõe (metadados):
1. **Captura ao vivo.** Feito aqui; entrega endpoints, assinatura e um token real sem reimplementar cripto.
2. **Reimplementar o HTTP em Rust.** As chaves (`uuid`/`appKey`) e o algoritmo de sign estão no binário.

O vídeo em si ainda usa P2P/DSS (rotas B/C) ou, se a câmera estiver no DVR, o túnel Intelbras que já temos.

## Reimplementar o P2P da Xiongmai (rota C, detalhada)


O vídeo das câmeras XM/iCSee se divide em duas camadas independentes. Mapeia direto na arquitetura que já temos para
a Intelbras (cloud → túnel → RTSP).

### Camada 1: o protocolo do aparelho (fácil, documentado)

- `[O]` DVRIP ("Sofia") em TCP 34567 é a interface real; RTSP em 554 sozinho já basta para vídeo
  (refs: `Moxnatiy/hass-xmeye/docs/protocol.md`, `OpenIPC/python-dvr`, `alexshpilkin/dvrip`).
- `[O]` Login: `sofia_hash(senha)` = MD5 da senha, cada par de bytes mapeado num alfabeto base62, 8 chars;
  payload `{"EncryptType":"MD5","LoginType":"DVRIP-Web","PassWord":<hash>,"UserName":<user>}`, msgid 1000.
- `[O]` RTSP direto: `rtsp://HOST:554/user=USER&password=PASS&channel=N&stream=0.sdp?real_stream` (canal a partir de
  1; stream 0 = main, 1 = sub). O `ffmpeg` abre essa URL como já faz com a Intelbras.
- `[I]` Portar isso para Rust é trabalho moderado, análogo ao que o Intel SIAM já faz com RTSP sobre o túnel `libt2u`.

### Camada 2: o túnel P2P de nuvem (difícil, pouco documentado)

- `[O]` Para alcançar a câmera remota sem IP público, o app usa o "XMEye P2P Cloud": protocolo UDP proprietário
  (hole punching / relay) contra `xmeye.net`/`secu100.net`, pelo CloudID do aparelho. É a única parte que exige
  engenharia reversa; a `hass-xmeye/protocol.md` diz isso explicitamente.
- `[O]` É o papel equivalente ao da `libt2u` no lado Intelbras: abre um caminho e deixa o DVRIP/RTSP correr por dentro.
- `[O]` **Diferença crítica.** A `libt2u` é Mach-O e por isso roda no helper x86_64 sob Rosetta. O NetSDK PC da
  Xiongmai (que faz esse túnel) é DLL do Windows ou `.so` do Linux, e **não** carrega no macOS nem sob Rosetta
  (Rosetta roda Mach-O x86_64, não PE/ELF). Não há lib nativa para reusar no Mac.
- `[O]` `wire67/xmeye_cloud_to_rtsp` faz exatamente este caminho, mas no Windows, via `NetSdk.dll` (não é
  reimplementação). Relata que o build 2024 do NetSDK tem o login de nuvem quebrado (`-12003`); o de 2018 funciona.

### Caminhos possíveis

| Caminho | Como | Esforço | macOS nativo |
|---|---|---|---|
| C1. Reimplementar o UDP P2P em Rust | do zero, a partir da RE do protocolo de nuvem XM | alto (semanas) | sim |
| C2. Helper em Linux com o NetSDK `.so` da XM | FFI do NetSDK num processo/VM Linux, DVRIP por dentro | médio | não (precisa de Linux) |
| C3. Ponte na LAN das câmeras | qualquer máquina sempre-ligada no local das câmeras fala DVRIP/RTSP direto | mínimo | n/a |

- `[I]` C3 é de longe o mais simples: sem nuvem, sem P2P. Se houver um aparelho sempre ligado na rede das câmeras
  (um Raspberry Pi, um PC, o próprio DVR), ele abre o RTSP local e o Intel SIAM consome. Só não resolve acesso remoto
  sozinho.
- `[H]` Modelo de credencial: o DVRIP aceita usuário/senha locais (`admin` + senha definida no app). Câmeras iCSee
  novas podem exigir só token do app; confirmar com um teste real antes de qualquer porte.

### Antes de codar: spike de de-risco

Provar a cadeia com UMA câmera e uma ferramenta pronta, antes de escrever Rust:
1. Se a câmera for alcançável numa LAN: `python-dvr`/`dvrip` + `ffmpeg` no RTSP 554. Responde "a senha funciona?" e
   "o vídeo é H.264/H.265?".
2. Se só por nuvem: `wire67/xmeye_cloud_to_rtsp` num Windows, por CloudID + usuário + senha. Responde "o túnel de
   nuvem funciona daqui?" e "a credencial é senha ou token?".

Precisa do usuário: a senha de uma câmera (definida no iCSee) e onde ela é alcançável (há máquina sempre-ligada na
rede dela?).

## Como o SIM da Intelbras "suporta" essas câmeras

- `[O]` O SIM Next não fala o protocolo da Xiongmai. Manual 1.12.5, nota 1 das Características: "A conexão com os
  dispositivos ocorre utilizando o protocolo de comunicação Intelbras. Dispositivos que não possuam este protocolo
  (ex.: câmeras com conexão exclusiva via Onvif) precisam estar conectadas à um dispositivo de gravação (DVR ou NVR)
  para que o SIM Next possa acessar suas imagens." O FAQ do SIM Next lista só IP, Domínio, Intelbras Cloud e
  Registro Automático como modos de conexão.
- `[O]` O SIMPlay 1.4.1 decompilado também não tem cliente Xiongmai: busca por `xmeye`, `icsee`, `xiongmai`,
  `FunSDK`, `NetSdk`, `34567`, `DVRIP`, `secu100`, `jftech` e `xmcsrv` em `references/decompiled/` sem resultado.
- `[O]` O DVR do usuário, MHDX 3108-C, aceita "8 canais BNC + 4 canal IP ou 12 canais IP no modo NVR", com
  protocolos "Intelbras-1 e Onvif Perfil S" (datasheet Intelbras, fev/2026).
- `[I]` A compatibilidade vem portanto do DVR: a câmera iCSee entra como canal IP via ONVIF, e o SIM (ou o Intel
  SIAM) a vê como mais um canal do DVR pela nuvem Intelbras. Requisito: o DVR alcançar a câmera pela rede, na prática
  a mesma rede local.
- `[H]` As câmeras iCSee costumam ter ONVIF na porta 8899, às vezes desligado de fábrica; confirmar em cada modelo.
- `[H]` Os canais IP do DVR devem aparecer como canais 9 a 12 no RTSP do DVR; a lista de `/video-devices` observada
  tinha só os 8 canais BNC.

## Limites desta análise

- Busca negativa: não procurei no binário principal (cifrado) nem nos handlers internos de `CDataCenter`.
- A análise Ghidra do FunSDK leva ~12 min por consulta pelo CLI; só `FUN_DevLogin` e `FUN_MediaRealPlay` foram
  decompiladas. As demais afirmações `[O]` vêm de tabela de símbolos e strings (`nm`, `strings`, `otool`).
