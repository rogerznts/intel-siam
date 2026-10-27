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

Dois jeitos de executar a rota E:
1. **Captura ao vivo (mais barato).** Rodar o iCSee sob um proxy HTTPS com CA local, logar no app e gravar as
   requisições de login e de lista. Entrega os endpoints exatos, a assinatura e um `userToken` real, sem reimplementar
   a cripto. Bloqueia só se houver pinning nos endpoints XM (não observado).
2. **Reimplementar o HTTP.** Extrair `uuid`/`appKey`/`appSecret` e o algoritmo de sign do binário e refazer o login
   em Rust. Mais trabalho, mas vira código no Intel SIAM.

Em ambos, a lista traz série, usuário e senha de cada câmera; o vídeo em si ainda usa P2P/DSS (rotas B/C) ou, se a
câmera estiver no DVR, o túnel Intelbras que já temos.

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
