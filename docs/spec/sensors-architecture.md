# Sensores: arquitetura da Fase 0 + Estados

Resultado do `architect` (arena com três candidatos; base: candidato 3). Decisões de produto no grill e nos ADRs
[0001](../architecture/adr/0001-sensors-in-supervised-vision-process.md),
[0002](../architecture/adr/0002-sensor-data-in-sqlite.md) e
[0003](../architecture/adr/0003-swappable-detector-dfine-first.md); termos no
[glossário](../architecture/context.md); origem destilada em
[distill-visionstate-sensors.md](../discovery/distill-visionstate-sensors.md).

## Problem

A Fase 0 precisa pôr o primeiro tipo de sensor, Estado, para funcionar de ponta a ponta: criar o sensor, desenhar a
região, ensinar com amostras, ver o valor no selo do tile e no painel, revisar e consultar o histórico. A inferência
roda num processo de visão separado (ADR 0001), dono do SQLite e das imagens (ADR 0002). Três coisas tornam a forma
pouco óbvia:

1. **Dois processos, uma UI que só lê.** O egui redesenha a cada frame a partir de estado em memória, sem async. Tudo
   o que o painel mostra (amostras, fila de revisão, histórico, o frame para desenhar a região) mora do outro lado de
   um pipe, num processo que pode reiniciar a qualquer momento com backoff.
2. **Segredo que atravessa a fronteira.** A URL RTSP local carrega a senha do DVR, que só existe na memória do app. O
   app é dono do túnel, então precisa saber de quais DVRs o processo de visão precisa, mesmo com a câmera fora da
   grade. A senha não pode ir no argv do processo de visão.
3. **O resto chega depois sem mudar a forma.** Objetos e pessoas, Leituras, MQTT e o modo servidor sem interface
   precisam caber como variantes novas, não como mensagens ou tabelas de outro formato.

Restrições herdadas do código: sem runtime async (threads std, `mpsc`, `request_repaint`); `reconcile` idempotente,
com a igualdade de `ChildSpec` decidindo o reinício; `Layout` só muda por `reduce`; `TileStatus` derivado; um leitor
por `FrameSlot`; 401 terminal; logs redigidos; crate único.

## Usage (caller's view)

**O README do módulo, para quem vai mexer na UI:**

> `crate::sensors` é tudo o que existe sobre sensores. O app guarda um `Sensors` e chama quatro métodos por frame:
> `poll` antes de desenhar, `view` para ler, `queue` para pedir algo e `sync` depois do `reconcile`. O que a UI vê
> (`SensorsView`) é uma réplica do estado do processo de visão: nunca escreva nela. Para pedir dados (amostras,
> revisão, histórico, o frame ao vivo) não existe chamada de consulta: declare o que o painel está olhando em
> `Interest`, e a resposta chega sozinha em `view().focus`. Toda ação do usuário é um `UserCommand`, e todos são
> idempotentes. O protocolo, o banco e o motor são privados: um `use crate::sensors::wire` fora do módulo não compila.

**Chamada 1 — o laço do app (`app.rs`), que ganha quatro linhas:**

```rust
self.procs.poll(now);
self.sensors.poll(&mut self.procs);                       // adota filho novo, drena relatórios
let scene = Scene { /* … */ sensors: self.sensors.view(), now };
let actions = ui::draw(ui, &scene, &mut self.ui);
for cmd in actions.sensors { self.sensors.queue(cmd); }
let ports = self.procs.tunnel_ports();
let desired = plan::desired_children(&self.layout, &self.view, self.account.devices(), &ports,
                                     &self.tools, &self.sensors.view().wanted_feeds);
self.procs.reconcile(desired, now);
self.sensors.sync(self.account.devices(), &ports, self.ui.sensors.interest());
```

**Chamada 2 — o selo no tile (`ui.rs`):**

```rust
for entry in scene.sensors.on_camera(cam) {
    let text = match (&entry.status, &entry.value) {
        (SensorStatus::Live, Some(SensorValue::State(k))) => state_name(entry, *k),
        (SensorStatus::Live, _) => "?".into(),
        (status, _) => sensor_status_label(status),       // "pausado", "mudou a resolução", …
    };
    badges.push((entry.settings.name.as_str(), text));
}
```

**Chamada 3 — ensinar e desenhar a região no painel (`ui.rs`):**

```rust
if let Some(focus) = &scene.sensors.focus
    && let Some(preview) = &focus.preview
{
    let tex = panel.images.get(&preview.image);           // textura do frame reduzido
    if let Some(region) = region_editor(ui, tex, &draft.region) {
        draft.region = region;                            // `Region` validado na construção
    }
    for def in states.iter() {
        if ui.button(def.name.as_str()).clicked() || key_pressed(def.key) {
            actions.sensors.push(UserCommand::Teach { id, frame: preview.frame, state: def.key });
        }
    }
}
if ui.button("Salvar").clicked() {
    actions.sensors.push(UserCommand::Configure { id, settings: draft.to_settings()? });
}
```

**Chamada 4 — o binário do processo de visão:**

```rust
fn main() -> ExitCode { intel_siam::sensors::run_vision(std::env::args_os().skip(1)) }
```

## Shape

### A decisão que segura o resto: um módulo dono das duas pontas do fio

`sensors` contém o cliente do app (`client.rs`), o protocolo (`wire.rs`), os tipos de domínio (`types.rs`) e o
processo de visão inteiro (`vision/`). `wire`, `client` e `vision` são submódulos privados. Os dois lados do protocolo
são o mesmo código, com o mesmo `serde`, compilados na mesma build. Para o resto do repositório, o fio não existe.

```
src/sensors/
  mod.rs        facade: Sensors, run_vision, vision_child, pub use types::*
  types.rs      domínio + modelo de leitura + UserCommand + Interest (com serde, invariantes nos construtores)
  wire.rs       privado: Command/Report, enquadramento, PROTOCOL, serialização da RtspUrl
  client.rs     privado: Sensors (lado do app)
  vision/       privado: o processo de visão
    mod.rs      run(): argv, threads, canal de Input
    engine.rs   dono único: config, estado em runtime, Store, Feeds
    logic.rs    puro: agenda, burst, debounce, veredito, revisão, geometria, status, retenção
    pixels.rs   puro: recorte com máscara, assinatura 48×48, letterbox, redução
    feeds.rs    um feed por (fonte, stream): ffmpeg via Supervisor, ou snapshot HTTP
    store.rs    SQLite + arquivos
    infer.rs    registro de modelos, download, ort, embedding, treino da regressão logística
```

O pacote ganha `src/lib.rs` (os módulos compartilhados) e um segundo binário, `intel-siam-vision`. O binário da UI
nunca referencia `sensors::vision`, então o linker deixa `ort`, `rusqlite` e `image` de fora dele. Isso preserva o
argumento do ADR 0001 de que o runtime ONNX não pesa no binário da UI.

### Protocolo: estado desejado desce, modelo de leitura sobe

Transporte: stdin e stdout do filho, com stderr indo para o log redigido, como em todo filho. Um codec nos dois
sentidos: `u32 header_len | JSON | u32 payload_len | bytes`. Só `Report::Image` tem payload, em RGBA8 cru, então o
app não precisa de decodificador de imagem. Os limites são checados na leitura. Uma violação derruba o link, e o
supervisor faz o resto.

| Sentido | Mensagem | Natureza |
|---|---|---|
| app → visão | `Desired { feeds: StreamKey→RtspUrl, interest }` | declarativa; enviada quando muda e a cada filho novo |
| app → visão | `User(UserCommand)` — `Create`, `Configure`, `Delete`, `CheckNow`, `Teach`, `Resolve`, `Relabel`, `SetRetention` | ação idempotente |
| visão → app | `Hello { protocol }` | primeira mensagem; nada secreto desce antes dela |
| visão → app | `Sensor(SensorEntry)` / `Removed(id)` | upsert do estado inteiro de um sensor |
| visão → app | `Wants(BTreeSet<StreamKey>)` | quais streams de DVR ela precisa |
| visão → app | `Image { id, size }` + RGBA | precede o `Focus` que a referencia |
| visão → app | `Focus(Option<FocusView>)` | a resposta ao `Interest` atual |
| visão → app | `System(SystemStatus)` | modelos, uso de disco, retenção; também é o heartbeat (5 s) |

**Por que não há consultas.** O painel não pede "me dá a página 2 do histórico" e espera a resposta. Ele declara
`Interest { sensor, tab, history_before, open }`, e o processo de visão empurra o `FocusView` correspondente sempre
que algo nele muda: frame novo da prévia, amostra nova, item revisado. Esse é o mesmo idioma do `reconcile`. Não
existe id de requisição, resposta atrasada de outra aba nem reenvio. Um `Interest` repetido não envia nada (o cliente
compara com `link.sent`).

**Imagens sem protocolo de cache.** `FocusView::images()` devolve o conjunto de `ImageId` que a visão referencia. O
app guarda exatamente esse conjunto. O processo de visão envia só o que não estava no `FocusView` anterior. Os dois
lados derivam o conjunto da mesma struct, pela mesma função, então nunca divergem. Não existe mensagem do tipo
"ainda tem a imagem X?".

**Reinício do filho.** O supervisor reinicia `ChildKey::Vision` com backoff. `take_pipes` entrega os pipes do filho
novo uma vez só. O cliente então descarta o link antigo, marca a réplica como desatualizada (os selos ficam cinza com
o último valor, não somem), espera o `Hello` e reenvia o `Desired` inteiro. **O último `Wants` é mantido** até o
primeiro snapshot da instância nova, para os túneis de DVRs que só servem sensores não caírem a cada reinício. O
processo de visão começa sempre com um snapshot completo (todas as entradas, `Wants`, `System`, `Focus`).
Como tudo o que desce é declarativo ou idempotente, nenhum dos lados precisa de lógica de recuperação. O que se perde
é o que vivia só na memória: valor publicado, debounce, anel de frames. Na volta, o primeiro resultado é publicado na
hora, como na origem, e um `Teach` com `FrameId` antigo vira o aviso `FrameExpired`.

**Versão.** `PROTOCOL = 1`. Uma versão diferente é `ExitReason::Incompatible`, terminal como o 401: o supervisor não
reinicia até o app reiniciar. Os dois binários saem da mesma build, então isso só acontece quando um binário é
trocado com o app aberto.

**Stdout protegido.** Antes de qualquer outro código, o processo de visão duplica o fd 1 para uso exclusivo do
protocolo e aponta o fd 1 para o stderr, para que um print do ONNX Runtime ou de uma biblioteca nunca corrompa o fio.

**Imagens limitadas.** Nenhuma imagem passa de 960 px no lado maior no fio (`MAX_WIRE_EDGE`); a região é
normalizada, então desenhar sobre a prévia reduzida vale para o stream Main.

### Dono de cada dado

| Dado | Dono (único escritor) | Onde | Quem lê |
|---|---|---|---|
| Configuração do sensor (`SensorRecord`) | engine | SQLite `sensor` + cópia write-through na memória do engine | app, pela réplica `SensorEntry` |
| Amostras, embeddings em cache, head treinado | engine | SQLite + `img/s/` | app, pelo `FocusView` |
| Histórico e fila de revisão | engine | SQLite `history` + `img/h/` | app, pelo `FocusView` |
| Estado vivo (valor, debounce, agenda, anel de frames, assinatura) | engine | só memória | app, pela réplica |
| Modelos | `infer` (thread de download) | `<data>/models/<sha256>` | `infer` |
| Registro de modelos | código: `infer::BACKBONE` | constante | `infer` |
| Credenciais do DVR e URL RTSP local | app (`cloud`) | só memória; descem em `Desired.feeds`, e a visão também só as guarda em memória | visão (`Feeds`) |
| URL de câmera avulsa | engine | SQLite (ver perguntas abertas) | visão |
| `Interest` | UI (`SensorsPanel`) | memória da UI, derivado | visão |
| Selo, `SensorStatus` | ninguém (derivados) | `logic::sensor_status` a cada publicação; selo derivado da réplica a cada frame | UI |
| Túneis necessários | derivado de `view.wanted_feeds` em `desired_children` | — | supervisor do app |

Cada estado tem um dono e um escritor. A réplica do app é só leitura: é substituída inteira por `Report`, nunca editada
aos pedaços. O `Wants` vem da visão porque é ela quem sabe quais sensores estão ativos, focados ou à espera de
geometria. O app só traduz `StreamKey` em URL.

### Dentro do processo de visão

```
stdin reader ─Command─┐
infer worker ─Done────┤ mpsc<Input> ─▶ ENGINE ─Out─▶ stdout writer
model fetcher ─Model──┤               (Store, Feeds, sensores)
feeds waker ──Wake────┘
```

- **Engine.** Uma thread, dono de todo o estado mutável, num laço `recv_timeout(próximo prazo)` seguido de `tick`. É
  o único escritor do SQLite, então não há lock nem conflito de escrita.
- **Decoders.** São os filhos ffmpeg, rodados pelo **mesmo `procs::Supervisor`** do app: `reconcile`, backoff, stall e
  401 terminal, sem uma segunda forma de supervisionar processos. O `Supervisor` deixa de depender de
  `egui::Context` e recebe um `Waker` (`Arc<dyn Fn() + Send + Sync>`): no app ele chama `request_repaint`, no
  processo de visão posta `Input::Wake`. A amostragem é de 2 fps (`Rate::Sampled`): o decode H.264 é inevitável,
  mas a escala, a conversão para RGBA e a cópia no pipe não são pagas a 25 fps. Um feed por (fonte, stream) é
  compartilhado pelos sensores dessa fonte. O engine é o único leitor de cada `FrameSlot`.
- **Snapshot HTTP.** Uma thread de fetch busca a imagem quando uma checagem pede. `Feeds::frame_for_check` esconde a
  diferença entre stream contínuo e snapshot.
- **Fila de inferência.** Uma worker (`infer`) roda `Embed` e `Train` em ordem. Cada sensor tem no máximo uma
  checagem em voo; uma checagem que vence com outra em voo é pulada, não enfileirada.
- **Treino.** `Teach`, `Resolve` e `Relabel` marcam `retrain_at = now + 1 s`, e cliques seguidos viram um treino só.
  A amostra é endereçada pelo SHA-256 da imagem (`UNIQUE(sensor, sha256)`), então um clique duplo grava uma linha
  só. O job leva os embeddings em cache e os caminhos das amostras sem cache. A worker calcula o que falta, faz o fit
  e devolve o head e os vetores novos. O engine grava os dois.
- **Criação.** Um sensor nasce com a região igual ao quadro inteiro e ganha a região desenhada no editor depois;
  não existe decoder temporário para um sensor que ainda não existe.
- **Ausência não apaga.** Dados de um sensor só somem por `UserCommand::Delete` explícito; nenhum `Desired` ou
  snapshot parcial apaga nada.
- **Download de modelo.** `ensure(&BACKBONE)` é idempotente. Enquanto o download corre, o sensor fica em
  `WaitingModel { progress }`. O arquivo é verificado por SHA-256 e só aparece por rename.
- **Retenção.** Roda de hora em hora e a cada `SetRetention`: `logic::retention_plan` sobre o histórico, primeiro por
  idade (7 dias), depois por tamanho (2 GB). As amostras ficam fora.

**Fluxo de uma checagem.** `tick` → `logic::next_check` → `feeds.frame_for_check`. Se for `Probe`:
`pixels::signature`, depois `logic::change_score`. Se der ≥ 4 %, `start_burst` e segue como `Full`. Se for `Full`:
`pixels::crop` → `Job::Embed` → `on_embedded` → `head.predict` → `logic::state_verdict` → `Debouncer::update` →
`logic::review_reason` → `store.record` (quando mudou ou foi para revisão) → `publish`. Esse caminho passa por quatro
arquivos (`engine`, `logic`, `pixels`, `infer`), com o engine como hub: cada um fica a um passo dele, não em cadeia.

**Troca de resolução.** O logger do supervisor lê a linha `Stream #0:0 … Video: …, WxH` do stderr do ffmpeg e grava em
`input_geometry`. `logic::geometry_verdict` compara com a geometria gravada no sensor. Se mudou, o status vira
`GeometryChanged { expected, seen }`, as checagens param e o painel avisa. Salvar o sensor (`Configure`) aceita a
geometria nova.

### Lógica pura, separada do shell

`logic.rs` e `pixels.rs` não fazem I/O, não leem relógio (o `Instant` entra como argumento) nem sorteiam (o `roll`
entra como argumento). Eles contêm `next_check`, `start_burst`, `change_score`, `state_verdict`, `Debouncer`,
`review_reason`, `recent_changes`, `geometry_verdict`, `sensor_status` e `retention_plan`, além de `crop`,
`signature`, `letterbox` e `fit_rgba`. `infer::fit` e `Head::predict` também são puros. As constantes da origem estão
copiadas literalmente, com o nome original ao lado.

### Invariantes nos tipos

- `SensorId` é gerado pelo app, o que torna o `Create` idempotente e deixa o painel selecionar antes da resposta.
- O tipo, a fonte e o stream não estão em `Settings`, então `Configure` não consegue trocá-los. O tipo é o
  discriminante de `KindSettings`, e mudá-lo é recusado com `KindMismatch`.
- `Region`, `NormPoint`, `StateSet` (de 1 a 9, chaves e nomes únicos), `StateKey` (de 1 a 9, nunca reaproveitada),
  `Unit`, `Debounce`, `Interval`, `Name` e `SecretUrl` só existem válidos. O `serde` passa por `try_from`, então o JSON
  que chega pelo fio é validado na fronteira.
- `SecretUrl` e `RtspUrl` têm `Debug` redigido. `RtspUrl` não tem impl de serde: `wire::secret_urls` é o único lugar
  que a serializa, e só para o stdin do filho.
- O embedding fica em cache por (amostra, backbone, `Region::hash`), e o head vale para (backbone, região). Editar a
  região invalida os dois por construção, sem etapa de sincronização.
- `SensorStatus` é derivado a cada publicação e nunca é gravado.

### Como Objetos, Leituras, MQTT e o modo servidor entram

- **Tipos novos.** Entram como variantes em `KindSettings`, `KindLive`, `SensorValue` e `Detail`, com jobs novos em
  `infer::Job`, modelos novos no registro e braços novos no `match` do engine. As mensagens do protocolo e as tabelas
  `sensor` e `history` não mudam de forma (o JSON vai na coluna). Caixas ensinadas, rostos e estatísticas diárias
  entram como migrações aditivas. Pessoas valem para todas as fontes; viram um `Interest` sem sensor e um `Detail`
  novo.
- **MQTT.** É uma saída separada dentro do processo de visão: um segundo consumidor dos mesmos `Report::Sensor` que já
  vão para o stdout (o writer passa a abrir em leque). Não há mensagem nova nem acesso do MQTT ao banco.
- **Servidor sem interface.** É `intel-siam-vision --headless`: sem link no stdin, com fontes por URL vindas do banco.
  Fontes de DVR sem UI pedem que o túnel e a conta rodem dentro do processo; esse é o passo que o roadmap descreve,
  fora deste escopo.

### Profundidade da interface

| O que o chamador vê | O que fica escondido |
|---|---|
| `Sensors`: `new`, `poll`, `view`, `queue`, `sync` | threads de leitura e escrita, enquadramento, verificação de versão, adoção de filho reiniciado, reenvio do estado desejado, réplica, cache de imagens e GC, montagem das URLs RTSP |
| `SensorsView` (dados) + `on_camera`, `image` | origem de cada campo (banco, memória, modelo), derivação de status |
| `UserCommand` (8), `Interest` (4 campos) | SQLite, arquivos, treino, coalescência, embeddings em cache, retenção |
| `run_vision`, `vision_child` | o processo de visão inteiro e o formato do argv, que mora num lugar só |

O app não conhece nenhum tipo de transporte. `Command`, `Report`, `Desired`, `Frame` e `RtspUrl` serializada ficam
dentro de `sensors`.

### Triagem de red flags

- **Módulo raso.** Não. Uma operação do usuário é um `queue`, e uma consulta é um campo de `Interest`.
- **Vazamento de informação.** O formato do fio e o esquema ficam dentro de `sensors`. O formato do argv do filho
  também: `vision_child` monta e `run_vision` interpreta, no mesmo módulo.
- **Decomposição temporal.** O engine agrupa por dono (os sensores), não por etapa.
- **Pass-through.** `run_vision` encaminha para `vision::run`. É de propósito: é a única porta pública para um
  submódulo privado e não acrescenta camada de lógica.
- **Propriedade dividida.** A config tem cópia em memória e no banco, mas os dois são escritos pelo engine no mesmo
  método (write-through). O app nunca escreve.
- **Dois jeitos de fazer.** Um só supervisor de processos (o do app, reusado), um só codec, um só caminho de consulta
  (`Interest`).
- **Internals importáveis.** `wire`, `client` e `vision` são privados, então um import de fora quebra a build.
- **Lista sincronizada à mão.** O conjunto de imagens vem de `FocusView::images()` nos dois lados. Os modelos estão
  numa constante só. Variantes novas de tipo quebram os `match` exaustivos até serem tratadas.

## Synthesis decision

Três candidatos, todos no modelo da sessão (o `task` do omp não aceita escolher modelo por runner), com direções de
partida diferentes para forçar formas distintas: (1) processo de visão dono de tudo, com `Request/Reply` e cache
por revisão; (2) estado por ator, app dono da definição do sensor em `sensors.json`; (3) forma livre. Juiz cruzado:
c3 25, c1 22, c2 20 na rubrica de seis critérios; minha leitura concordou.

**Base: candidato 3.** Menor superfície pública (`Sensors` com cinco métodos), um dono por dado sem cópia
sincronizada, consultas sem ids pelo `Interest` declarativo no mesmo idioma do `reconcile`.

**Enxertos:**

- De c1: stdout do protocolo protegido por duplicação do fd 1; `Wants` mantido durante o reinício (túneis não
  caem); réplica marcada como desatualizada em vez de limpa; amostra endereçada por conteúdo; imagens no fio
  limitadas a 960 px; sensor nasce com região = quadro inteiro.
- De c2: "ausência não apaga"; versão divergente como `ExitReason::Incompatible` terminal.
- Do próprio c3 (alternativa que ele mesmo propôs) com o juiz: `Waker` no `Supervisor` em vez de `egui::Context`
  headless.

**Rejeitado:** a proposta do c2 de reabrir o ADR 0002 (definição do sensor no app). A dependência circular com o
túnel que ela aponta é real, mas `Wants` a quebra sem dividir o dono; e o modo servidor precisa da definição dentro
do processo de visão. O `Request/Reply` com cache por revisão do c1 resolve o mesmo que o `Interest` com mais
mecanismo (ids, pendências, invalidação). NDJSON com base64 (c1, c2) perdeu para o enquadramento binário: um frame
Main em base64 passaria do teto de linha.

## Tradeoffs accepted

- Aceitamos que o app tenha uma réplica que pode ficar alguns milissegundos atrás do processo de visão. Em troca, a
  UI desenha sem nunca bloquear nem esperar resposta.
- Aceitamos reenviar o `SensorEntry` inteiro a cada checagem (alguns KB a cada 10 s por sensor) em vez de deltas. Em
  troca, há uma forma de mensagem por sensor, e a réplica é sempre substituição.
- Aceitamos reenviar o `FocusView` inteiro a cada frame da prévia (2 Hz, metadados apenas). Em troca, há uma forma só
  de resposta.
- Aceitamos imagens em RGBA cru no pipe (8,3 MB para um frame Main aberto, 37 KB por miniatura). Em troca, o app não
  precisa de decodificador de imagem, e o custo vai para o pipe, que aguenta GB/s.
- Aceitamos que o supervisor mate (`SIGKILL`) o processo de visão ao parar, em vez de um encerramento gracioso. Em
  troca, o `stop` não ganha caso especial. Commits do SQLite são duráveis, e arquivo antes de linha garante que não
  fica linha órfã.
- Aceitamos que o `Supervisor` receba um `Waker` em vez de `egui::Context`. Em troca, o mesmo supervisor serve os
  dois processos sem puxar o egui para o binário de visão.
- Aceitamos um processo de visão sempre rodando enquanto o app está aberto, mesmo sem sensores (alguns MB, nenhum
  modelo carregado). Em troca, a lista de sensores e os túneis necessários têm uma fonte só.
- Aceitamos 2 fps fixos para os decoders de sensor. Em troca, sobra CPU. O burst de 2 s e a sonda de 2 s cabem nessa
  taxa.
- Aceitamos que o estado vivo se perca no reinício (o valor fica `None` até a próxima checagem, que é imediata), como
  na origem.

## Alternatives considered

Estas são formas inteiras diferentes, não variações da escolhida. A comparação é por profundidade.

**B. O processo de visão como serviço HTTP local** (`tiny_http` em 127.0.0.1, com token de sessão; o app como
cliente `ureq`).

- *O que oferece:* a API serve depois para o servidor MCP e para o modo headless, e as imagens são corpos HTTP
  nativos.
- *O que expõe ao chamador:* uma rota por necessidade da UI (umas 15: listar, criar, configurar, amostras, revisão,
  histórico paginado, frame ao vivo, imagem por id, ensinar…), além de um cliente por rota. Como não há async, o app
  precisa de uma thread de requisições com ids e de um cache de respostas com invalidação. Os valores ao vivo pedem
  SSE ou polling.
- *O custo estrutural:* surgem dois transportes para um filho (stdio para o ciclo de vida e a porta, HTTP para os
  dados), e a lista de rotas fica sincronizada à mão entre servidor e cliente. No reinício, aparecem respostas
  atrasadas e porta nova.
- *Por que perdeu:* é a forma mais rasa. Cada capacidade nova vira rota, cliente e cache. O ganho (reuso pelo MCP)
  ainda dá para ter depois como mais um consumidor dos `Report` dentro do processo de visão, sem fazer a UI pagar por
  isso agora.

**C. Leitura direta do SQLite pelo app (CQRS)**: a visão escreve; o app abre o banco somente leitura (WAL) por um
`store::Reader` compartilhado e lê as imagens do disco; comandos pelo stdin e valores ao vivo pelo stdout.

- *O que oferece:* o protocolo menor de todos (comandos e valores ao vivo), e as consultas são chamadas de função.
- *O que expõe ao chamador:* a UI passa a saber quando reler (o banco não notifica), o que traz de volta um evento
  "mudou" pelo pipe e duas fontes para a mesma tela (config do banco, valor do pipe), juntadas na leitura.
- *O custo estrutural:* a decodificação de JPEG cai na thread da UI ou ganha um carregador próprio. O frame da prévia
  continua precisando do pipe, porque não está no banco. Mudanças de esquema afetam dois processos vivos, e o
  `rusqlite` entra no binário da UI.
- *Por que perdeu:* a profundidade é parecida, mas a informação vaza. O esquema vira interface pública de fato, que é
  justamente o que o ADR 0002 evita ao dar o banco a um dono só.

**D. RPC sobre stdio** (a forma "óbvia": o mesmo pipe, mas com consultas `GetSamples { req_id }` → `Samples { req_id }`).

- *O que expõe ao chamador:* ids de requisição, correspondência de respostas, descarte de respostas de uma aba já
  fechada, retry no reinício, e uma consulta e uma resposta por tela.
- *Por que perdeu:* o `Interest` declarativo dá o mesmo resultado sem nenhum desses mecanismos e segue o idioma do
  `reconcile` que o app já usa.

**E. Executável único com subcomando (`intel-siam vision`) em vez de lib com dois binários.**

- *O que oferece:* um artefato só para empacotar e assinar, e versão travada por construção.
- *Por que perdeu:* com `ort` estático, o binário da UI carregaria o runtime ONNX, contra o NFR-002 (binário leve) e
  o argumento do ADR 0001. Com `ort` em `load-dynamic`, a dylib baixada sob demanda esbarra na validação de
  bibliotecas do runtime endurecido do macOS. A forma do módulo `sensors` é idêntica nos dois casos: só muda o
  `main`. Se o empacotamento pedir um executável só, a troca é local.

## Implementation reconciliation

*Vazio até a implementação começar.*

## Open questions and risks

Resolvidas na síntese (padrão conservador; reverta se discordar):

1. **Senha do DVR no argv do ffmpeg.** Dívida existente (`todo.md`, "Tirar a senha do DVR do argv do ffmpeg"). Os
   decoders do processo de visão herdam o mesmo `plan::ffmpeg_args`, então a correção futura cobre os dois. Fora
   do escopo desta fase; o processo de visão em si nunca recebe senha no argv.
2. **URL de câmera avulsa com senha.** Fica no `sensors.db`, criado com permissão 0600, no diretório de dados do
   app. `SecretUrl` redige no `Debug` e nos logs; a UI só vê a forma redigida.
3. **Linha `Stream #0:0 … WxH` do ffmpeg.** [H] até capturar stderr real de um DVR em `testdata/`. Troca no meio da
   sessão RTSP sem reconexão pode passar sem ser vista até o próximo reinício do decoder.
4. **Processo de visão sempre de pé** enquanto o app está aberto, mesmo sem sensores: uma fonte só para a lista de
   sensores e os túneis necessários.
5. **`Waker` no `Supervisor`**, não `egui::Context` headless.
6. **L-BFGS próprio**, puro e testado contra valores da origem, em vez de uma crate de otimização.
7. **Ações com o link fora são descartadas** e a UI desabilita os controles; nada é enfileirado para depois.

Riscos:

- **CPU dos decoders em Main.** Mesmo a 2 fps, o decode H.264 1080p é integral. Medir antes de usar Main por padrão
  (`NFR-003` — decode por hardware desejável, atrás de uma fronteira).

## Next implementation step

Escrever `sensors/types.rs` e `sensors/wire.rs` com testes de ida e volta do codec (incluindo limites e o `Debug`
redigido), depois `vision/logic.rs` com os testes portados da origem, antes de qualquer processo ou thread.
