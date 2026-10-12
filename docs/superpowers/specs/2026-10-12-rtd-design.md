# `=RTD()` nativo no fork Mav-CG/gridcraft — design

> **Data:** 2026-10-12 · **Estado:** aprovado em conversa, aguardando revisão do documento
> **Fork:** `Mav-CG/gridcraft` (origin) ← `storytold/gridcraft` (upstream), base `b3acae6` (0.6.0)

## 1. Objetivo

A função `=RTD(progID; servidor; tópico1; …)` funciona no GridCraft como no Excel, lendo dados ao
vivo do **Profit** (`rtdtrading.rtdserver`) e do **Cedro Fast Trade** (`srv.rtd`), em qualquer
planilha. O upstream recusou o RTD por decisão ("Native DLL/COM calls; not portable or safe",
`docs/function-parity.md`), então ele vive só neste fork, com merge do upstream ~1×/mês.

### Critérios de pronto

1. Testes novos de `calc` e `engine` passam; `cargo xtask ci` continua verde.
2. `E:\Planilha Fluxo\Dashboard.xlsm` abre no fork e as células RTD mostram os últimos valores dos
   dois servidores.
3. **Prova ao vivo (próximo pregão):** os valores andam sozinhos; o Ctrl+Z não cresce; o tempo de
   cada atualização (`GRIDCRAFT_PROFILE`) fica bem abaixo de 200 ms com o `Dashboard.xlsm`
   (~6 mil tópicos únicos). Sem este item o trabalho é "pronto, falta a prova ao vivo".

## 2. Fatos medidos que o design usa (12/10/2026)

| Fato | Origem |
|---|---|
| `srv.rtd` = DLL .NET **in-process** (`C:\Cedro Technologies\Fast Trade x64\bin\srv.DLL`, `mscoree.dll`, ThreadingModel Both) | registro do Windows |
| `rtdtrading.rtdserver` = **out-of-process** (`LocalServer32 = …\Profit_2026\profitchart.exe`) | registro do Windows |
| Typelib de `IRtdServer`/`IRTDUpdateEvent` registrada pelo **Profit**, não pelo Office | registro (`TypeLib {EFCFBDCA-…}` → `profitchart.exe`) |
| Os dois servidores respondem a cliente que não é o Excel: `ServerStart`=1, `ConnectData` devolve o valor inicial, `UpdateNotify`→`RefreshData` traz os deltas | sonda Python (comtypes) |
| Fast Trade manda `UpdateNotify` ~2×/s mesmo sem mudança (RefreshData com 0 tópicos) | sonda |
| Profit fechado: a ativação COM **abre o Profit**; `ConnectData` durante a carga dá *Access violation* dentro do `profitchart.exe`; depois de carregado responde normal | sonda |
| `Dashboard.xlsm`: 6.357 fórmulas RTD, 5.967 tópicos únicos, 843 com argumento vindo de célula; `DashBoard_Acoes.xlsm`: 10.785 / 5.789 | análise do XML |
| Formato no arquivo: `RTD("srv.rtd","","SQT","PETR4","LAST")`, `RTD("rtdtrading.rtdserver",,"WINZ25_F_0","ULT")` | análise do XML |

## 3. Arquitetura

Princípio: **mínimo de linhas dentro dos arquivos do upstream**; o resto em arquivos/crates novos.

```
 Profit (out-of-proc)         Fast Trade (in-proc .NET)
          └──────────── COM ───────────┘
 crates/rtd-com        (NOVO, só Windows, ÚNICO lugar com unsafe)
   thread STA própria: ServerStart · ConnectData · RefreshData · DisconnectData
   API segura: start(waker) → RtdClient { connect, disconnect, drain_updates }
          │
 apps/gridcraft/src/rtd.rs   (NOVO, #[cfg(windows)], código seguro)
   implementa calc::rtd::RtdSource sobre o RtdClient; throttle 200 ms; varredura
          │
 crates/calc/src/rtd.rs      (NOVO, portável, wasm-safe)
   trait RtdSource + fonte global (instalada 1×) + tabela tópico→valor
          │                              sem fonte instalada ⇒ #N/A
 crates/calc/src/eval.rs     ramo "RTD" no avaliador (como o ramo NOW)
 crates/engine/src/rtd.rs    (NOVO) Session::rtd_refresh(): recalcula sem histórico
```

### 3.1 Pontos de encaixe no código do upstream

| Arquivo do upstream | Mudança |
|---|---|
| `crates/calc/src/eval.rs` | 1 ramo `"RTD" =>` no `match` de funções especiais (perto de `"NOW" \| "TODAY" …`) |
| `crates/calc/src/lib.rs` | `pub mod rtd;` |
| `crates/engine/src/lib.rs` | `mod rtd;` |
| `apps/gridcraft/src/main.rs` | `#[cfg(windows)] mod rtd;`, instalar a fonte antes de abrir documentos, 1 chamada por quadro em `App::logic` |
| `apps/gridcraft/Cargo.toml` | `gridcraft-rtd-com` no alvo Windows |
| `xtask/src/layers.rs` | 1 linha na `TABLE`: `("rtd-com", Class::Layer(0))` |

Essa lista vai também para `RTD-FORK.md` (raiz), o roteiro do merge mensal.

### 3.2 Decisões

- **Fonte RTD global** (uma por processo, `calc::rtd::install`), em vez de atravessar
  `Session`/`DocState`/`Calc`/`PassHost`: poupa ~10 encaixes no upstream.
- **Dependências sem mudança:** o RTD **não** entra no registro `FnSpec` nem em `is_special`, então
  `precedents()` (`eval.rs`, regra "função desconhecida ⇒ dynamic") já o trata como sempre-sujo,
  como um volátil. Toda passada de recálculo reavalia todas as células RTD.
- **`unsafe` isolado num crate próprio** (`crates/rtd-com`, sem `[lints] workspace = true`), com
  API segura, igual ao precedente do upstream com `fmv-macos-events`. O resto do workspace continua
  com `unsafe_code = "forbid"`.
- **Crate `windows` = 0.62.2**, a versão que já está no `Cargo.lock` (sem segunda cópia).
  `IRtdServer`/`IRTDUpdateEvent` não existem no crate: são declaradas com `#[interface]` (IIDs da
  sonda Python, `backend/tools/book_reader/rtd_com.py` do dash-app) e o callback com `#[implement]`.
- **A atualização não é um comando** (desvio consciente do "Everything is a command"): comando entra
  no `journal` e o tick o faria crescer sem fim. É um método `Session::rtd_refresh()`.
- **Lista de servidores permitidos, fixa no código:** `srv.rtd`, `rtdtrading.rtdserver`
  (comparação sem diferenciar maiúsculas). Outro progID ⇒ `#N/A`, sem tocar no COM.

## 4. Comportamento

### 4.1 Avaliação de `=RTD(progID; servidor; t1; …; tn)`

- Aridade: mínimo 3 argumentos (como no Excel); erro de aridade ⇒ `#VALUE!`.
- Cada argumento é avaliado e convertido para texto (número `27` ⇒ `"27"`; ausente ⇒ `""`). Se algum
  avaliar para erro, a célula devolve esse erro.
- Chave do tópico = `(progID minúsculo, servidor, [t1..tn])`. Fórmulas iguais compartilham o tópico.
- Tópico conhecido ⇒ o último valor. Novo ⇒ registra o pedido e devolve `#N/A` até o valor chegar.
- Toda consulta marca o tópico como **visto na passada atual** (usado na varredura, §4.3).

### 4.2 Thread COM (`crates/rtd-com`)

- Uma thread, `CoInitializeEx(COINIT_APARTMENTTHREADED)`, com laço de mensagens próprio
  (`MsgWaitForMultipleObjects`/`PeekMessage`, tempo-limite ~50 ms). Isso é obrigatório para o
  `UpdateNotify` chegar.
- Um `IRtdServer` por progID, criado sob demanda (`CLSIDFromProgID` + `CoCreateInstance`,
  `CLSCTX_INPROC_SERVER | CLSCTX_LOCAL_SERVER`), `ServerStart(callback)` tem que devolver 1.
- `ConnectData(id, strings, true)` ⇒ o valor inicial já entra na tabela.
- `UpdateNotify` só marca a flag; o laço chama `RefreshData`, aplica os deltas e acorda a UI
  (`waker` = `egui::Context::request_repaint`). `RefreshData` com 0 tópicos ⇒ nada muda, nada acorda.
- Conversão de VARIANT: `VT_R8`/`VT_R4`/`VT_I*`/`VT_UI*`/`VT_CY`/`VT_DATE`⇒número; `VT_BOOL`⇒lógico;
  `VT_BSTR`⇒texto; `VT_EMPTY` ou `""`⇒vazio; `VT_ERROR` ou desconhecido⇒`#N/A`.
- **Falhas:** erro em `CoCreateInstance`/`ServerStart`/`ConnectData` (inclui o Profit ainda
  carregando) ⇒ o tópico fica `#N/A` e é tentado de novo a cada **5 s**. Servidor que cai
  (chamada falha com `RPC_E_DISCONNECTED`/`RPC_S_SERVER_UNAVAILABLE`) ⇒ descarta o servidor, as
  células mantêm o último valor, reconecta a cada 5 s e religa todos os tópicos vivos.
- Na saída do app: `DisconnectData` dos tópicos vivos e `ServerTerminate` em cada servidor.

### 4.3 Atualização (`apps/gridcraft/src/rtd.rs` + `Session::rtd_refresh`)

- Em `App::logic` (todo quadro): se houve valor novo **e** passaram ≥ **200 ms** desde a última
  atualização ⇒ `session.rtd_refresh()` e `request_repaint`. Se só faltar o tempo, agenda
  `request_repaint_after` do restante.
- `rtd_refresh()`: para cada documento aberto em modo **automático**, clona o `wb` (COW),
  `calc.cells_changed(&mut wb, &[])` (só os nós dinâmicos e dependentes), troca o `wb`; se o
  documento **não** estava modificado antes, `saved = wb` (mesmo truque de `DocState::store_view`).
  Nada entra em `undo`/`redo`/`journal`.
- **Varredura:** a atualização abre uma passada nova; ao fim, os tópicos não consultados ⇒
  `DisconnectData` (fórmula apagada, argumento trocado, documento fechado). A varredura **não
  acontece** se algum documento estiver em modo manual (as células dele não reconsultam os tópicos).
- Modo manual: o RTD não atualiza até F9 (`formulas.calculateNow`), como no Excel.

### 4.4 Fora da primeira versão

- `IRtdServer::Heartbeat` (os dois servidores funcionaram sem ele; a reconexão de 5 s cobre a queda).
- Opção de interface para o intervalo (200 ms fixo no código) ou para a lista de servidores.
- Isolar a DLL do Fast Trade num processo auxiliar (é a opção 2, avaliada só se a DLL derrubar o app).
- Persistir os valores RTD de última vez no arquivo (`volatileDependencies.xml`).
- Atualizar `docs/function-parity.md` do upstream (seria conflito em todo merge; o fork documenta em
  `RTD-FORK.md`).

## 5. Testes

| Onde | O que prova | Precisa dos servidores? |
|---|---|---|
| `crates/calc` (fonte falsa) | `=RTD()` devolve o valor da fonte; sem fonte ⇒ `#N/A`; argumento de célula vira o tópico certo; tópico repetido é um só; dependente acompanha após `cells_changed(&[])`; erro em argumento propaga; aridade < 3 ⇒ `#VALUE!` | não |
| `crates/engine` (fonte falsa) | `rtd_refresh()` não cria undo/journal e não suja documento limpo (e mantém sujo o que já estava); modo manual não atualiza; varredura desliga tópico apagado e não varre com doc manual | não |
| `crates/rtd-com` (`#[ignore]`, à mão) | conecta no `srv.rtd` e no `rtdtrading.rtdserver` e lê `SQT/PETR4/LAST` e `PETR4_B_0/ULT` | sim |
| Manual | `Dashboard.xlsm` abre com valores; `GRIDCRAFT_PROFILE` mede a atualização; no pregão, valores andam e Ctrl+Z parado | sim |

A fonte global é trocável nos testes (`install` substitui), porque os testes rodam no mesmo processo.

## 6. Fluxo de trabalho do fork

- Commits no `main` do fork; push só quando o Mav pedir.
- Por commit: `cargo fmt`, `cargo clippy -D warnings` e os testes dos crates tocados; `cargo xtask ci`
  completo uma vez no fim.
- Merge mensal: `git fetch upstream && git merge upstream/main`; conferir os encaixes de §3.1
  (listados em `RTD-FORK.md`); rodar os testes de RTD e `cargo xtask ci`.

## 7. Riscos

| Risco | Mitigação |
|---|---|
| A DLL do Fast Trade trava dentro do processo ⇒ o GridCraft cai | aceito (é como no Excel); se acontecer, opção 2 (processo auxiliar) |
| `#[interface]` do `windows` 0.62 com base `IDispatch` não montar a vtable que os servidores esperam | primeira tarefa do plano é a conexão real (teste `#[ignore]`), antes de qualquer integração |
| 6 mil tópicos × 5 atualizações/s pesar no recálculo | medido com `GRIDCRAFT_PROFILE` no critério 3; se pesar, recalcular só as células cujos tópicos mudaram |
| Upstream reorganizar `eval.rs`/`main.rs` | os encaixes são poucos e listados em `RTD-FORK.md` |
