# Sensores rodam num processo de visão supervisionado, fora da grade

Os sensores rodam num processo filho em Rust com ONNX Runtime (`ort`), supervisionado pelo app como o túnel e os
decoders, e não dentro do processo da UI nem em Python. Esse processo abre os próprios decoders, um por fonte com
sensor, independentes da grade: o sensor continua valendo quando a câmera sai da grade, e o mesmo processo é a base
do modo servidor sem interface do roadmap. Cada sensor escolhe o stream: Sub por padrão, Main para Pessoas e
Leituras, que precisam de pixels para rostos e dígitos.
O app continua dono do túnel P2P e passa ao processo de visão a URL RTSP local de cada fonte de gravador; fontes
por URL vão direto.

## Considered Options

- **Inferência dentro do processo da UI** — uma falha ou pico de memória da IA derrubaria a grade, e o runtime ONNX
  pesaria no binário da UI contra o `NFR-002` (binário leve, poucas dependências de runtime).
- **Filho em Python reaproveitando o VisionState** — porte mais rápido, mas obriga a distribuir Python, NumPy e os
  modelos nos três sistemas.
- **Sensor lendo o frame do tile** — o sensor pararia quando a câmera saísse da grade, e o `FrameSlot` tem um só
  leitor, a UI.

## Consequences

- Câmeras com sensor custam um decoder a mais quando também estão na grade.
- O uso de CPU com decoders em Main precisa ser medido antes de decidir o `NFR-003` (decode por hardware).
