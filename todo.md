# todo: cliente nativo Intel SIAM

Spec: docs/spec/viewer.md. Fluxo: pstack poteto-mode / Feature + architect.

## Decisão (fechada 2026-10-27)
- [x] GUI + backend de vídeo: egui/eframe + FFmpeg atrás do contrato de processo do decoder.

## Esqueleto (depois da decisão)
- [x] `architect`: tipos e fronteiras (docs/spec/viewer-architecture.md). Pacote único, sem workspace; decode por
      `ffmpeg` filho em vez de trait `VideoSource` linkado.
- [x] Pacote Cargo único com módulos model, layout, cloud, plan, procs, frames, status, ui.
- [x] cloud: OAuth2 PKCE + refresh + GET /video-devices (porta do oauth_login.py para Rust).
- [x] tunnel: helper x86_64 (`helper/t2u_tunnel.c`) com protocolo READY/DOWN e vigia de stdin, embutido pelo build.rs.
- [x] decoder: `ffmpeg` filho, rawvideo RGBA fixo (Sub 640x360 nos tiles, Main 1920x1080 no solo).
- [x] app/UI: grade dinâmica 1-9 (FR-001), resize/reflow (FR-002), drag-reorder (FR-003),
      solo por duplo-clique (FR-004), persistência de estado (FR-005).
- [x] state: ordem dos tiles no storage do eframe + janela + refresh token em secrets.json.
- [x] Verificar ao vivo: 6 câmeras na grade, reordenar, solo, fechar e reabrir mantendo o estado (2026-10-27).

## Pendências do esqueleto
- [ ] Tirar a senha do DVR do argv do ffmpeg (visível por `ps`); ex.: proxy RTSP local que injeta a autenticação.
- [ ] Medir CPU com 12 Sub + 1 Main antes de decidir o NFR-003.
- [ ] Ver ao vivo a mensagem do tile para senha recusada (401).

## Depois do esqueleto
- [ ] HW decode atrás do trait (VAAPI/D3D11VA/VideoToolbox) — NFR-003.
- [x] App macOS (`tools/package/macos.sh`, assinatura ad-hoc, instalado em /Applications).
- [ ] Assinar com Developer ID e notarizar; empacotar Linux, depois Windows.

## Sensores de Estado
- [x] Lib compartilhada, supervisor com `Waker`, núcleo puro, codec, SQLite e processo de visão.
- [x] Cliente do app, túneis para sensores fora da grade e reenvio declarativo após reinício.
- [x] Painel com criação, região, ensino, amostras, revisão, histórico e retenção; selos na grade.
- [x] Binário de visão incluído no pacote macOS.
- [ ] Capturar o stderr de um DVR real para confirmar a detecção de geometria. Mudança sem reconexão pode
      passar despercebida até reiniciar o decoder.
- [ ] Medir CPU do decode Main a 2 fps antes de adotá-lo por padrão.
- [ ] Separar app/UI da lib para retirar AppKit do processo de visão.
- [ ] Rever proteção de URLs avulsas com senha no SQLite 0600.

## Roadmap (futuro)
- [ ] Gravação contínua e por evento.
- [ ] Screenshots de uma câmera ou da grade.
- [ ] Integração com Google Drive (envio de gravações e screenshots).
- [ ] Sensores de Objetos.
- [ ] Sensores de Pessoas e reconhecimento facial.
- [ ] Sensores de Leituras (placas, texto e valores).
- [ ] Publicação MQTT.
- [ ] Modo CLI servidor (headless).
- [ ] Servidor MCP para câmeras e eventos.
