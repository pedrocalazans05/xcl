# Arquivo oculto na camada virtual

O marcador que oculta o arquivo original pode ser removido antes de CreateFile ter sucesso. Se a criação falhar, verificar se o original reaparece na próxima abertura. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/vfs.rs#L512)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
