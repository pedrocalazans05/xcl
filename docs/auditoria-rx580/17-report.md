# Relatório de falha pode bloquear

A coleta de estado para o relatório passa por locks além da espera inicial limitada. Se o detentor estiver parado, o relatório pode não terminar. Testar a coleta com o lock indisponível e exigir saída parcial em prazo limitado. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/report.rs#L191)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
