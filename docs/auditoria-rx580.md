# Auditoria do XCL na RX 580 2048SP com 16 GB de RAM

Revisão `edd2bdc4a12bf43090d5b9ded829eaa7fcd34209` da branch `rx580-mesh-vs-fallback`. O inventário reúne os achados identificados nos dois logs e nos caminhos de código examinados. Cada item indica a condição e uma verificação de regressão. A cobertura da revisão é parcial.

## O que as duas execuções demonstram

A primeira sessão registrou 33.291 apresentações, perda do dispositivo com DXGI_ERROR_DEVICE_HUNG e page fault de GPU em 0x0. Na seguinte houve 37.518 apresentações e zero pipelines com falha no relatório. O shader de compute `fe4357c214f998d2` aparece nas duas. A cena alcançada e a causa da perda de GPU continuam indeterminadas. Na última saída, 0xc000013a precedeu em 82 ms uma leitura nula (0xc0000005). Comparar saída normal e interrupção pelo console na mesma cena.

A última amostra registrou uso local de 8.877 MB para orçamento DXGI de 7.241 MB e commit do processo de 21.642 MB. Os valores medem grandezas diferentes. A sessão não registrou E_OUTOFMEMORY. Medir a duração do excesso de orçamento antes de atribuir o encerramento à memória.

## Inventário e verificações propostas

Itens 1 e 2 apareceram nas duas sessões. Os itens 19 e 24 tiveram verificações isoladas com o leitor XML e a expressão do script, respectivamente. Os demais resultam da inspeção de código. As verificações propostas descrevem testes futuros. A ordem segue os subsistemas.

1. **Fila D3D12.** A mesma gravação volta à mesma fila antes da conclusão anterior. O aviso apareceu nas duas sessões, aos 398,038 s na última. Correlacionar a identidade da gravação com a fence concluída antes de reenviar. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/d3d12x/real.rs#L8327)

2. **Cópia UMA descartada.** Quando uma das pontas não é resolvida na janela UMA, CopyMemoryToMemoryX retorna sem copiar. Os dois logs registram 0x4000 bytes descartados, aos 408,648 s na última sessão. Verificar o conteúdo do destino após uma cópia entre esses tipos de endereço. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/d3d12x/list_x.rs#L2150)

3. **Cópia dentro do mesmo buffer.** Se a alocação temporária falha, o caminho alternativo usa o mesmo recurso como origem e destino em estado de destino. Forçar a indisponibilidade do temporário e verificar os bytes e as mensagens da camada de depuração. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/d3d12x/list_x.rs#L2163)

4. **Permutação de lanes.** A tradução de DS_BPERMUTE_B32 permite cruzar metades de 32 lanes no caminho Wave64. Para o modo RDNA2 que restringe a operação à metade de origem, o resultado pode vir da lane errada. Comparar lanes 0 e 32 nos emissores HLSL e DXIL e preservar os modos com outra semântica. Há testes locais que esperam a leitura cruzada. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/gpu/dxil/lower_mem.rs#L2110)

5. **Endereço LDS.** A máscara usada na tradução faz os offsets 0 e 32768 coincidirem. Em grupos que usam mais de 32 KiB de LDS, gravar valores distintos nesses offsets e comparar as duas leituras. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/gpu/dxil/lower_mem.rs#L2074)

6. **Retenção de heaps pequenos.** O alocador reutiliza blocos, mas mantém os heaps de 16 MiB em seu vetor mesmo quando todos os blocos ficam livres. Medir os bytes retidos depois de esvaziar a classe com XCL_VRAM_KEEP_SPARE_POOLS=0. O impacto sobre o excesso de VRAM das sessões permanece em aberto. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/d3d12x/vram_small.rs#L137)

7. **Reutilização prematura após falha.** Se a criação retoma um slot ainda em resguardo e depois falha, o rollback do último usuário entrega o bloco diretamente à lista livre. Outra chave pode obtê-lo antes do prazo do recurso anterior. Testar com relógio controlado e duas chaves. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/d3d12x/vram_small.rs#L154)

8. **Espera por PSO assíncrono.** Se o trabalho termina em panic antes de publicar sua conclusão em uma build com unwind, a espera sem prazo pode ficar pendente. Simular falha do trabalho e verificar que todo consumidor recebe um resultado terminal. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/d3d12x/pso_async.rs#L87)

9. **DirectStorage.** Uma falha ao criar a thread de submissão pode descartar o lote retirado da fila e deixar seu status pendente. Injetar falha de criação e verificar a conclusão de cada notificação. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/dstorage.rs#L1114)

10. **Save parcial.** A atualização grava os blobs em sequência. Se a segunda gravação falhar, a primeira já pode estar publicada apesar do erro do lote. Verificar a versão dos dois blobs após a falha. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/xgameruntime/gamesave.rs#L569)

11. **Exclusão de save.** O resultado de remove_file é descartado. Uma exclusão negada pelo sistema pode devolver S_OK com o blob ainda presente. Comparar arquivo ausente com arquivo bloqueado para exclusão. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/xgameruntime/gamesave.rs#L589)

12. **Arquivo oculto na camada virtual.** O marcador que oculta o arquivo original pode ser removido antes de CreateFile ter sucesso. Se a criação falhar, verificar se o original reaparece na próxima abertura. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/vfs.rs#L512)

13. **Áudio Opus.** Um pacote integralmente consumido pelo pre-skip pode produzir zero amostras válidas. O caminho de reprodução pode convertê-lo em silêncio com duração positiva e deslocar o áudio seguinte. Verificar a contagem de frames submetidos. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/xaudio2.rs#L1045)

14. **Limiar analógico de GameInput.** O registro do callback ignora o limiar solicitado. Comparar notificações com duas variações de eixo abaixo e acima do limiar. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/gameinput.rs#L913)

15. **Pilha de outra thread.** O coletor retoma a thread antes de terminar de percorrer a pilha capturada. O conteúdo pode mudar sob a leitura e prejudicar o diagnóstico. Comparar com uma cópia da pilha feita enquanto a thread está suspensa. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/eh.rs#L679)

16. **Falsos quadros na pilha.** A varredura de ponteiros aceita endereços de seções de dados do módulo como candidatos. Na análise anterior apareceram endereços em .rdata e .pdata. Identificar separadamente candidatos de varredura e frames desenrolados. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/eh.rs#L565)

17. **Relatório de falha pode bloquear.** A coleta de estado para o relatório passa por locks além da espera inicial limitada. Se o detentor estiver parado, o relatório pode não terminar. Testar a coleta com o lock indisponível e exigir saída parcial em prazo limitado. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/report.rs#L191)

18. **Rotação perde o relatório anterior.** O launcher ignora o resultado da renomeação e em seguida tenta apagar o original. Se só o destino da rotação estiver bloqueado, o único relatório daquela sessão pode sumir. Testar preservação do original nesse cenário. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/launcher.rs#L361)

19. **XML do manifesto.** O leitor exige aspas duplas e o sinal de igual sem espaços. Também corta a tag no primeiro > dentro de um atributo. Com a função Rust do checkout, o controle devolveu a.exe e as três variantes válidas devolveram lista vazia. Isso afeta a descoberta do executável pelo manifesto. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/launcher.rs#L20)

20. **Índice de módulos esgotável.** O índice de intervalos só cresce. Remover um módulo limpa a base, mas não libera o índice para outro. Após 512 registros acumulados, um módulo ainda pode aparecer na lista principal e ficar invisível à consulta por endereço. Testar carga e remoção repetidas com um único módulo ativo. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/modules.rs#L47)

21. **Preparação subestima falhas.** Um worker que não inicia gera aviso, mas o total de falhas conta apenas processos iniciados. Com dois workers solicitados e nenhuma inicialização bem-sucedida, o resumo pode indicar zero falhas. Simular erro de spawn antes de consolidar o resultado. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/shaders.rs#L1849)

22. **Leitura temporal do controle.** GetTemporalReading descarta o timestamp e chama GetCurrentReading. Uma consulta ao passado pode receber o estado presente com sucesso. Verificar com dois estados distintos e uma consulta ao instante do primeiro. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/gameinput.rs#L800)

23. **Smoke altera a sessão PowerShell.** O script deixa XCL_NETWORK em 0 e remove XCL_RUN_SECONDS e XCL_ACTIVITY mesmo que já estivessem definidos pelo usuário. Comparar o ambiente antes e depois da execução na mesma sessão, inclusive em saída por erro. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/scripts/smoke.ps1#L58)

24. **Smoke conta mensagens como crashes.** O contador soma as mensagens de exceção e de término. Uma amostra com uma linha de cada, para o mesmo processo, devolveu 2. Testar um único encerramento acompanhado das duas mensagens. [Código](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/scripts/smoke.ps1#L84)

## Ordem de investigação para a RX 580

1. Repetir a mesma transição de cena, rastrear gravação, fila e fence, e validar o destino da cópia omitida. São os dois sintomas que se repetiram nos logs.
2. Investigar a leitura nula de saída com os símbolos do binário correspondente. Manter a sequência de encerramento separada do episódio anterior de perda de GPU.
3. Medir a residência e a reutilização dos heaps por classe. Testar LDS e permutação com entradas conhecidas antes de alterar a tradução globalmente. A discussão de Wave64 também aparece no [PR upstream 1022](https://github.com/xcl-dev/xcl/pull/1022).
4. Tratar atomicidade dos saves como prioridade independente pelo risco de persistir uma atualização incompleta. Validar os demais caminhos com os casos descritos acima.

Filtragem adicional de passes de ray tracing, limitação histórica de PSOs, proteção no compilador AMD, pré-aquecimento e LTO permanecem propostas. Os dois logs não demonstram que essas mudanças resolveriam o travamento.

