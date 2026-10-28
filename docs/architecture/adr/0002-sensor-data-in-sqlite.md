# Dados dos sensores em SQLite, com imagens em disco

Sensores, amostras, caixas ensinadas, rostos, histórico e estatísticas diárias ficam num banco SQLite (`rusqlite`
com SQLite embutido) que pertence ao processo de visão; as imagens ficam em arquivos ao lado, referenciadas pelo
banco. O resto do app continua no storage do eframe. Histórico com limpeza por idade e tamanho, o limite do contador
aprendido de estatísticas diárias e as consultas do futuro servidor MCP pedem um banco; arquivos JSON exigiriam
reescrever e varrer arquivos à mão.
