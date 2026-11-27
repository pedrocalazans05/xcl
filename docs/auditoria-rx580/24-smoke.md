# Smoke conta mensagens como crashes

O contador soma as mensagens de exceção e de término. Uma amostra com uma linha de cada, para o mesmo processo, devolveu 2. Testar um único encerramento acompanhado das duas mensagens. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/scripts/smoke.ps1#L84)

A auditoria anterior reproduziu o comportamento em uma verificação isolada no Windows.
