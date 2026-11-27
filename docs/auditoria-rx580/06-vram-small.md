# Retenção de heaps pequenos

O alocador reutiliza blocos, mas mantém os heaps de 16 MiB em seu vetor mesmo quando todos os blocos ficam livres. Medir os bytes retidos depois de esvaziar a classe com XCL_VRAM_KEEP_SPARE_POOLS=0. O impacto sobre o excesso de VRAM das sessões permanece em aberto. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/d3d12x/vram_small.rs#L137)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
