# Fila D3D12

A mesma gravação volta à mesma fila antes da conclusão anterior. O aviso apareceu nas duas sessões, aos 398,038 s na última. Correlacionar a identidade da gravação com a fence concluída antes de reenviar. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/d3d12x/real.rs#L8327)

O comportamento aparece nos dois logs da RX 580 2048SP. A relação com a perda do dispositivo segue em investigação.
