# Identidade visual

## Cor primária

A cor primária do Intel SIAM é o verde dos tiles do ícone do app. Todo botão usa essa cor: botões comuns,
alternadores (como **Câmeras** e **Sensores** na barra superior), abas, escolhas lado a lado, listas suspensas e
campos numéricos arrastáveis.

| Estado | Fundo | Texto |
|---|---|---|
| Selecionado (ligado) | `#1AFA7F` (26, 250, 127) | preto |
| Não selecionado | `#006E37` (0, 110, 55) | branco |
| Não selecionado, com o mouse em cima ou pressionado | `#008C46` (0, 140, 70) | branco |

O verde não selecionado é escuro de propósito, para que ligado e desligado se distingam de relance. O texto branco
nos estados não selecionados também é o texto dos campos de edição, dos cabeçalhos recolhíveis e dos rótulos
clicáveis, que o egui desenha com as mesmas cores de widget sobre o painel escuro.

## Onde mora no código

- `src/ui.rs`, `apply_theme`: a única definição das cores (`PRIMARY`, `PRIMARY_OFF`, `PRIMARY_OFF_HOVER`).
  `ViewerApp::new` aplica o tema a todos os estilos do egui na criação do app.
- `src/ui.rs`, `toggle`: o botão que alterna ligado/desligado. `src/sensors_ui.rs`, `choice`: uma opção numa fileira
  de escolhas, construída sobre `toggle`.

Regras para código novo:

- Use `ui.button`, `egui::Button::new`, `toggle` ou `choice`. O tema dá a cor.
- Não passe `.fill(...)` nem `RichText::strong()` num botão. `strong()` pinta o texto com a cor de texto forte
  do tema, que é branca, e some no verde selecionado.
- Não use `selectable_label` nem `selectable_value` fora de listas suspensas: eles escondem a moldura quando
  desligados e não parecem botões.
- Entradas dentro de listas suspensas (`ComboBox`) continuam como `selectable_value`/`selectable_label`.
