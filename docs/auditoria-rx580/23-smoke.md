# Smoke altera a sessão PowerShell

O script deixa XCL_NETWORK em 0 e remove XCL_RUN_SECONDS e XCL_ACTIVITY mesmo que já estivessem definidos pelo usuário. Comparar o ambiente antes e depois da execução na mesma sessão, inclusive em saída por erro. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/scripts/smoke.ps1#L58)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
