# Rotação perde o relatório anterior

O launcher ignora o resultado da renomeação e em seguida tenta apagar o original. Se só o destino da rotação estiver bloqueado, o único relatório daquela sessão pode sumir. Testar preservação do original nesse cenário. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/launcher.rs#L361)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
