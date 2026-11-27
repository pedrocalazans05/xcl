# Áudio Opus

Um pacote integralmente consumido pelo pre-skip pode produzir zero amostras válidas. O caminho de reprodução pode convertê-lo em silêncio com duração positiva e deslocar o áudio seguinte. Verificar a contagem de frames submetidos. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/xaudio2.rs#L1045)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
