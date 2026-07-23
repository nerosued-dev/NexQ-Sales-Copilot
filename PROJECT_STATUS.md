# PROJECT_STATUS — NexQ-Sales-Copilot

## Objetivo

O NexQ-Sales-Copilot é um copiloto local para chamadas comerciais no Windows. O aplicativo captura separadamente o microfone do closer e o áudio reproduzido pelo sistema, transcreve os dois canais e organiza a conversa. A integração futura com Codex deverá gerar sugestões discretas durante a reunião e um relatório estruturado após o encerramento. A primeira versão é local e destinada a um único usuário.

## Snapshot auditado

- Data da auditoria: 23 de julho de 2026.
- Branch: `main`.
- Baseline funcional auditado: `38495302ca2f9148b31b9cb399947d0ef0c9574a` (`feat(stt): drive local voice gate with Silero probabilities`).
- Commit imediatamente anterior do classificador: `7af9a276d9fbf90a24a90ee59e4a190b635a47ec` (`feat(vad): add embedded Silero speech classifier`).
- Plataforma prioritária: Windows 11 x64.
- Desktop: Tauri 2.
- Frontend: React e TypeScript.
- Backend: Rust.
- Frontend e backend já compilavam no baseline anterior.

Esta atualização é apenas documental. O código funcional auditado continua sendo o baseline `3849530`; nenhum threshold, endpointing, gate de resposta, arquivo Rust, frontend, persistência ou comportamento de captura foi alterado.

## Validação automatizada deste snapshot

- `cargo test --manifest-path src-tauri\Cargo.toml --lib`: 108 testes descobertos; 107 aprovados, 1 benchmark diagnóstico ignorado e 0 falhas.
- `cargo test --manifest-path src-tauri\Cargo.toml`: mesmos 108 testes de biblioteca, com 107 aprovados, 1 ignorado e 0 falhas; targets de binário e doc-tests também concluíram sem falhas.
- `cargo check --manifest-path src-tauri\Cargo.toml --lib`: aprovado.
- `cargo fmt --all --check` na raiz: não é aplicável porque não existe `Cargo.toml` na raiz.
- `cargo fmt --all --check` em `src-tauri`: reprovado pelo baseline amplo de formatação em arquivos antigos; nenhuma formatação foi aplicada.
- `rustfmt --edition 2021 --check` direcionado a `silero_vad.rs`, `local_voice_gate.rs` e `groq_whisper.rs`: aprovado.
- `cargo clippy --manifest-path src-tauri\Cargo.toml --all-targets --all-features -- -D warnings`: reprovado pelo baseline preexistente, com 84 erros no target `lib` e 85 no target `lib test`.
- O Clippy não reportou diagnóstico em `silero_vad.rs` nem em `local_voice_gate.rs`. Os dois diagnósticos exibidos em `groq_whisper.rs` recaem sobre linhas existentes desde o commit original `4a79580`, não sobre a integração Silero.
- Warnings não bloqueantes dos testes: stdout do linker MSVC ao criar as bibliotecas de importação `.lib`/`.exp`.

## Funcionalidades validadas

- Captura do microfone no canal You.
- Captura do áudio do sistema no canal Them.
- Groq STT com o modelo `whisper-large-v3-turbo` nos dois canais.
- Isolamento dos stores de transcript entre reuniões.
- Barreira determinística de encerramento.
- Gate de resposta Groq baseado em `verbose_json`.
- Fila FIFO limitada e worker ordenado por canal.
- Classificação local de voz com Silero VAD v6.2.1 antes do envio à Groq.

## Arquitetura STT atual

```text
PCM i16 mono 16 kHz
→ Silero VAD v6.2.1
→ LocalVoiceGate
→ ReadyBatch
→ fila FIFO limitada por canal
→ worker Groq ordenado por canal
→ defesa RMS 100
→ Groq whisper-large-v3-turbo / verbose_json
→ gate de resposta
→ acumulador de utterance
→ barreira determinística de encerramento
```

### Classificação e gate local

- O artefato oficial `silero_vad.onnx` da versão `v6.2.1` está incorporado ao binário com `include_bytes!`.
- SHA-256 fixado e conferido: `1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3`.
- Não existe download do modelo em runtime.
- Cada instância de `GroqWhisperSTT` cria sua própria sessão ONNX, estado recorrente, buffer parcial, `LocalVoiceGate`, fila e worker. You e Them não compartilham estado.
- Silero apenas classifica a probabilidade de voz em frames de 512 amostras, equivalentes a 32 ms em 16 kHz.
- O `LocalVoiceGate` usa somente as probabilidades Silero para ativação e endpointing. Ele não calcula nem consulta RMS.
- O VAD RMS legado do módulo de áudio ainda pode preencher `AudioChunk.is_speech` para outros usos, mas esse campo não ativa o `LocalVoiceGate` nem decide requests Groq.
- Os thresholds atuais continuam sendo a hipótese inicial não calibrada: entrada `0,50`, saída negativa abaixo de `0,35`, duas evidências positivas consecutivas, pre-roll de `320 ms`, endpoint após `704 ms` e post-roll de `192 ms`.
- O threshold RMS `100` permanece somente como defesa secundária dentro do worker, depois que o Silero e o `LocalVoiceGate` já aprovaram o lote.
- A Groq continua sendo a única responsável pela transcrição. Whisper Local não foi implementado.

### Ordenação e independência

- Cada canal possui uma fila `mpsc` FIFO limitada a seis lotes.
- O worker de cada provider processa no máximo uma requisição Groq por vez e preserva a ordem de entrada dos lotes daquele canal.
- Backpressure aguarda espaço na fila em vez de descartar silenciosamente um `ReadyBatch`.
- You e Them possuem providers, sequências, filas e workers independentes; uma requisição lenta em um canal não reordena nem bloqueia o worker do outro.

### Encerramento

- O provider classifica o remainder final do Silero, finaliza somente fala confirmada no `LocalVoiceGate` e enfileira o residual válido.
- Em seguida fecha o produtor da fila, aguarda o worker drenar todos os lotes, envia `Flush` ao acumulador e aguarda o acumulador terminar.
- A barreira externa aguarda o pipeline e os tasks de transcript antes de devolver o snapshot final ao frontend.
- Os testes automatizados cobrem resposta atrasada, drenagem de lotes, residual final, erro durante stop, independência dos canais e ausência de task de transcript após o retorno.
- A implementação da barreira externa não foi alterada pelos commits do Silero.

## Correção do manifesto Windows

- O commit `2f7b4d808fd2339f8a3da44deffb1bb3c33948eb` (`fix(build): embed Windows manifest in Rust tests`) incorporou um manifesto ao harness Rust.
- A leitura do recurso `#1` do executável de testes com `mt.exe -inputresource` confirmou:
  - `Microsoft.Windows.Common-Controls`;
  - `version="6.0.0.0"`.
- O harness executou os 108 testes sem a falha de inicialização anterior.
- A validação isolada de schema do `mt.exe` ainda retorna `c10100b7` para `processorArchitecture="*"`. Isso é uma característica do manifesto fonte atual, não impediu a incorporação do recurso nem a execução do harness.

## Resultado manual inicial do Silero

O teste inicial fornecido pelo usuário registrou:

```text
Silêncio:
nenhuma transcrição observada no teste inicial.

Som curto do Windows:
transcrito como “Música”.
Resultado considerado aceitável temporariamente porque não houve frase inventada.

Vídeo no YouTube:
captura e transcrição do canal Them funcionaram no teste inicial.
```

Esse resultado é um baseline preliminar. A contagem de requests Groq e as probabilidades Silero não foram registradas nesse teste. A calibração não está concluída, e o teste não demonstra que todos os sons não verbais serão rejeitados.

O texto isolado `Música` para um som curto do Windows não reprova sozinho a primeira versão, conforme a decisão atual do usuário. Não existe filtro específico para `Música`, e o gate de resposta não foi alterado para rejeitar descrições de música.

O roteiro completo, a tabela de registro e os critérios de decisão estão em [`docs/SILERO_MANUAL_VALIDATION.md`](docs/SILERO_MANUAL_VALIDATION.md).

## Itens antigos corrigidos ou reclassificados

| Afirmação anterior | Estado atual |
| --- | --- |
| Silêncio sempre é encaminhado à Groq | Resolvido para o fluxo normal: somente lotes com fala confirmada pelo Silero/`LocalVoiceGate` entram na fila. A validação manual completa ainda deve confirmar o comportamento no ambiente real. |
| O gate local usa apenas RMS | Resolvido: ativação e endpointing usam probabilidades Silero; RMS 100 é apenas defesa secundária. |
| Requests Groq do mesmo canal podem terminar fora de ordem | Resolvido: fila FIFO limitada e um worker por provider serializam as requests de cada canal. |
| O test harness Windows continua quebrado | Resolvido pelo manifesto incorporado; os testes executaram no Windows. |
| O `LocalVoiceGate` está desconectado do provider | Resolvido: o provider alimenta o gate com frames e probabilidades Silero e enfileira seus `ReadyBatch`. |
| A barreira de encerramento ainda não foi implementada | Resolvido pelos commits `722a0ad` e `a693c9e`; o stop aguarda pipeline, providers e tasks de transcript. |
| Reuniões curtas podem perder o transcript final por snapshot prematuro | Histórico do diagnóstico anterior; a barreira determinística e o flush ordenado cobrem a corrida que havia sido identificada. |

## Histórico preservado

### Isolamento do transcript entre reuniões

O commit `e577d6e8c6c90faee82a05d93be0fed55a9570c5` isolou e reinicializou os stores Zustand do launcher e do overlay entre reuniões. A preocupação posterior com respostas antigas sem identidade suficiente passou a ser mitigada pela barreira determinística, que não devolve o stop enquanto pertencem requests e tasks à captura encerrada.

Essa correção histórica:

- adicionou reset completo da sessão do transcript;
- passou a resetar o launcher antes de iniciar a captura;
- passou a resetar o store próprio do overlay ao receber o início da reunião;
- reiniciou o checkpoint de persistência;
- manteve o transcript até o flush final informar sucesso;
- envolveu `src/App.tsx`, `src/stores/meetingStore.ts` e `src/stores/transcriptStore.ts`.

### Reuniões curtas e encerramento

Antes da barreira, reuniões de aproximadamente 15 e 30 segundos podiam exibir transcript ao vivo e salvar zero palavras, enquanto uma reunião próxima de um minuto era persistida. Uma reunião de 11 segundos chegou a indicar duas palavras formadas apenas por `"."`. A causa confirmada era o retorno de `stop_capture` antes da drenagem dos providers e tasks, combinado com reuniões que terminavam antes do checkpoint periódico.

Depois da correção, a validação manual da barreira observou:

1. conclusão da request Groq ativa;
2. recebimento de `Flush` pelo acumulador;
3. término dos tasks de transcript;
4. retorno de `stop_capture` somente depois;
5. persistência dos segmentos;
6. `end_meeting` por último.

O encerramento no meio da fala preservou os segmentos finais no teste manual anterior.

### Diagnóstico anterior de silêncio e gate de resposta

Antes do gate local, o provider podia enviar lotes de baixo RMS à Groq, e o gate `verbose_json` aceitava algumas frases plausíveis produzidas durante silêncio ou ruído. Os metadados `no_speech_prob` e `avg_logprob` não separaram esses casos de forma confiável. Esse diagnóstico motivou o gate acústico local com Silero.

Nos testes históricos anteriores ao Silero:

- pontuação e espaços isolados foram rejeitados;
- alucinações plausíveis foram aceitas com RMS aproximado de `110`, `116`, `138` e `199`;
- `avg_logprob` dessas respostas variou aproximadamente entre `-0,38` e `-0,65`;
- `no_speech_prob` chegou a `0,0` tanto em resultados rejeitados quanto em alucinações aceitas;
- fala normal continuou sendo transcrita depois dos falsos positivos.

O gate de resposta continua útil para rejeitar conteúdo vazio, pontuação isolada, frases exatas conhecidas e metadados extremos. Ele não deve ser tratado como classificador acústico nem como garantia contra toda descrição de som não verbal.

### Instrumentação de diagnóstico

Os logs `NEXQ_TRANSCRIPT_DIAG` registram metadados técnicos em builds de debug: canal, sequência, duração, RMS, estado do gate, probabilidades agregadas, runs positivos, tamanho lógico da fila e eventos de encerramento. Não registram chaves, tokens, áudio nem conteúdo integral do transcript por padrão.

## Limitações abertas

- A matriz de validação manual ainda não foi executada integralmente.
- Os thresholds Silero/endpointing são hipóteses iniciais e não estão calibrados para o ambiente do usuário.
- Uma única ocorrência de `Música` para um chime foi tolerada; sons não verbais ainda podem produzir descrições ou texto.
- A contagem de requests e as probabilidades do teste manual inicial não foram preservadas.
- O idioma default de `GroqConfig` no backend ainda é `en`; a validação em português deve confirmar a configuração `pt`.
- Não há AEC, noise suppression nem garantia de eliminação de vazamento acústico; fones continuam recomendados.
- Não há retry, backoff ou reprocessamento automático da fila Groq.
- Não há filtro específico para `Música`.
- Whisper Local não foi implementado.
- Diarização de múltiplos participantes remotos não faz parte da primeira versão.
- Codex App Server ainda não foi integrado como provider de análise.
- O baseline global de formatação e Clippy permanece aberto e não foi alterado nesta tarefa.

## Backlog reclassificado

- Incluir `meetingId` e `captureSessionId` nos eventos continua sendo hardening útil, embora a barreira impeça resultados pertencentes à captura encerrada de escapar após o stop.
- Rejeição de pontuação isolada, fila ordenada e espera das requests no encerramento estão resolvidas.
- Retry, backoff, reprocessamento e eventual deduplicação continuam abertos.
- Português como default do backend continua aberto.
- A integração futura com Codex App Server permanece separada do STT.

## Próximo passo

Executar o roteiro de [`docs/SILERO_MANUAL_VALIDATION.md`](docs/SILERO_MANUAL_VALIDATION.md), preencher a tabela com contagem de requests e evidência agregada e decidir:

1. aprovar o baseline para a primeira versão;
2. repetir cenários inconclusivos;
3. abrir calibração somente se houver um padrão reproduzível e mensurado.

Não alterar thresholds com base em uma ocorrência isolada.

## Segurança e privacidade

Este documento não contém chaves, tokens, segredos, áudio nem conteúdo de chamadas.
