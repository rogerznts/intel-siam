# Intel SIAM

@/Users/admin/.claude/pstack-models.md

## Fluxo de trabalho: pstack, não anvil

Este projeto usa o **pstack** (plugin `pstack@pstack-claude`, declarado em `.claude/settings.json`). As diretrizes
do anvil no `~/.claude/CLAUDE.md` global **não valem aqui**, exceto a seção "Idioma":

- Não use as skills `anvil-*`, inclusive `anvil-next`. A "cauda de roteamento" não dispara neste projeto.
- Não exija `.claude/rules/` e não sugira `/anvil-boot`. A ausência do diretório é intencional.
- Trabalho que toca mais de um arquivo, envolve decisão de arquitetura ou é bug de causa desconhecida passa pelo
  `pstack:poteto-mode`. Com intenção já específica, entre direto na skill: `pstack:architect`, `pstack:how`,
  `pstack:why`, `pstack:tdd`, `pstack:arena`, `pstack:interrogate`.
- Subagentes de código são `pstack:poteto-agent`, não `general-purpose`.
- Grill (`anvil-grill`, `anvil-grilling` ou equivalente): cada rodada de perguntas vai pelo tool `ask` (no Claude Code,
  `AskUserQuestion`), uma pergunta por decisão, com as alternativas como opções, a recomendada marcada e o
  trade-off na descrição de cada opção; `multi` quando as opções se combinam. Contexto que o usuário precisa para
  decidir vem em texto antes da chamada.

O idioma continua o mesmo: chat e documentação em pt-BR; código, identificadores, caminhos e mensagens de commit
em inglês.

## Identidade visual

A cor primária é o verde do botão **Câmeras**, aplicada a todos os botões do app. Valores, estados e regras para
código novo em [docs/design/visual.md](docs/design/visual.md); leia antes de mexer em UI.

## Engenharia reversa

- Alvos em `references/simplay-1.4.1/` (gitignorado; material da Intelbras, nunca commitar): `libt2u.so` (túnel P2P,
  Mach-O x86_64) e as DLLs .NET do SIMPlay 1.4.1, incluindo o wrapper `VVEye.Libt2u.dll`.
- REA `rea-agents@6.4.0` (devDependency) com Ghidra do Homebrew (`/opt/homebrew/opt/ghidra/libexec`), MCP `rea` em
  `.mcp.json`. Antes de usar, leia `.claude/skills/reverse-engineer-anything/SKILL.md`.
- DLLs .NET: o REA 6.4.0 rejeita (máquina PE `0xC020`, AMD64 com marcador de macOS). Use o ILSpy local
  (`dotnet-tools.json`): `DOTNET_ROLL_FORWARD=Major dotnet ilspycmd -p -o references/decompiled/<Asm> -r
  references/simplay-1.4.1 references/simplay-1.4.1/<Asm>.dll`. Saída em `references/decompiled/` (gitignorado).
- CLI sem MCP: `GHIDRA_INSTALL_DIR=/opt/homebrew/opt/ghidra/libexec npx --no-install rea decompile <arquivo> <função> --provider ghidra`.
- Toda descoberta sobre o binário vai para `docs/protocol/` (um arquivo por componente, ex. `libt2u.md`), não fica
  só no chat nem na trilha do pstack. Cada afirmação leva tag: `[O]` observado (com função e endereço ou arquivo
  .NET e membro), `[I]` inferido (com o raciocínio), `[H]` hipótese (precisa de confirmação). Promover `[H]` → `[O]`
  exige o endereço que prova. Busca negativa diz o que foi procurado e onde, nunca "não existe".
- Sessão REA aberta mantém uma JVM de ~2 GB. Feche sempre com `close_binary`. `tools/re/reap.sh` mata JVMs e
  `decompile` órfãos; roda pelos hooks em `.claude/settings.json`. À mão: `tools/re/reap.sh --dry-run`.
