# libt2u: túnel P2P do SIMPlay

Alvo: `references/simplay-1.4.1/libt2u.so`, Mach-O x86_64, sha256 `7b586e74…f462afa1`. Endereços como no Ghidra.
Código .NET citado de `references/decompiled/` (ILSpy 11.2 sobre as DLLs do SIMPlay 1.4.1).

## O que o túnel é

- `[O]` O `libt2u` é um encaminhador de portas, não fala o protocolo do DVR. `t2u_add_port_v3(uuid, passwd,
  remote_ip, remote_port, local_port)` devolve uma porta TCP local; o app então conecta em `127.0.0.1:<porta>` com o
  Dahua NetSDK (`TryLoginDevice("127.0.0.1", num3, …)` em `SimPlay.Services.Device/DeviceService.cs`, ramo
  `P2P DEFAULT`).
- `[O]` O mesmo mecanismo encaminha outras portas do DVR: `AddHttpPort` tunela a porta HTTP do dispositivo
  (`DeviceService.cs`, `AddHttpPort`).
- `[H]` Por ser um túnel de porta genérico, deve encaminhar também a porta RTSP (554). Se valer, um cliente
  nativo pode usar túnel + RTSP sem o Dahua NetSDK. Confirmar em execução.

## Qual caminho o SIMPlay 1.4.1 usa

- `[O]` Há dois caminhos de nuvem: `libt2u` (`TunnelingService`) e `libibcs` (`_ibcsAdapter`). O IBCS só roda com
  `Constants.Features.AllowP2PIBCS`, que é `false` (`SimPlay.Core/Constants.cs:1202`); nenhuma atribuição a ele foi
  encontrada nas DLLs decompiladas (busca por `AllowP2PIBCS` em `references/decompiled/`).
- `[O]` O DVR do usuário passa pelo `libt2u`: `~/Library/Preferences/SimPlay.log` registra
  `P2P DEFAULT :<serial>` e exceções vindas de `TunnelingService.AddPort`.

## Parâmetros fixos

Constantes em `SimPlay.Services.Tunneling/TunnelingService.cs`. A chave e a senha são segredos da aplicação da
Intelbras embutidos no binário distribuído; não ficam neste repositório. O Intel SIAM as lê do ambiente.

| Constante | Valor ou variável | Uso |
|---|---|---|
| `CLOUD_SERVER_ADDR` | `intelbrasp2p.com.br` | `t2u_init` |
| `CLOUD_SERVER_PORT` | `1250` | `t2u_init` |
| `CLOUD_SERVER_KEY` | `INTEL_SIAM_T2U_KEY` | `t2u_init` |
| `CLOUD_SERVER_PWD` | `INTEL_SIAM_T2U_PASSWORD` | `passwd` de `t2u_add_port_v3` |
| `CLOUD_SERVER_IP` | `127.0.0.1` | `remote_ip` de `t2u_add_port_v3` |

- `[O]` `uuid` é o serial do dispositivo (`device.Serial`) e `remote_port` é `device.ServicePort`
  (`TunnelingService.AddPort`).

## Fluxo no app

`TunnelingService` (`SimPlay.Services.Tunneling/TunnelingService.cs`):

1. `[O]` `Initialize()` chama `t2u_init(ADDR, PORT, KEY)` uma vez.
2. `[O]` `AddPort` chama `t2u_add_port_v3`; retorno fora de `1..65535` vira erro `-10`.
3. `[O]` Faz polling de `t2u_port_status(porta, ref T2uNetStat)` a cada 100 ms, até 100 vezes (10 s), enquanto o
   status for `0` (`T2U_CONN_CONNECTING`). `1` é sucesso; ao estourar o limite o erro vira `-11` (timeout). Em erro
   chama `t2u_del_port`.
4. `[O]` `T2uNetStat.proxy != 0` significa conexão por relay; `0`, P2P direto (`GetConnectionType`).
5. `[O]` Ao remover a última porta, chama `t2u_exit()`.

Códigos de status (`VVEye.Libt2u/Libt2uNative.cs`): `0` conectando, `1` ok, `-1` porta não encontrada, `-2` falha de
conexão, `-3` interrompida, `-4` dispositivo offline, `-5` erro de autenticação, `-10` porta indisponível, `-11`
timeout.

`T2uNetStat` (`VVEye.Libt2u/T2uNetStat.cs`): `ip` (char[20]), `port`, `proxy`, `lost_rate` (float), `bandwidth`,
`remote_nattype`, `local_nattype`, `ip6` (char[40]), `remote_version`.

## Lado nativo

- `[O]` `_t2u_init` (`0x1290d`) só chama `_t2u_init_v2` (`0x1260a`) sem caminho de log.
- `[O]` `_t2u_init_v2(svraddr, svrport, server_key, logpath)` loga `"t2u_init! svraddr:%s svrport:%d"`, chama
  `_vv_nat_init(svraddr, svrport, 0, 0, &callbacks)` (ou `_vv_nat_init_v2` com log), `_vv_nat_set_reconnect(5)`,
  `_vv_nat_set_server_key(server_key)`, e cria as threads `_t2u_main`, `_t2u_recv` e `_t2u_send2local`.
- `[O]` Com `logpath` não vazio grava `<logpath>/t2u.log` e `<logpath>/vvnat.log` (`_t2u_init_v2`). Útil para
  observar o protocolo em execução.
- `[O]` Existe uma segunda pilha paralela, `_t2u2_*` sobre `_vv2_nat_*` (ex. `_t2u2_init` em `0x1575c`, mesma forma
  de `_t2u_init_v2`). O wrapper .NET não declara nenhuma função `t2u2_` (`Libt2uNative.cs`), então o SIMPlay 1.4.1
  não a usa.
- `[O]` `_t2u_add_port_v3` (`0x1052b`): com `local_port == 0` sorteia uma porta em `2000..65535`; retorna `0` se a
  porta local pedida já está em uso por outra entrada.

## Teste ao vivo (2026-10-27)

Harness `tools/probe/t2u_tunnel.c` (x86_64, `dlopen` do `libt2u.so`), DVR do usuário.

- `[O]` O túnel encaminha porta arbitrária. `t2u_add_port_v3(serial, CLOUD_SERVER_PWD, "127.0.0.1", 554, 0)`
  devolveu porta local e `t2u_port_status` retornou `1`. Log nativo: `Add P2P tunnel port: <lp> -> <serial> [<ip>:554]`.
- `[O]` A conexão veio por relay (`proxy=1`), não P2P direto, neste NAT (`natR=3 natL=3`).
- `[O]` O DVR fala RTSP na 554 através do túnel: `ffprobe` em `rtsp://127.0.0.1:<lp>/cam/realmonitor?channel=1&subtype=0`
  recebeu `RTSP/1.0 401 Unauthorized`. Prova que o servidor RTSP respondeu pelo túnel.
- `[O]` A conta de nuvem Intelbras (login por e-mail) **não** autentica o RTSP: o `401` persistiu com ela. O RTSP
  exige o usuário local do gravador (`device.UserName`/`device.Password` no app), que é distinto da conta de nuvem.
- `[O]` Caminho confirmado para um cliente nativo arm64: `t2u` só para o túnel (sob Rosetta) + RTSP via ffmpeg,
  sem o Dahua NetSDK. Com a credencial do DVR vinda da nuvem (ver [cloud-auth.md](cloud-auth.md)), o canal 1
  decodificou em HEVC 1920x1080 @ 15 fps pela porta 554 tunelada.

## Em aberto

- Protocolo de fio entre `_vv_nat_*` e `intelbrasp2p.com.br:1250`: registro, consulta pelo serial, NAT traversal e
  relay. É o que um cliente sem `libt2u` precisa reimplementar.
- Papel de `server_key` e de `passwd` no protocolo (cifra, autenticação ou só identificação).
- Se o túnel aceita a porta RTSP do DVR (ver hipótese acima).
