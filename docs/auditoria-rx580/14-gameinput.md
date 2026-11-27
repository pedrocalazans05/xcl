# Limiar analógico de GameInput

O registro do callback ignora o limiar solicitado. Comparar notificações com duas variações de eixo abaixo e acima do limiar. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/gameinput.rs#L913)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
