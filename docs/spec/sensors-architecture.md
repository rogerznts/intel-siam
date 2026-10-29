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
fn main() -> ExitCode { intel_siam::sensors::run_vision(std::env::args_os().skip(1), Onnx::load) }
```

## Shape

### A decisão que segura o resto: um módulo dono das duas pontas do fio

`sensors` contém o cliente do app (`client.rs`), o protocolo (`wire.rs`), os tipos de domínio (`types.rs`) e o
processo de visão inteiro (`vision/`). `wire`, `client` e `vision` são submódulos privados. Os dois lados do protocolo
são o mesmo código, com o mesmo `serde`, compilados na mesma build. Para o resto do repositório, o fio não existe.

```
src/sensors/
  mod.rs        facade: Sensors, run_vision, vision_child, Network, pub use types::*
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
    infer.rs    registro de modelos, download, embedding pelo `Network`, treino da regressão logística
```

O pacote ganha `src/lib.rs` (os módulos compartilhados) e um segundo binário, `intel-siam-vision`, que implementa
`sensors::Network` com `ort`. A lib nunca nomeia o `ort`, então o binário da UI não linka o runtime ONNX, e o
`rusqlite` e o `image`, que a UI não chama, saem no `-dead_strip`. Isso preserva o argumento do ADR 0001 de que o
runtime ONNX não pesa no binário da UI (medido em "Implementation reconciliation").

### Protocolo: estado desejado desce, modelo de leitura sobe

Transporte: stdin e stdout do filho, com stderr indo para o log redigido, como em todo filho. Um codec nos dois
sentidos: `u32 header_len | JSON | u32 payload_len | bytes`. Só `Report::Image` tem payload, em RGBA8 cru, então o
app não precisa de decodificador de imagem. O cabeçalho tem teto de 1 MiB na leitura e na escrita. A escrita
recusa um cabeçalho maior antes de tocar no pipe. Uma violação na leitura derruba o link.

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
`Interest { sensor, tab, history_before, samples_before, open }`, e o processo de visão empurra o `FocusView` correspondente sempre
que algo nele muda: frame novo da prévia, amostra nova, item revisado. Esse é o mesmo idioma do `reconcile`. Não
existe id de requisição, resposta atrasada de outra aba nem reenvio. Um `Interest` repetido não envia nada (o cliente
compara com `link.sent`).

Amostras usam páginas de até 100 linhas, ordenadas por id decrescente. `samples_before` seleciona ids menores que
o cursor. `Detail::Samples { rows, before, older }` confirma o cursor recebido e entrega o próximo, ou `None`.
O cursor vem das linhas consultadas, não das miniaturas legíveis, então arquivos danificados não bloqueiam a navegação.
O painel mantém os cursores anteriores para voltar uma página. Os campos de cada `SampleItem` são numéricos ou
temporais de tamanho limitado, mantendo o cabeçalho muito abaixo de 1 MiB mesmo com uma biblioteca sem limite de amostras.

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

**Versão.** `PROTOCOL = 2`, com paginação de amostras. Uma versão diferente é `ExitReason::Incompatible`, terminal como o 401: o supervisor não
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
- **Snapshot HTTP.** Uma thread busca a imagem ao aparecer a fonte e quando uma checagem ou prévia pede.
  `Feeds::frame` esconde a diferença entre stream contínuo e snapshot. `poll` retenta falhas após 5 s,
  mesmo sem sensor treinado ou painel aberto, com no máximo uma busca em voo por fonte.
  Usuário e senha recebem percent-decoding separado antes de montar Basic Auth. A URL da requisição não contém userinfo.
- **Fila de inferência.** Uma worker (`infer`) roda `Embed` e `Train` em ordem. Cada sensor tem no máximo uma
  checagem em voo; uma checagem que vence com outra em voo é pulada, não enfileirada.
- **Treino.** `Teach`, `Resolve` e `Relabel` marcam `retrain_at = now + 1 s`, e cliques seguidos viram um treino só.
  A amostra é endereçada pelo SHA-256 da imagem (`UNIQUE(sensor, sha256)`), então um clique duplo grava uma linha
  só. O job leva os embeddings em cache e os caminhos das amostras sem cache. A worker calcula o que falta, faz o fit
  e devolve o head e os vetores novos. O engine grava os dois.
  Ao carregar o banco, sensores com amostras rotuladas ou head existente ganham treino imediato. Essa reconstrução
  reaproveita embeddings persistidos e cobre encerramento antes do treino, rótulos alterados, exclusão de todas as
  amostras e mudança de região. Não há revisão persistida adicional nem migração de esquema.
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
| `UserCommand` (8), `Interest` (5 campos) | SQLite, arquivos, treino, coalescência, embeddings em cache, retenção |
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

**Núcleo puro (`types`, `wire`, `vision/logic`, `vision/pixels`, `vision/infer` sem I/O).** Diferenças em relação ao
esboço:

- `Region` é uma struct com construtores; a forma (`RegionShape`) é pública para leitura, mas só vira `Region` por
  eles. No esboço, `Region` era um enum público, e qualquer código podia montar um polígono de dois pontos sem passar
  pela validação. A forma canônica segue `imaging.normalise_roi`: retângulo que cobre o quadro vira `Whole`, polígono
  que é retângulo vira `Rect`. Polígono com mais de 32 pontos é recusado, e não truncado como na origem.
- Os ids (`SensorId`, `FrameId`, `ImageId`, `SampleId`, `HistoryId`) têm o campo `pub(in crate::sensors)`: cliente e
  visão constroem, a UI não.
- Os limites e padrões que definem invariantes (`INTERVAL_LIMITS_S`, `DEBOUNCE_LIMITS`, `MAX_STATES`,
  `MAX_POLYGON_POINTS`…) ficam em `types.rs`; os do motor (burst, sonda, revisão, `NEUTRAL_FILL`) em `logic.rs`. O
  nome tem até 64 caracteres, como na origem (o esboço dizia 60).
- `StateSet::next_key` ficou de fora: "menor chave livre" contradiz "chave nunca reaproveitada". A regra sai junto
  com o formulário da UI.
- `SensorStatus` declara `Failed` antes de `Untrained` e `Live`, para que a ordem de declaração seja a precedência.
  `FeedFact` tem `Waiting` no lugar de `NoUrl` e `Starting`, que davam o mesmo status.
- `RowCost` ganhou `pending_review`: como em `cleanup_history`, item à espera de revisão vive o dobro da idade e é
  o último cortado por tamanho.
- O fio não tem `MAX_PAYLOAD`: o tamanho do payload sai do cabeçalho (`Message::payload_len`) e precisa bater com o
  que chegou, com cada lado da imagem em `1..=MAX_WIRE_EDGE`. `HEARTBEAT` entra com o processo de visão.
- `logic::changed` decide o limiar de 4 %; `change_score` é interno.
- `Head::predict` recebe o `StateSet` e devolve todos os estados, com 0 para os que não têm amostras
  (`Head.predict(all_keys)` da origem). O `Head` ainda não guarda backbone nem região: isso entra com o `store`.
  O `fit` reproduz `classifier.fit` da origem até ~1e-15 nos dois conjuntos de teste (2 e 3 estados).
- `pixels` usa um só reamostrador, o filtro triangular do BILINEAR do PIL, também no `letterbox`, que na origem é
  BICUBIC. Amostras e checagens passam pelo mesmo caminho, então a diferença não atravessa nenhuma fronteira.
  `fit_rgba` recebe o lado maior (`MAX_WIRE_EDGE`), não um par.

**Processo de visão (`vision/{mod,engine,store,feeds,infer}`, binário `intel-siam-vision`).** Diferenças em
relação ao esboço e ao texto acima:

- **O ONNX Runtime mora no binário, não na lib.** `sensors` expõe `trait Network` (roda um tensor NCHW e devolve
  a última dimensão e os valores da primeira saída) e `type LoadNetwork`; `run_vision(args, load)` recebe o
  carregador, e `src/bin/intel-siam-vision.rs` implementa os dois com `ort`. A chamada 4 vira
  `run_vision(std::env::args_os().skip(1), Onnx::load)`. Motivo medido: com `ort` usado dentro da lib, o binário
  da UI carregava os objetos do `ort` (as CGUs da lib que a UI usa também tinham código do `infer`), e os
  inicializadores estáticos C++ do `libonnxruntime.a` sobreviviam ao `-dead_strip`: 811 símbolos `onnxruntime`,
  `CoreML.framework` e `libc++` no `otool -L`, +0,9 MB. ThinLTO não resolveu (700 símbolos) e quebrou o link do
  binário de visão (`___isPlatformVersionAtLeast`). Com o `Network`, a lib nunca nomeia o `ort`: o binário da UI
  tem 0 símbolos `onnxruntime`/`sqlite3` e não linka CoreML nem libc++.
- `build.rs` passa `-Wl,-dead_strip_dylibs` aos binários no macOS: cada um fica só com as dylibs que usa. O
  binário de visão ainda linka AppKit e afins, porque compartilha CGUs com `app`/`ui` e as referências a classes
  Objective-C nunca são removidas pelo linker. Risco aceito por ora; resolver pede tirar `app`/`ui` da lib.
- **Modelo sob demanda de verdade.** O download começa quando algum sensor tem amostras para treinar ou um head
  para checar, não quando um sensor existe. Sensor sem amostras fica `Untrained` e não baixa 24 MB. Sem head
  válido (backbone e região atuais) não há checagem: o valor fica `None`, em vez de checagens que só dariam
  `Unknown`.
- `procs` ganhou `ChildKey::UrlDecoder(UrlKey, StreamKind)` (o `UrlKey` é um prefixo do SHA-256 da URL, então
  nem a chave nem o nome do log carregam a senha), `take_frame(&ChildKey)` e `input_geometry(&ChildKey)`, que
  o logger preenche com a primeira linha `Stream #…: Video: …, WxH` (`frames::parse_input_geometry`).
  `plan::ffmpeg_args(input: &str, kind, Rate)` serve aos dois processos; `Rate::Sampled(2)` põe `fps=2` no
  filtro. `ExitReason::label` saiu do `ui` para o `status`, porque o `SourceDown` também precisa do texto.
- O fio não tem mais `Frame<M>`: `read_frame` devolve `(mensagem, payload)`.
- **Store.** Amostra em `img/s/<sensor>/<sha256>.jpg` (por sensor, para apagar um sensor nunca apagar o arquivo
  de outro que ensinou o mesmo frame); histórico em `img/h/<sensor>/<id>.jpg`. `sample` e `history` são
  `AUTOINCREMENT` e o histórico grava o arquivo antes da linha usando o próximo id de `sqlite_sequence`: ids
  nunca se repetem, então um `ImageId` derivado do id da linha nunca aponta para outra imagem. Não há arquivos
  de miniatura: a miniatura (≤ 160 px) sai do frame gravado quando é enviada pela primeira vez. Embeddings em
  BLOB `f32` LE; o head em JSON (`Head` ganhou serde), com backbone e hash da região em colunas.
- **`ImageId`.** Prévia conta a partir de 1; imagens gravadas usam o id da linha com a espécie nos dois bits
  altos (miniatura de amostra, miniatura de histórico, frame aberto), então a mesma imagem tem o mesmo id em
  qualquer `FocusView` e sobe uma vez só.
- **Feeds.** Snapshot busca uma vez ao aparecer, para o status sair de `WaitingSource` sem esperar checagem, e
  guarda a imagem no tamanho nativo (a região é normalizada). Falha espera 5 s para tentar de novo.
- **Engine.** O anel de `Teach` guarda os frames da prévia (`FRAME_RING = 5`, origem `frame_cache_size`),
  porque é o frame que o usuário vê. A sonda compara com a assinatura da última checagem cheia (como na
  origem); sem essa base, a sonda vira checagem cheia sem burst. `Configure` que muda a região ou os estados
  agenda retreino. `Infer` espera a worker terminar o job ao ser descartado: os destrutores estáticos do ONNX
  Runtime rodam no `exit` e falhavam uma inferência em voo.
- **Correções da revisão.** `redact_userinfo` aceita apóstrofo dentro da autoridade antes de ocultar o userinfo,
  tanto no `SourceView` quanto no stderr. A prévia nunca fornece prazo vencido ao laço. Com frame antigo,
  mantém a imagem exibida e espera o feed avisar ou o próximo prazo de outro trabalho.
- **Paginação de amostras.** O limite de 100 linhas é aplicado no SQL antes de carregar imagens. O cursor continua
  disponível mesmo quando a página perde miniaturas ilegíveis. O leitor e o escritor compartilham o teto de cabeçalho.
  A versão 2 exige atualizar os dois executáveis juntos.
- **Teste do processo.** É um teste de unidade da lib (`vision/process_tests.rs`), não um `tests/`: o fio é
  privado, e só de dentro de `sensors` dá para falar o codec. Como o cargo não constrói binários para testes de
  unidade, o teste roda `cargo build --bin intel-siam-vision` e tira do ambiente as variáveis que o cargo dá ao
  teste (`OUT_DIR`, `CARGO_PKG_*`…); com elas, scripts de build que as observam reconstruíam metade da árvore a
  cada alternância entre `cargo test` e o build interno.
- **Evidência da correção, 2026-10-28.** Os testes reproduziram antes da correção a senha exposta no stderr,
  treino perdido, prazo vencido, retentativa ausente, Basic Auth sem decoding e cabeçalho de amostras rejeitado.
  Após as correções, `cargo test` passou com 104 testes e `cargo clippy --all-targets` terminou sem avisos.
  `engine_tests.rs` percorre 12.000 amostras pelo codec, sem duplicação ou perda, e avança após uma página
  inteira com imagens ilegíveis. `process_tests.rs` navega 205 amostras em páginas de 100, 100 e 5 pelo stdin/stdout
  do executável real e retorna à primeira página. Os testes HTTP usam somente um servidor em loopback.
  Um smoke separado do executável debug recebeu `Hello` 2, criou uma fonte RTSP pausada com apóstrofo na senha,
  conferiu a URL redigida, abriu Amostras, excluiu o sensor e encerrou com código 0 ao fechar stdin, sem rede externa.
- A linha `Stream #0:0: Video: …, WxH` foi observada com o ffmpeg 9.0.2 numa entrada `lavfi` (704x480); de um DVR
  real continua [H] (pergunta 3 abaixo).

**Lado do app (`sensors/client`, `sensors_ui`, `plan`, `procs`).**

- `Sensors::new` recebe o mesmo `Waker` do supervisor em vez de `egui::Context`. O leitor converte os
  pixels fora da UI; `FocusView::images()` limita tanto o cache de imagens quanto o de texturas.
- `ChildIo::Duplex` entrega os pipes uma vez por instância e um contador de progresso. `fail` comunica
  o erro terminal de versão ao supervisor. O cliente conserva entradas e `Wants` enquanto aguarda a nova
  instância; o primeiro `Focus` encerra o snapshot e remove entradas ausentes. Foco e imagens são
  descartados ao adotar o filho, porque os ids de prévia reiniciam.
- Comandos do usuário descem antes do `Desired` no mesmo frame. Assim, o `Create` existe quando chega
  o interesse que o seleciona. Ações com o link fora são descartadas na entrada e na desconexão.
- A ausência de ffmpeg não impede abrir a visão para sensores HTTP; nesse caso o argv recebe `ffmpeg`
  para resolver pelo PATH. Ausência do binário de visão aparece no painel. `cargo run` tem binário
  padrão explícito, e o README pede `cargo build --release` antes para construir também o companheiro.
- O formulário cria as chaves 1–9 uma vez. Não há remoção/adição de estados depois de criar, portanto
  nenhuma chave apagada é reaproveitada. A região e a pausa são alteradas por `Configure`.
- Os logs dos decoders de visão ficam em `logs/vision`, separados dos decoders da grade. O histórico oferece
  a página seguinte mais antiga e retorno à mais recente. Amostras também permitem voltar à página anterior,
  com uma pilha de cursores privada do painel e confirmação de `before` para não desenhar uma resposta de outra página.

**Tipo Objetos (sem rostos), 2026-10-28.** Entrou como variantes, sem mensagem nova de protocolo:
`KindSettings::Objects`, `KindLive::Objects`, `SensorValue::Objects`, `UserCommand::TeachBox` e
`UserCommand::SetDetector`, `ReviewReason::Ask`, `Job::Detect` e `Job::Index`. Diferenças em relação ao texto acima e à
origem:

- **`Network` devolve todas as saídas** (`Vec<Output>` com forma e valores), não a última dimensão da primeira: o
  D-FINE tem duas (`logits` e `pred_boxes`). O binário cria **um** ambiente do ONNX Runtime por processo
  (`LazyLock`); o smoke mostrou que o segundo modelo carregado falhava com "only one environment is allowed per
  process".
- **Registro de modelos** é `infer::Model { Backbone, Detector(Detector) }`, com URL, SHA-256 e tamanho copiados de
  `detectors.json`. A escolha do detector é global, como a configuração `detector` da origem, gravada em `setting`
  e exposta em `SystemStatus.detector`. O conjunto `coco` (80 classes, nomes em pt-BR) fica em `sensors/coco.rs`.
- **Uma checagem de Objetos é um job só**: o worker recorta a região com margem, roda o detector, faz o
  `postprocess`, chama `logic::filter_detections`, `teach::plan`, embeda as caixas uma a uma e chama `teach::apply`.
  Dois jobs (detectar, depois comparar) pediriam um estado intermediário no engine sem ganho. As funções continuam
  puras; o engine cuida de trilhas, histórico e perguntas.
- **Caixas ensinadas são amostras** na mesma tabela `sample` (migração 2: `answer`, `taught`), guardando o recorte
  com margem, não o quadro. O embedding usa a tabela `embedding` com região `0` (`TAUGHT_CROP`, a `roi_key`
  "taught" da origem). O índice de comparação vive só na memória e é reconstruído no início, após ensinar,
  responder, reetiquetar ou configurar, pelo mesmo `retrain_at` dos Estados. `SampleLabel` cobre os dois tipos;
  `Resolve` e `Relabel` passaram a recebê-lo.
- **Rótulo próprio** tem id escolhido pelo app (`LabelId`), em vez do slug da origem: o `Configure` que cria o
  rótulo e o `TeachBox` que o usa vão no mesmo quadro. Remover o rótulo apaga as caixas ensinadas com ele.
- **Prévia de Objetos** é o último quadro checado, para as caixas ficarem sobre o quadro em que foram achadas
  (origem: "o quadro muda junto com as caixas"). Antes da primeira checagem, a prévia é o quadro ao vivo. Só o
  último quadro checado fica na memória por sensor.
- **Histórico**: uma linha por checagem com todos os eventos (`On`, `Off`, `Filtered`) e as caixas, em vez de uma
  linha por chave; cada pergunta é uma linha própria na revisão. A pergunta não se repete enquanto outra sobre o
  mesmo rótulo espera resposta, como em `_record_question`.
- **Não portado nesta unidade**: aviso "visto de leve" ao desenhar caixa perdida, ensino a partir de quadro do
  histórico (só prévia e resposta da revisão), limites de ensino por sensor (`TEACH` é constante), restaurar
  "visto por último" do histórico no início, rostos e pessoas (próxima unidade).
- `Report::Sensor` leva `Box<SensorEntry>` (o clippy apontou a variante grande). `PROTOCOL = 3`.
- **Evidência, 2026-10-28.** `cargo build --all-targets --release`, `cargo test` (138 testes) e
  `cargo clippy --all-targets` sem avisos. Smoke do executável de visão real em
  `docs/spec/sensors-ui-verification.md`. As fotos da origem têm tamanhos diferentes (640×421 e 640×424); trocar
  uma pela outra na mesma URL vira `GeometryChanged`, como deve, então o smoke usou o gato redimensionado.

**Pessoas dentro dos sensores de Objetos, 2026-10-28.** Entrou como variantes e uma migração aditiva:
`FaceSettings` em `ObjectsSettings` (`#[serde(default)]`, desligado), `ObjectKey::Person`/`Stranger`,
`Found.face: Option<FaceLook>`, `ReviewReason::Face`, `Tab::People` com `Detail::People`,
`UserCommand::{NameFace, RenamePerson, ForgetPerson, RelabelFace, SetRecogniser}`, `Job::IndexFaces`,
`Model::{FaceDetector, Recogniser}` com URL, SHA-256 e tamanho de `faces.json`, migração 3 (`person`, `face`,
`face_embedding`) e `<data>/img/f/<sha256>.jpg`. `PROTOCOL = 4`. Diferenças em relação ao texto acima e à origem:

- **Pessoas no `SystemStatus`, rostos no `FocusView`.** Nome, quantidade de rostos, onde a pessoa está agora e
  onde/quando foi vista por último vão no `SystemStatus.people` (no máximo 10), porque o selo do tile precisa dos
  nomes a cada frame. As miniaturas dos rostos vêm pelo `Interest { tab: People }`, e `FocusView.sensor` virou
  `Option` (`None` para as pessoas).
- **Nomear não roda modelo de novo.** A checagem guarda o rosto de cada caixa de pessoa (`FaceLook`, com os cinco
  pontos normalizados); `NameFace` recorta o rosto do quadro guardado (anel de `FRAME_RING` quadros checados) ou do
  quadro da revisão (`FaceQuestion.face`). A origem roda o YuNet de novo na caixa. Só um rosto procurado naquele
  quadro (`age == 0`) e sem problema é ensinado.
- **Uma checagem de Objetos continua um job só**: detector, caixas ensinadas e rostos (`look_for_faces`) no worker;
  `faces.rs` é puro (região da cabeça, decodificação do YuNet, alinhamento, filtro de qualidade, `even_light`,
  `identify`, `decide`, `name_face`, `keep_name`, `plan_search`). O burst à procura de rosto e as perguntas sobre
  rostos ficam em `logic.rs` (`steer_face_burst` em `Schedule.face_burst`, `face_questions`).
- **Limiares como constantes** (`FACES`, `MIN_FACE_PX` = 30, similaridades por reconhecedor). Sem ajuste global ou
  por sensor, como decidido no grill. A escolha SFace/AuraFace é global, como o detector.
- **Alinhamento** pela solução complexa de mínimos quadrados, que é o Umeyama sem reflexão em 2D; amostragem
  bilinear com centros de pixel em `+0,5`, como o `transform(AFFINE)` do PIL. `even_light` reproduz o
  `ImageOps.equalize` do canal Y em ponto flutuante.
- **O índice de pessoas é um só**, construído quando o reconhecedor fica pronto e reconstruído `RETRAIN_DELAY`
  depois de ensinar, mover, apagar ou trocar de modelo. Enquanto ele não existe, as checagens seguem sem rostos.
  Sem ninguém ensinado, o reconhecedor nem é carregado. Falha ao embedar conta como falha do modelo de rosto, com
  a mesma retentativa.
- **`Identity` não guarda o rosto mais próximo**: nada o consumia.
- **Não portado nesta unidade**: fotos enviadas do celular, rosto suspeito e pessoas parecidas, estatística
  "rosto utilizável em 9 de 40", quadro do histórico para rosto desconhecido sem pergunta (`_record_face`),
  ensinar rosto a partir do histórico aberto (só prévia e revisão), "visto por último" restaurado do histórico no
  início (as aparições valem só para o processo atual), exportação (não existe exportação no app), liberar o
  modelo de rosto da memória quando todos desligam.
- **Evidência, 2026-10-28.** `cargo build --all-targets --release`, `cargo test` (154 testes) e
  `cargo clippy --all-targets` sem avisos. Smoke do executável de visão real com YuNet e SFace baixados em
  `docs/spec/sensors-ui-verification.md`. As fotos da origem têm alturas diferentes (600 e 640 px); a de 2018 foi
  reduzida e completada com preto para 480×600, para não cair em `GeometryChanged`.

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
