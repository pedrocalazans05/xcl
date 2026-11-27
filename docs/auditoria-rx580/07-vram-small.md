# Reutilização prematura após falha

Se a criação retoma um slot ainda em resguardo e depois falha, o rollback do último usuário entrega o bloco diretamente à lista livre. Outra chave pode obtê-lo antes do prazo do recurso anterior. Testar com relógio controlado e duas chaves. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/d3d12x/vram_small.rs#L154)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
