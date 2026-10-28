# Verificação dos sensores de Estado

## Compilação, testes e pacote

Os comandos abaixo terminaram com sucesso, sem warnings de Rust ou clippy.

- `cargo build --all-targets --release`.
- `cargo test`. Foram aprovados 94 testes.
- `cargo clippy --all-targets`.
- `tools/package/macos.sh`. O pacote inclui `intel-siam` e `intel-siam-vision` em `Contents/MacOS`.

O `otool -L` do executável de visão empacotado lista apenas bibliotecas de sistema e frameworks do macOS.
Não há dylib ONNX externa nesta build. O script copia as dylibs de `target/release`, se existirem.

`src/sensors/client_tests.rs` usa o processo de visão real para criar um sensor, conservar `wanted_feeds`
enquanto o filho reinicia e restaurar o foco em Amostras após reenviar `Desired`. O teste também fornece
uma URL RTSP local e observa `SourceDown`, em vez de `WaitingSource`, nas duas instâncias. Um segundo teste
usa um processo de protocolo com `Hello` versão 999 e observa `ExitReason::Incompatible` sem nova tentativa.

## Smoke na interface real

O app foi iniciado com `set -a; . ./.env; set +a; cargo run --release`.
O servidor de snapshots usou as imagens `open_*.png` e `closed_*.png` de `/tmp/smoke` na porta 8766.
As ações semânticas de acessibilidade do `orca computer` abriram o painel e criaram **Portão Smoke** com
fonte `http://127.0.0.1:8766/snapshot`. Os estados foram **Aberto** e **Fechado**.

Os botões de ensino gravaram uma amostra de cada situação. A interface passou de **sem amostras de dois
estados** para **Fechado**. Depois de alternar o servidor para a imagem aberta, o valor passou para
**Aberto** sem novo ensino. A aba Amostras mostrou as duas imagens e seus rótulos. A aba Histórico mostrou
as checagens e as respectivas miniaturas.
Ao terminar, **Apagar** e **Confirmar exclusão** removeram o sensor de smoke da lista. A interface voltou
a mostrar a lista vazia, sem deixar um sensor tentando acessar o servidor temporário.

As imagens abaixo são recortes das capturas reais do painel. A grade foi excluída do recorte para não
publicar novas imagens das câmeras do usuário.

![Sensor com a prévia do portão fechado, valor Fechado e uma amostra por estado](../images/sensors-closed.png)

![O mesmo sensor após alternar o servidor para o portão aberto, com valor Aberto](../images/sensors-open.png)

![Histórico das checagens com valores, confiança e miniaturas](../images/sensors-history.png)

## Complemento de verificação da interface

A rodada complementar usou o executável release existente, com `.env`, e o mesmo servidor HTTP.
Ela não repetiu a classificação, as abas e os testes automatizados já aprovados acima.
Não houve alteração de código nesta rodada.

### Entrada nativa do macOS

O guia retornado por `orca skills get computer-use --json` distingue entrega de entrada e efeito observado.
`synthetic_input` significa entrada enviada sem comprovação automática, não um erro do editor.
Para `window_not_focused`, o guia recomenda uma tentativa com `--restore-window` e depois foco nativo.
As coordenadas do Orca são locais à janela e precisam considerar `screenshot.scale`.

A janela iniciou em outro monitor, com posição `{-5996, 6}`. O arrasto pelo Orca não mudou a região.
AppleScript, selecionando o processo pelo PID, trouxe a janela para `{50, 60}`, com tamanho `{1400, 850}`.
Mesmo nessa posição, o arrasto pelo Orca não produziu o retângulo esperado.

O caminho alternativo usou eventos reais de mouse do CoreGraphics, publicados em `.cghidEventTap`.
O arrasto enviou `mouseMoved`, `leftMouseDown`, vinte eventos `leftMouseDragged` e `leftMouseUp`.
Os cliques do polígono enviaram `leftMouseDown` e `leftMouseUp`.
AppleScript com `System Events` enviou as teclas. Capturas e releitura do banco comprovaram os efeitos.
O driver Swift temporário foi removido ao final.

### Regiões e atalhos

O sensor temporário **Region Verify Temp** usou a fonte URL do smoke.
O arrasto de tela `{1080, 350}` até `{1370, 510}` produziu o retângulo verde dentro da prévia.
Após **Salvar região**, uma consulta somente leitura em `sensors.db` retornou a região persistida:

```json
{"Rect":{"min":[0.16397229,0.3248332],"max":[0.83371824,0.8519374]}}
```

![Retângulo desenhado dentro da prévia do sensor temporário](../images/sensors-region-rectangle.png)

No modo **Polígono**, os cliques de tela `{1050, 340}`, `{1280, 320}`, `{1380, 500}` e `{1100, 510}`
habilitaram **Fechar**. O botão fechou o contorno verde e voltou a ficar desabilitado.
Após **Salvar região**, a consulta retornou quatro vértices, sem substituir o polígono pelo quadro inteiro:

```json
{"Polygon":[[0.09468822,0.1539646],[0.62586606,0.07185014],[0.85681295,0.8108802],[0.21016166,0.8519374]]}
```

![Polígono de quatro vértices fechado na prévia](../images/sensors-region-polygon.png)

A tecla `1`, sem clicar no botão de ensino, mudou **Aberto (0)** para **Aberto (1)**.
Após alternar o servidor para a imagem fechada, a tecla `2` mudou **Fechado (0)** para **Fechado (1)**.
A captura registra uma amostra de cada estado e o valor **Fechado**.
As teclas de `3` a `9` não foram exercitadas, pois o sensor tinha dois estados.

![Uma amostra por estado após ensino pelas teclas 1 e 2](../images/sensors-keyboard.png)

### Retenção

A ação de acessibilidade `increment` no controle de dias não mudou o valor observado.
O duplo clique nativo abriu o editor numérico. `Cmd+A`, digitação e `Return` alteraram ambos os limites.
A interface mostrou **8 dias** e **2049 MB**. A tabela `setting` confirmou
`max_age.secs = 691200` e `max_bytes = 2148532224`.
Os limites foram aumentados para evitar expiração de dados durante a prova.

![Retenção alterada para 8 dias e 2049 MB](../images/sensors-retention.png)

Os mesmos controles restauraram **7 dias** e **2048 MB**.
A consulta final confirmou `max_age.secs = 604800` e `max_bytes = 2147483648`.

### Selo no tile de DVR

Havia câmeras reais disponíveis na grade. O sensor temporário **DVR Verify Temp**, vinculado ao **Canal1**,
mostrou **DVR Verify Temp: aguardando fonte** sobre o tile correspondente.
Depois de **Pausar**, tanto o painel quanto o selo passaram para **DVR Verify Temp: pausado**.
As imagens abaixo contêm somente os rótulos, sem publicar o vídeo da câmera.

![Selo no tile enquanto o sensor aguarda a fonte](../images/sensors-tile-waiting.png)

![O mesmo selo após pausar o sensor](../images/sensors-tile-paused.png)

Essa prova cobre o selo e a atualização do status no tile, não a mudança de um valor classificado nesse tile.
A classificação com duas situações continua comprovada pelo smoke da fonte URL.
A correção de uma revisão pendente não foi exercitada nesta rodada.

### Limpeza e alcance

Os dois sensores temporários foram removidos pelos botões **Apagar** e **Confirmar exclusão**.
A lista voltou a ficar vazia. A consulta final retornou zero linhas em `sensor`, `sample` e `history`.
Nenhum sensor do usuário foi removido. A retenção original foi restaurada e os dois processos de smoke foram encerrados.
As capturas de regiões, atalhos e retenção foram recortadas para excluir a grade de câmeras.

Não foi encontrado defeito do aplicativo nos caminhos exercitados.
Build, testes e clippy não foram repetidos, pois esta rodada alterou somente o relatório e as evidências.
As aprovações da primeira seção pertencem à rodada anterior, não validam alterações concorrentes posteriores.
