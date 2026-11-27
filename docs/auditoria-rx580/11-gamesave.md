# Exclusão de save

O resultado de remove_file é descartado. Uma exclusão negada pelo sistema pode devolver S_OK com o blob ainda presente. Comparar arquivo ausente com arquivo bloqueado para exclusão. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/xgameruntime/gamesave.rs#L589)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
