# Arch R Flasher 1.4.1: Release Notes

> Correção no Windows: "Verification failed" em cartões bons (issues #15 e #16).

## Correção

- **Windows não mexe mais no cartão durante a gravação.** A tabela de partições da imagem chegava ao cartão logo no começo; o Windows via as partições novas, montava sozinho e escrevia nelas (pasta "System Volume Information", marca da FAT), e a verificação achava o cartão diferente da imagem. Agora:
  - a montagem automática do Windows fica desligada durante a gravação e a verificação, e volta como estava no fim (como o Rufus faz);
  - o primeiro pedaço da imagem (com a tabela de partições) é gravado por último; até lá o cartão não tem partições para o Windows achar.
- **Log mais útil quando a verificação falha:** diz quantos pedaços de 4 MiB ficaram diferentes, em que ponto (MiB) e quantos voltaram zerados. Poucos pontos espalhados = algo escrevendo no cartão; tudo a partir de um ponto = cartão falso (menor do que diz) ou com defeito.

Linux e macOS não mudam.
