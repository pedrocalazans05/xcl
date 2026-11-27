# Save parcial

A atualização grava os blobs em sequência. Se a segunda gravação falhar, a primeira já pode estar publicada apesar do erro do lote. Verificar a versão dos dois blobs após a falha. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/xgameruntime/gamesave.rs#L569)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
