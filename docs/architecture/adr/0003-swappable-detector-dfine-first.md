# Detector de objetos trocável, começando pelo D-FINE e não pelo YOLO

O detector de objetos fica atrás de um contrato por modelo ONNX (entrada, rótulos, saída de caixas), e o primeiro
modelo é o D-FINE, apesar de o roadmap citar YOLO. O D-FINE é Apache-2.0 e é o que o VisionState usa, então os
limiares e filtros copiados da origem valem para ele; o YOLO da Ultralytics é AGPL-3.0 em código e pesos, o que
acrescenta obrigações ao distribuir. O contrato deixa plugar outro detector depois sem mexer nos sensores.
