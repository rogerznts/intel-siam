# Cliente nativo Intel SIAM: especificação

Visualizador de câmeras do DVR Intelbras, nativo, substituindo o SIMPlay. Prioridade macOS, depois Linux; Windows por último.
Motivação: o SIMPlay é x86-only (roda sob Rosetta, quebrou), força grade de 9 e esquece o layout a cada abertura.

## Requisitos funcionais

- `FR-001` — grade de 1 a 12 câmeras, para cobrir gravadores de até 12 canais. Qualquer N, não só quadrados:
  6 câmeras arranjam em 3×2 ou 2×3, 10 a 12 em 4×3 ou 3×4. O layout escolhe linhas/colunas equilibradas para o N atual.
- `FR-002` — janela redimensionável. Ao mudar o tamanho, os tiles refluem mantendo proporção 16:9 por tile.
- `FR-003` — reordenar arrastando. O usuário arrasta um tile para outra posição e a ordem troca.
- `FR-004` — solo por duplo-clique. Duplo-clique num tile mostra só aquela câmera em tela cheia; outro duplo-clique
  volta à grade anterior, intacta. Solo é modo de visualização, não altera o layout salvo.
- `FR-005` — estado persistente. Quais câmeras, em que posição, o tamanho/posição da janela e a grade atual são
  salvos e recarregados na próxima abertura, sem reorganizar nada à mão.
- `FR-006` — login na nuvem. Fluxo OAuth2 PKCE (ver [cloud-auth.md](../protocol/cloud-auth.md)), com refresh token
  guardado para não logar toda vez. Lista de dispositivos via `GET /video-devices`.
- `FR-007` — vídeo por túnel. Um túnel `libt2u` por DVR (porta 554); cada canal é uma sessão RTSP a
  `rtsp://127.0.0.1:<porta>/cam/realmonitor?channel=N&subtype=S` (ver [libt2u.md](../protocol/libt2u.md)).
- `FR-008` — substream nos tiles, mainstream no solo. Tiles usam `subtype=1` (baixa resolução, leve); o solo usa
  `subtype=0` (full HD). Reduz CPU/banda com até 12 streams simultâneos.

## Requisitos não funcionais

- `NFR-001` — macOS em primeiro lugar (ambiente de desenvolvimento atual); concluído o macOS, a prioridade passa a ser
  Linux, e Windows vem depois, sem reescrever a UI.
- `NFR-002` — leve: binário único quando possível, poucas dependências de runtime, inicialização rápida.
- `NFR-003` — decode por hardware desejável (VAAPI no Linux, D3D11VA no Windows, VideoToolbox no macOS), mas atrás
  de uma fronteira para não travar o esqueleto. Primeiro decode por software em substream; HW depois.
- `NFR-004` — segredos fora do git: tokens e credenciais do DVR em arquivo local ignorado, nunca no código.

## Arquitetura em camadas

Desenho detalhado e decisão de decode em [viewer-architecture.md](viewer-architecture.md).

1. **Cloud** — OAuth2 PKCE + `/video-devices`. Entrega tokens e, por dispositivo, serial, `username`, `password`,
   `service_port`, canais. Já provado em `tools/probe/oauth_login.py`; vira módulo Rust.
2. **Tunnel** — abre o túnel `libt2u` e devolve a porta local. A lib é x86_64, então roda num helper filho sob
   Rosetta (evolução de `tools/probe/t2u_tunnel.c`). `NFR-003` deixa espaço para um backend P2P nativo depois.
3. **Decoder (contrato de processo)** — um filho que escreve frames RGBA de tamanho fixo no stdout. Primeira
   implementação com o `ffmpeg` CLI; a fronteira permite trocar por GStreamer/VAAPI sem tocar na UI.
4. **UI** — grade, drag, solo, resize, persistência. Consome frames da camada 3 como texturas.
5. **State** — ordem dos tiles e geometria da janela salvas em disco, mais o refresh token; a fonte da verdade de
   `FR-005`. O modo solo fica só em memória (`FR-004`).

## Decisão de GUI (decidida: egui + FFmpeg, 2026-10-27)

| Opção | Prós | Contras |
|---|---|---|
| **egui/eframe + FFmpeg** (recomendada) | Grade, drag, solo e resize são triviais em modo imediato e idênticos nos 3 OS; GPU-backed; binário único fácil de empacotar; já usamos FFmpeg | Upload de frame para textura é manual; decode por software até plugar HW accel |
| **GTK4 + GStreamer** (`gtk4paintablesink`) | HW decode VAAPI e RTSP via `rtspsrc` quase de graça; nativo no Linux | Runtime GStreamer+GTK pesado; empacotar no Windows é penoso; drag/grid custom é mais boilerplate; há bug de colorspace VAAPI+paintable reportado |
| **Slint + FFmpeg** | Declarativo, leve, 3 OS; exemplo FFmpeg oficial | Windows exige vcpkg+LLVM; integração de vídeo menos madura que egui para grade dinâmica |

Recomendação: **egui/eframe para a UI, FFmpeg atrás do trait `VideoSource`**. As interações (grade dinâmica, drag,
solo, persistência) são o coração do produto, e o modo imediato do egui as torna baratas e iguais nos três sistemas.
Começar com substream em software adia a peça cross-platform mais difícil (HW decode) sem travar a UX
(**Laziness Protocol**, **Outcome-Oriented Execution**). O trait mantém a porta aberta para GStreamer/VAAPI no Linux.

## Fora de escopo por ora

- Gravação/playback de vídeo gravado no DVR.
- PTZ, áudio, alarmes, múltiplos DVRs ao mesmo tempo.
- Reimplementar o protocolo P2P em Rust (continua via `libt2u` sob Rosetta).
- HW decode (previsto pela fronteira, não no primeiro esqueleto).
