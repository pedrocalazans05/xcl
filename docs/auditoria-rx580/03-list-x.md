# Cópia dentro do mesmo buffer

Se a alocação temporária falha, o caminho alternativo usa o mesmo recurso como origem e destino em estado de destino. Forçar a indisponibilidade do temporário e verificar os bytes e as mensagens da camada de depuração. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/d3d12x/list_x.rs#L2163)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
