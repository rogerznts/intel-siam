# Cliente nativo Intel SIAM: arquitetura do esqueleto

Síntese do `architect` de 2026-10-27 sobre [viewer.md](viewer.md). Dois candidatos de forma inteira foram desenhados
em paralelo: **A**, decode dentro do processo com `ffmpeg-next`; **B**, decode por `ffmpeg` CLI como processo filho.
A base é **B**, com enxertos de A listados no fim.

## Fatos que moldam o desenho

- `libt2u.so` é Mach-O x86_64 e o app é arm64. Um processo arm64 não carrega essa lib, então o túnel é sempre um
  processo filho x86_64 sob Rosetta (ver [libt2u.md](../protocol/libt2u.md)).
- `ffmpeg-next` 9.0 compila e linka contra o FFmpeg 9.0.2 do Homebrew (testado em 2026-10-27). As duas opções eram
  viáveis; a escolha é de forma, não de viabilidade.

## Decisão: decode por processo filho

| Critério | A: libav no processo | B: `ffmpeg` filho (escolhido) |
|---|---|---|
| Mecanismos de supervisão | dois: threads de decode e o helper do túnel | um só, para túnel e decoders |
| Cancelamento | `AVIOInterruptCB` por contexto, ordem de teardown | `kill` |
| Stream HEVC malformado | derruba o app e todos os tiles | derruba um tile, que reconecta |
| Build | pkg-config + bindgen, acoplado ao major do libav | Rust puro + um arquivo C |
| Erros tipados (401 etc.) | código `AVERROR` | parse do stderr do filho |
| Cópias por frame | uma | uma a mais pelo pipe (~250 MB/s no pico, por conta) |
| Decode por HW (`NFR-003`) | contexto HW no mesmo decoder | `-hwaccel` no argv |

B vence porque o processo filho já é obrigatório para o túnel, e estender o mesmo supervisor aos decoders dá
isolamento de falha e cancelamento por `kill` sem um segundo modelo de concorrência. O custo de cópia fica medido no
primeiro uso real, antes de qualquer otimização.

O trait `VideoSource` da spec vira um **contrato de processo**: um filho que escreve exatamente `w*h*4` bytes RGBA
por frame no stdout, com `w×h` conhecido antes de iniciar. Trocar FFmpeg por GStreamer ou ligar HW decode muda o
argv, não a UI.

## Forma

Um pacote Cargo único na raiz, sem workspace: há um só consumidor, e os módulos puros são testáveis dentro do crate.

| Módulo | Responsabilidade |
|---|---|
| `src/main.rs` | bootstrap do eframe |
| `src/app.rs` | `ViewerApp`: laço por frame, dono de todo o estado, único escritor do layout |
| `src/model.rs` | `DvrSerial`, `ChannelIndex`, `CameraId`, `Layout`, `ViewMode`, `Intent`, `reduce` |
| `src/layout.rs` | geometria pura: `grid_dims`, `tile_rects` (16:9, linhas/colunas equilibradas) |
| `src/cloud.rs` | OAuth PKCE + refresh, sessão, parse de `/video-devices`, `secrets.json`, worker |
| `src/plan.rs` | puro: estado → conjunto desejado de filhos; URL RTSP; argv do ffmpeg |
| `src/procs.rs` | `Supervisor`: spawn, `reconcile`, backoff, stall, kill no `Drop` |
| `src/frames.rs` | tamanhos por `StreamKind`, `FrameSlot`, leitura do rawvideo |
| `src/status.rs` | puro: `tile_status`, `classify_exit` |
| `src/ui.rs` | egui: grade, overlays de status, arrastar, duplo-clique, seletor de câmeras |
| `helper/t2u_tunnel.c` | helper x86_64, compilado pelo `build.rs` e embutido no binário |

O fluxo por frame: `account.poll()` → `procs.poll()` → `ui::draw` devolve `Intent`s → `reduce` →
`plan::desired_children` → `procs.reconcile(desired)`. O `reconcile` é idempotente: mata o que não é desejado ou
mudou de spec, inicia o que falta fora do backoff. Uma porta nova do túnel muda o argv dos decoders, e eles são
reiniciados por desigualdade de spec, sem caso especial.

## Estado

| Salvo | Derivado |
|---|---|
| ordem dos tiles (`SavedLayout { version, tiles: Vec<CameraId> }`, chave no storage do eframe); N = tamanho | linhas/colunas, retângulos dos tiles |
| geometria da janela (eframe `persist_window`) | modo solo (`ViewMode`, só em memória, `FR-004`) |
| refresh token (`secrets.json`, 0600, único escritor: o worker de cloud) | access/session token, lista e credenciais dos DVRs |
| | porta local do túnel, argv, `TileStatus` |

As credenciais do DVR não vão para disco: vêm de `/video-devices` a cada início. Tudo fica em
`eframe::storage_dir("intel-siam")`, fora do git (`NFR-004`).

## Processos

- **Helper do túnel**: `t2u_tunnel <libt2u_path> <serial> <remote_port> <logdir>`. Protocolo de linhas no stdout:
  `READY <porta> RELAY|P2P` e `DOWN <status>`. Faz `poll` no stdin a cada 1 s e consulta `t2u_port_status`; status
  diferente de `1` imprime `DOWN` e sai. EOF no stdin faz `t2u_del_port` + `t2u_exit` e sai 0. Como o stdin fecha em
  qualquer morte do app, inclusive SIGKILL, o helper nunca fica órfão.
- **Decoder**: `ffmpeg -nostdin -rtsp_transport tcp -timeout 5000000 -i <url> -an -vf scale=W:H -f rawvideo
  -pix_fmt rgba pipe:1`. Sub em 640×360 (tiles), Main em 1920×1080 (solo). Morre por EPIPE quando o app morre.
  `[O]` O substream do DVR é 704×480 anamórfico 16:9 sem SAR; por isso o filtro estica, sem `pad`, e a imagem sai
  na proporção certa (conferido ao vivo em 2026-10-27).
- Durante o solo, os substreams continuam vivos e o Main entra junto; voltar à grade é instantâneo. Enquanto o Main
  não chega, o solo mostra o Sub ampliado.
- Falha → `Down { reason, attempt, retry_at }`, backoff 1, 2, 4… até 30 s. `AuthRejected` (401) **não** reinicia
  sozinho, para não bloquear a conta local do DVR.

## Enxertos do candidato A

- `libt2u.so` nunca é embutido (material da Intelbras): caminho por `INTEL_SIAM_LIBT2U`, com padrão
  `references/simplay-1.4.1/libt2u.so` no checkout de desenvolvimento.
- Fora do macOS, o túnel fica `Unavailable(NoBackend)` por `cfg`; o resto compila igual (`NFR-001`).
- 401 como erro terminal do tile, não como falha a reiniciar.
- `eframe::storage_dir` no lugar do crate `directories`.

## Riscos abertos

- `[H]` A senha do DVR aparece no argv do ffmpeg e é visível por `ps` para o mesmo usuário. Aceito no esqueleto
  de desktop de um usuário só.
- `[O]` Substream observado: H.264 704×480 a 15 fps, anamórfico 16:9.
- `[H]` CPU de 12 decodes em software mais um 1080p não medida.
- `[O]` Não há `libt2u` para Linux ou Windows; o túnel nesses sistemas é problema aberto, independente do decode.
