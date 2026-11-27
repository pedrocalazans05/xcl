# Espera por PSO assíncrono

Se o trabalho termina em panic antes de publicar sua conclusão em uma build com unwind, a espera sem prazo pode ficar pendente. Simular falha do trabalho e verificar que todo consumidor recebe um resultado terminal. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/d3d12x/pso_async.rs#L87)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
