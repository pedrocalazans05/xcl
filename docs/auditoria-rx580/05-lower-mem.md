# Endereço LDS

A máscara usada na tradução faz os offsets 0 e 32768 coincidirem. Em grupos que usam mais de 32 KiB de LDS, gravar valores distintos nesses offsets e comparar as duas leituras. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/gpu/dxil/lower_mem.rs#L2074)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
