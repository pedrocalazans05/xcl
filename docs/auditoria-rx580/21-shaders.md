# Preparação subestima falhas

Um worker que não inicia gera aviso, mas o total de falhas conta apenas processos iniciados. Com dois workers solicitados e nenhuma inicialização bem-sucedida, o resumo pode indicar zero falhas. Simular erro de spawn antes de consolidar o resultado. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/shaders.rs#L1849)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
