# DirectStorage

Uma falha ao criar a thread de submissão pode descartar o lote retirado da fila e deixar seu status pendente. Injetar falha de criação e verificar a conclusão de cada notificação. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/dstorage.rs#L1114)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
