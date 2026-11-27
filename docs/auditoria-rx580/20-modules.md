# Índice de módulos esgotável

O índice de intervalos só cresce. Remover um módulo limpa a base, mas não libera o índice para outro. Após 512 registros acumulados, um módulo ainda pode aparecer na lista principal e ficar invisível à consulta por endereço. Testar carga e remoção repetidas com um único módulo ativo. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/modules.rs#L47)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
