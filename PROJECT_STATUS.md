# PROJECT_STATUS — NexQ-Sales-Copilot

## Objetivo

O NexQ-Sales-Copilot é um copiloto local para chamadas comerciais no Windows. O aplicativo captura separadamente o microfone do closer e o áudio reproduzido pelo sistema, transcreve os dois canais, organiza a conversa e, futuramente, usa o Codex para gerar sugestões discretas durante a reunião e um relatório estruturado após o encerramento. A primeira versão é local e destinada a um único usuário.

## Ambiente e baseline técnico

- Plataforma prioritária: Windows 11 x64.
- Desktop: Tauri 2.
- Frontend: React e TypeScript.
- Backend: Rust.
- Toolchain nativo disponível: LLVM, libclang, CMake e Ninja instalados.
- Frontend compilando.
- Backend compilando.
- Testes Rust: 45 aprovados.
- Clippy: há um baseline preexistente de 86 diagnósticos quando executado com `-D warnings`. Esses diagnósticos não foram introduzidos pela correção de isolamento entre reuniões.

## Funcionalidades validadas

- Captura do microfone validada.
- Captura do áudio do sistema validada.
- Groq STT com o modelo `whisper-large-v3-turbo` validado nos dois canais.

## Correção mais recente: isolamento do transcript entre reuniões

Foi implementada uma correção para limpar separadamente os stores Zustand mantidos pelas janelas do launcher e do overlay entre reuniões.

A correção:

- adiciona um reset completo da sessão do transcript;
- reseta o store do launcher antes de iniciar a captura;
- reseta o store próprio do overlay ao receber o evento de início da reunião;
- reinicia o checkpoint de persistência da reunião;
- só limpa o transcript no encerramento depois que o flush final informado pelo frontend termina com sucesso;
- adiciona logs de ciclo de vida sem registrar conteúdo do transcript.

Arquivos alterados nessa correção:

- `src/App.tsx`
- `src/stores/meetingStore.ts`
- `src/stores/transcriptStore.ts`

Commit mais recente:

- Hash completo: `e577d6e8c6c90faee82a05d93be0fed55a9570c5`
- Hash curto: `e577d6e`
- Mensagem: `fix: isolate transcript state between meetings`

### Teste manual de duas reuniões consecutivas

O teste manual confirmou que, ao encerrar a primeira reunião e iniciar a segunda, os estados de transcript do launcher e do overlay são reinicializados separadamente, evitando que segmentos já presentes no frontend sejam carregados diretamente de uma reunião para a seguinte.

Permanece uma limitação: respostas atrasadas da Groq ainda não carregam `meetingId` nem `captureSessionId`. Por isso, uma resposta iniciada durante uma reunião pode chegar depois do reset e não há identificação suficiente no evento para descartá-la com segurança ou associá-la inequivocamente à sessão correta.

## Bug confirmado: reuniões curtas sem transcript salvo

Foi confirmado um problema distinto da limpeza dos stores:

- reuniões de aproximadamente 15 e 30 segundos exibem transcript ao vivo;
- ao abrir essas reuniões salvas, aparecem zero palavras ou nenhum transcript;
- uma reunião de aproximadamente um minuto foi salva corretamente;
- uma reunião de 11 segundos indicou duas palavras, mas elas eram apenas transcrições de ponto (`"."`) e não havia conteúdo salvo;
- a principal suspeita é que a persistência periódica não ocorra a tempo em reuniões curtas ou que o flush final encerre sem aguardar segmentos e requisições ainda pendentes.

Não há correção implementada para esse bug neste momento.

## Diagnóstico confirmado: encerramento e persistência do transcript

A instrumentação temporária e a inspeção do fluxo confirmaram que:

- `stop_capture` encerra os produtores de áudio, mas retorna ao frontend antes da conclusão do task assíncrono que drena os chunks e encerra os providers STT;
- por isso, um segmento final pode ser emitido depois do snapshot usado pelo flush final, depois de `end_meeting` e depois do reset dos stores;
- o checkpoint periódico de 30 segundos é executado, mas não persiste segmentos enquanto o primeiro item ainda não persistido permanece parcial, pois a seleção exige um prefixo contíguo de segmentos finais;
- reuniões curtas podem terminar antes do primeiro checkpoint e dependem integralmente do flush de encerramento;
- não há correção implementada para essa corrida neste momento.

## Diagnóstico confirmado: detecção de voz e ruído

- O VAD atual é baseado em RMS, usa threshold fixo de `300` sobre energia suavizada e preenche `AudioChunk.is_speech`.
- O resultado desse VAD não controla o envio ao provider Groq: todos os chunks não mutados continuam sendo encaminhados a `feed_audio`.
- O provider Groq aplica outro threshold fixo: lotes com RMS menor que `100` são tratados como silêncio; lotes com RMS maior ou igual a `100` podem ser enviados à API.
- Qualquer resposta textual não vazia, fora da pequena lista de frases exatas reconhecidas como alucinação, é convertida em `Speech`.
- Não existe cancelamento de eco acústico (AEC) no pipeline atual.
- A barra visual usa `RMS / 3000`, suavização EMA e escala logarítmica. Ela amplifica níveis baixos e não representa diretamente o gate RMS usado pelo provider Groq.
- Não há correção implementada para detecção de voz, ruído, alucinações ou vazamento acústico neste momento.

## Validação manual: barreira de encerramento

A barreira determinística de encerramento foi validada novamente em uma chamada encerrada enquanto havia uma requisição Groq ativa. O teste confirmou a seguinte ordem:

1. a requisição Groq em andamento terminou;
2. o acumulador recebeu `Flush`;
3. os tasks de transcript terminaram;
4. `stop_capture_returning` ocorreu somente depois;
5. todos os segmentos foram persistidos;
6. `end_meeting` ocorreu por último.

O encerramento no meio da fala também preservou os segmentos finais. Essa validação confirma que `stop_capture` não retorna antes das requisições e dos tasks pertencentes à captura encerrada terminarem.

## Validação manual: gate de resposta Groq

O gate baseado em `verbose_json` foi apenas parcialmente eficaz:

- respostas contendo somente espaços ou pontuação isolada foram rejeitadas corretamente;
- a resposta agora fornece metadados reais por segmento, incluindo `no_speech_prob` e `avg_logprob` quando presentes;
- alucinações textuais plausíveis ainda foram aceitas como `Speech` durante aproximadamente o primeiro minuto sem fala humana no canal You;
- no silêncio observado, `no_speech_prob` retornou `0.0` tanto para resultados rejeitados quanto para alucinações aceitas;
- alucinações aceitas ocorreram com RMS baixo, incluindo valores aproximados de `110`, `116`, `138` e `199`;
- `avg_logprob` das alucinações aceitas variou aproximadamente entre `-0.38` e `-0.65` em vários casos;
- um som curto reproduzido pelo Windows no canal Them também foi convertido em texto inventado;
- depois desses casos, fala normal nos canais You e Them continuou sendo transcrita.

Portanto, os thresholds atuais de `no_speech_prob` e `avg_logprob` não distinguem de forma confiável silêncio real, ruído curto e fala humana nesse ambiente. O gate elimina artefatos simples de pontuação e melhora a observabilidade, mas não resolve as alucinações durante silêncio.

## Instrumentação temporária de diagnóstico

Os logs `NEXQ_TRANSCRIPT_DIAG` registram somente metadados técnicos, como timestamps, janela, IDs técnicos, contagens, RMS, duração de lote e resultado das etapas. Os logs frontend ficam restritos ao modo de desenvolvimento e os logs Rust a builds de debug.

A instrumentação não altera a ordem funcional do encerramento, não adiciona espera por tasks ou requisições, não muda thresholds, seleção de segmentos, persistência, schemas ou migrations. Texto de transcript, áudio, tokens, chaves e corpos de requisição não são registrados.

## Próximo passo

Auditar um gate local anterior ao envio de áudio para a Groq. A auditoria deve incluir, no mínimo:

- verificar por que `AudioChunk.is_speech` atualmente não controla o envio ao provider;
- medir a proporção de frames classificados como voz em cada lote;
- avaliar WebRTC VAD para decisões rápidas por frame;
- avaliar Silero VAD para uma classificação local mais robusta;
- criar fixtures com silêncio, ruído ambiente, sons curtos do Windows, fala curta e fala distante.

Não tentar resolver o problema apenas aumentando thresholds RMS, `no_speech_prob` ou `avg_logprob`. Qualquer alteração futura de thresholds deve ser validada contra fala curta legítima e fala distante para evitar falsos negativos.

## Backlog posterior

- Incluir `meetingId` e `captureSessionId` nos eventos.
- Implementar cancelamento e espera das requisições Groq no encerramento.
- Definir português como idioma padrão.
- Filtrar transcrições `"."` geradas durante silêncio.
- Implementar fila, retry, backoff, ordenação e deduplicação.
- Integrar futuramente o Codex App Server como provedor de análise separado do STT.

## Segurança e privacidade

Este documento não contém chaves, tokens, segredos nem conteúdo das transcrições.
