# Fork Mav-CG/gridcraft — `=RTD()`

Este fork acrescenta `=RTD()` (Profit `rtdtrading.rtdserver` e Cedro Fast Trade `srv.rtd`, só
Windows). O upstream (`storytold/gridcraft`) recusa RTD por decisão. Design:
`docs/superpowers/specs/2026-10-12-rtd-design.md`; plano: `docs/superpowers/plans/2026-10-12-rtd.md`.

## Arquivos só do fork (não conflitam)

- `crates/rtd-com/` — cliente COM (único crate com `unsafe`)
- `crates/calc/src/rtd.rs` — `=RTD()` no avaliador, `RtdSource`, `TopicTable`
- `crates/engine/src/rtd.rs` — `Session::rtd_refresh()`
- `apps/gridcraft/src/rtd.rs` — liga tudo no app desktop
- `RTD-FORK.md`, `docs/superpowers/`

## Encaixes no código do upstream (conferir em todo merge)

| Arquivo | Encaixe |
|---|---|
| `crates/calc/src/eval/call.rs` | `"RTD" => self.rtd(args),` em `Evaluator::call`, antes do ramo `"NOW" \| "TODAY" …` (até 03/11/2026 ficava em `eval.rs`; o upstream partiu o arquivo em módulos) |
| `crates/calc/src/lib.rs` | `pub mod rtd;` |
| `crates/engine/src/lib.rs` | `mod rtd;` |
| `apps/gridcraft/src/main.rs` | `mod rtd;` · `rtd::tick` em `logic` · `rtd::stop` em `on_exit` · `rtd::install` antes de `Session::new()` · `rtd::start` no criador do eframe |
| `apps/gridcraft/Cargo.toml` | `gridcraft-rtd-com` no alvo Windows |
| `xtask/src/layers.rs` | `("rtd-com", Class::Layer(0))` na `TABLE` |
| `crates/ui-egui/src/theme.rs` | em `apply`, `text_options` com `color_transfer_function = Off` e `subpixel_binning = false` nos dois temas — célula preto-no-branco nítida também no tema escuro (03/11/2026; não é RTD, é nitidez do texto) |
| `crates/ui-egui/src/grid/quadrant.rs` | `use super::leading;` e os dois `let y = …` (com e sem quebra) chamam `leading::text_y` — texto da célula na altura do GDI/Excel; módulo `grid/leading.rs` é só do fork, `mod leading;` em `grid.rs` (03/11/2026) |

Premissas do upstream de que o RTD depende (se mudarem, adaptar):

- `precedents()` em `eval/special.rs` marca como `dynamic` a função que não está no registro nem em `is_special`.
- `Calc::cells_changed(wb, &[])` reavalia todos os nós `dynamic`.
- `DocState { wb, saved, calc }` e `is_dirty()` = `!Arc::ptr_eq(&wb, &saved)`.

## Toolchain

O upstream usa recursos do Rust estável mais novo (ex.: `std::f64::consts::EULER_GAMMA`) apesar de
declarar `rust-version` mais antigo (1.95 em 03/11/2026). Nesta máquina a pasta tem `rustup override set 1.99.0` (o `stable`
global ficou intocado). Ao subir de versão: `rustup toolchain install <versão> --profile minimal -c
clippy -c rustfmt`, `rustup target add wasm32-unknown-unknown --toolchain <versão>` e `rustup override
set <versão>` dentro de `E:\gridcraft`.

## Falhas que já vêm do upstream (12/10/2026, base `b3acae6`)

O `cargo xtask ci` não fica 100% verde nesta máquina por causas que não são do RTD:

- ~~`gridcraft-engine`: os 2 testes de CSV (`1.24412E+11`)~~ — consertados no upstream; no merge
  de 03/11/2026 (upstream `92cdc2a9`, 0.16.0) o `engine` passou 809/809.
- `gridcraft-ui-egui` (não reconferido em 03/11): `i18n::tests::translations_are_unique_and_preserve_unknown_text` (falta a
  tradução francesa de "Different First Page"); o fork não toca o `ui-egui`.
- `parity --check`: acusa `docs/parity-checklist.md` "desatualizado" só por causa do
  `core.autocrlf=true` (CRLF); regenerado, o conteúdo é idêntico.

No merge, compare com esta lista: falha nova fora dela é que merece atenção.

## Merge mensal

    git fetch upstream
    git merge upstream/main
    # resolver conflitos nos encaixes acima
    cargo test -p gridcraft-rtd-com -p gridcraft-calc -p gridcraft-engine -p gridcraft
    cargo test -p gridcraft-rtd-com --test live -- --ignored --nocapture   # Profit + Fast Trade abertos
    cargo xtask ci
