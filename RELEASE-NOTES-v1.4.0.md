# Arch R Flasher 1.4.0: Release Notes

> Novos consoles: AISLPC RG43V PRO (1024x768 e 1024x600) e RG52 MINI (RK3562).
> Sai junto com a Arch R v3.1.1, a primeira com a imagem AISLPC.

## Destaques

- **Seletor de console em dois grupos**: "- Game Console" (R36S Original, R36S Clone, R36S Soysauce, sem mudança) e "- AISLPC" (RG43V PRO 1024x768, RG43V PRO 1024x600, RG52 MINI).
- **AISLPC sem overlay**: os três usam a mesma imagem (`ArchR-RK3562.aarch64-<data>-aislpc.img.gz`). Depois de gravar, o flasher copia `extlinux/extlinux.conf.<placa>` por cima de `extlinux/extlinux.conf` na partição de boot, e é só isso. O passo Overlay some do assistente (Console → Imagem → SD → Gravar).
- **Só baixar a imagem**: novo botão "Download image only (don't flash)..." no passo Imagem. Baixa a imagem do console escolhido e salva onde o usuário quiser, com o `.sha256` ao lado. Usa o mesmo cache do "Download latest version".

---

## Por categoria

### Imagem certa para cada console

- Game Console baixa a imagem `-original` (Original e Soysauce) ou `-clone`; AISLPC baixa a `-aislpc`. A escolha mora em `console.rs` (`image_family`, `pick_image_asset`) e não depende da ordem dos assets na release.
- Enquanto a última release não tiver imagem AISLPC, o passo Imagem diz "Not in the latest release" (não "Offline"), e o download explica que falta a imagem e aponta para "Select file" com uma imagem local.
- Arquivo local de outra família é recusado como antes: imagem R36S num console AISLPC e imagem AISLPC num Game Console.

### Gravação AISLPC

- Linux e macOS: a troca do `extlinux.conf` acontece dentro da cópia privada da imagem, antes de gravar, como já era com o Soysauce. A leitura de volta confere o `extlinux.conf` antes de ir para o cartão, e a verificação cobre esses bytes.
- Windows: o script de configuração pós-gravação recebe o arquivo da placa por variável de ambiente e faz a mesma cópia.
- Para AISLPC não se grava o arquivo `variant`, não se cria `overlays/` nem `extlinux.conf.bak`: só o `extlinux.conf` muda.
- Imagem sem o `extlinux.conf.<placa>` (ou seja, não é a imagem AISLPC) para a gravação com erro claro, em vez de gravar um cartão que não liga.
- O backend recusa overlay para AISLPC e console desconhecido.

### Internacionalização

- 12 chaves novas nos 5 idiomas (en, pt-BR, es, zh, ru).

### Testes

- Escolha do asset por grupo de console, troca do `extlinux.conf` numa FAT de teste (cada placa, só o `extlinux.conf` muda), recusa de imagem sem a config da placa, preparo Linux sem `variant`.
- Teste manual (`--ignored`) contra a imagem AISLPC real: as três placas apontam para o DTB certo e o `fsck.fat` passa.
