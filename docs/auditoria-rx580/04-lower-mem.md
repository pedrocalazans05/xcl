# Permutação de lanes

A tradução de DS_BPERMUTE_B32 permite cruzar metades de 32 lanes no caminho Wave64. Para o modo RDNA2 que restringe a operação à metade de origem, o resultado pode vir da lane errada. Comparar lanes 0 e 32 nos emissores HLSL e DXIL e preservar os modos com outra semântica. Há testes locais que esperam a leitura cruzada. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/gpu/dxil/lower_mem.rs#L2110)

A auditoria identificou esta condição por inspeção do código em edd2bdc4. A reprodução em execução permanece pendente.
