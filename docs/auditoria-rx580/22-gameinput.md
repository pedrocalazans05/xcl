# Leitura temporal do controle

GetTemporalReading descarta o timestamp e chama GetCurrentReading. Uma consulta ao passado pode receber o estado presente com sucesso. Verificar com dois estados distintos e uma consulta ao instante do primeiro. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/gameinput.rs#L800)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
