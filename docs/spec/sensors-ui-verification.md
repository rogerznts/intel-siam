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

## Limites do smoke

A região permaneceu como o quadro inteiro. Duas tentativas de arrastar um retângulo pelo `orca computer`
retornaram `synthetic_input` sem mudança visual. Uma tentativa de inserir um ponto do polígono retornou
`window_not_focused`, com a mensagem `current: no focused window`. A tentativa de restaurar a janela e a
ativação por AppleScript não tornaram a interação por coordenadas verificável. Portanto, o desenho de
retângulo, o desenho de polígono e os atalhos de teclado não foram comprovados nesta sessão.

A seleção de modo Polígono funcionou pela acessibilidade. A construção e a validação das regiões têm
cobertura nos testes de domínio, mas esses testes não substituem o gesto na janela.

O smoke comprovou o valor no painel. A fonte URL avulsa não pertence a um tile, por definição.
O selo de sensor sobre um tile de DVR, a correção de uma revisão pendente e a edição de retenção
não foram exercitados manualmente. A renderização do selo usa a mesma réplica e o mesmo rótulo do painel.
