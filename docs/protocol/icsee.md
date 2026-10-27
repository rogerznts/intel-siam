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

## Limites desta análise

- Busca negativa: não procurei no binário principal (cifrado) nem nos handlers internos de `CDataCenter`.
- A análise Ghidra do FunSDK leva ~12 min por consulta pelo CLI; só `FUN_DevLogin` e `FUN_MediaRealPlay` foram
  decompiladas. As demais afirmações `[O]` vêm de tabela de símbolos e strings (`nm`, `strings`, `otool`).
