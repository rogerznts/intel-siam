# Intel SIAM

Visualizador de câmeras de gravadores Intelbras que também transforma regiões das imagens em sensores.

## Language

### Sensores

**Sensor**:
Uma região de uma fonte observada continuamente, que produz um valor de um único tipo.
_Avoid_: detector, regra, análise

**Tipo de sensor**:
O que o sensor produz: Estado, Objetos e pessoas ou Leitura. Escolhido na criação e nunca trocado.
_Avoid_: modo, categoria

**Estado**:
Tipo de sensor cujo valor é um entre até nove estados que o usuário ensina por exemplos (ex.: portão aberto,
fechado, entreaberto).
_Avoid_: classe, situação

**Objetos e pessoas**:
Tipo de sensor que diz, para cada objeto, pessoa ou rótulo próprio, se está presente e quantos há.
_Avoid_: detecção, YOLO

**Leitura**:
Tipo de sensor cujo valor é um número lido de um display ou contador (medidor de água, gás, energia; minutos
restantes).
_Avoid_: OCR, medição

**Região**:
A parte da imagem da fonte que o sensor observa; o resto da imagem é ignorado.
_Avoid_: área, zona, ROI

**Rótulo próprio**:
Um nome que o usuário dá a um objeto específico ensinando caixas dele (ex.: "nosso carro"), distinto da classe
genérica.
_Avoid_: tag, classe customizada

**Pessoa**:
Alguém que o usuário nomeou ensinando o rosto; vale para todas as fontes. Quem tem rosto visível e não é nenhuma
pessoa nomeada é uma **pessoa desconhecida**.
_Avoid_: usuário, contato, face

**Amostra**:
Uma imagem da região rotulada pelo usuário, usada para ensinar um sensor.
_Avoid_: exemplo de treino, dataset

**Fila de revisão**:
Imagens em que o sensor ficou inseguro, guardadas para o usuário confirmar ou corrigir; a resposta vira amostra.
_Avoid_: inbox, pendências

### Câmeras

**Fonte**:
De onde um sensor tira as imagens: um canal de um gravador ou uma URL de câmera avulsa (RTSP ou snapshot HTTP).
_Avoid_: câmera (quando se fala do sensor), entrada
