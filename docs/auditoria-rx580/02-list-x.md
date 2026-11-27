# Cópia UMA descartada

Quando uma das pontas não é resolvida na janela UMA, CopyMemoryToMemoryX retorna sem copiar. Os dois logs registram 0x4000 bytes descartados, aos 408,648 s na última sessão. Verificar o conteúdo do destino após uma cópia entre esses tipos de endereço. [Código na revisão auditada](https://github.com/pedrocalazans05/xcl/blob/edd2bdc4a12bf43090d5b9ded829eaa7fcd34209/crates/xcl/src/hle/d3d12x/list_x.rs#L2150)

O comportamento aparece nos dois logs da RX 580 2048SP. A relação com a perda do dispositivo segue em investigação.
