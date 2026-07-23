# Validação manual do Silero VAD

## Objetivo e escopo

Este roteiro valida o gate acústico local do NexQ-Sales-Copilot no Windows 11 x64. Ele mede se fala legítima chega à Groq, se silêncio e ruídos comuns deixam de gerar requests desnecessárias e se You e Them permanecem independentes.

O roteiro não autoriza alteração de thresholds, endpointing, lógica de áudio, gate de resposta Groq ou frontend. Silero classifica probabilidade de voz; a Groq continua sendo a única responsável pela transcrição.

## Preparação

1. Usar o baseline funcional `38495302ca2f9148b31b9cb399947d0ef0c9574a` ou um descendente que não altere o pipeline STT.
2. Confirmar `whisper-large-v3-turbo`, idioma `pt`, temperatura `0` e resposta `verbose_json`.
3. Ativar o modo de diagnóstico local em build de debug.
4. Anotar horário de início e fim de cada cenário.
5. Manter volume, dispositivo, distância e ambiente constantes durante repetições comparáveis.
6. Executar primeiro sem fones e repetir a etapa de independência com fones.
7. Não registrar chaves, tokens, áudio ou conteúdo integral de chamadas nos logs.

## Como preencher a tabela

- **Requests Groq:** contar os eventos `event=request_started` do canal durante o cenário. Usar `0` quando os logs comprovarem ausência de request.
- **Resultado exibido:** copiar somente o texto curto apresentado pela interface ou registrar `nenhum`.
- **Fala preservada:** usar `sim`, `não` ou `não se aplica`.
- **Falso positivo:** usar `sim`, `não` ou `inconclusivo`; explicar ocorrências isoladas em **Observação**.
- **Probabilidade Silero:** registrar `média <valor>; máxima <valor>` quando os logs agregados fornecerem os dois valores. Quando não estiver disponível, usar exatamente `não registrado`.
- **Observação:** registrar duração, distância, repetição e qualquer transição anormal entre `Candidate`, `Active` e `Hangover`.

Antes de recomendar calibração, repetir o cenário suspeito em condições equivalentes e preencher uma linha por repetição.

## Canal You

1. Manter 60 segundos de silêncio.
2. Digitar no teclado.
3. Movimentar e clicar o mouse.
4. Produzir barulho de cadeira ou mesa.
5. Manter ventilador ou ar-condicionado ligado.
6. Dizer “sim”.
7. Dizer “não”.
8. Dizer um nome curto.
9. Dizer números curtos.
10. Falar normalmente por 10 segundos.
11. Falar em volume baixo.
12. Falar a aproximadamente um metro do microfone.
13. Fazer uma pausa curta no meio de uma frase.
14. Encerrar a captura imediatamente após uma palavra.

## Canal Them

1. Manter 30 segundos sem reprodução de áudio.
2. Reproduzir um som curto do Windows.
3. Reproduzir dois sons do Windows em sequência.
4. Reproduzir uma notificação de navegador.
5. Reproduzir música instrumental curta.
6. Reproduzir vídeo com fala masculina.
7. Reproduzir vídeo com fala feminina.
8. Reproduzir vídeo com fala baixa.
9. Reproduzir vídeo com música de fundo e fala.
10. Pausar o vídeo.
11. Encerrar a captura durante uma fala.

## Independência dos canais

1. Falar apenas em You.
2. Reproduzir áudio apenas em Them.
3. Usar You e Them simultaneamente.
4. Confirmar que lentidão ou silêncio de um canal não bloqueia o outro.
5. Repetir parte do teste usando fones.

## Resultado manual inicial

| Cenário | Canal | Requests Groq | Resultado exibido | Fala preservada | Falso positivo | Probabilidade Silero | Observação |
| ------- | ----- | ------------: | ----------------- | --------------- | -------------- | -------------------: | ---------- |
| Silêncio, duração não registrada | não registrado | não registrado | nenhum | não se aplica | não observado | não registrado | Nenhuma transcrição observada no teste inicial. |
| Som curto do Windows | Them | não registrado | `Música` | não se aplica | inconclusivo | não registrado | Ocorrência isolada aceita temporariamente porque não houve frase inventada. |
| Vídeo no YouTube | Them | não registrado | transcrição considerada adequada | sim | não observado | não registrado | Captura e transcrição do canal Them funcionaram no teste inicial. |

O teste inicial não conclui a calibração e não garante rejeição de todos os sons não verbais.

## Tabela de execução

| Cenário | Canal | Requests Groq | Resultado exibido | Fala preservada | Falso positivo | Probabilidade Silero | Observação |
| ------- | ----- | ------------: | ----------------- | --------------- | -------------- | -------------------: | ---------- |
| 60 s de silêncio | You | pendente | pendente | não se aplica | pendente | pendente |  |
| Digitação no teclado | You | pendente | pendente | não se aplica | pendente | pendente |  |
| Movimento e clique do mouse | You | pendente | pendente | não se aplica | pendente | pendente |  |
| Barulho de cadeira ou mesa | You | pendente | pendente | não se aplica | pendente | pendente |  |
| Ventilador ou ar-condicionado | You | pendente | pendente | não se aplica | pendente | pendente |  |
| Palavra “sim” | You | pendente | pendente | pendente | pendente | pendente |  |
| Palavra “não” | You | pendente | pendente | pendente | pendente | pendente |  |
| Nome curto | You | pendente | pendente | pendente | pendente | pendente |  |
| Números curtos | You | pendente | pendente | pendente | pendente | pendente |  |
| Fala normal por 10 s | You | pendente | pendente | pendente | pendente | pendente |  |
| Fala baixa | You | pendente | pendente | pendente | pendente | pendente |  |
| Fala a aproximadamente 1 m | You | pendente | pendente | pendente | pendente | pendente |  |
| Pausa curta no meio da frase | You | pendente | pendente | pendente | pendente | pendente |  |
| Stop imediatamente após uma palavra | You | pendente | pendente | pendente | pendente | pendente | Confirmar palavra final e ausência de resultado tardio. |
| 30 s sem reprodução | Them | pendente | pendente | não se aplica | pendente | pendente |  |
| Um som curto do Windows | Them | pendente | pendente | não se aplica | pendente | pendente | `Música` isolado não reprova sozinho. |
| Dois sons do Windows em sequência | Them | pendente | pendente | não se aplica | pendente | pendente |  |
| Notificação de navegador | Them | pendente | pendente | não se aplica | pendente | pendente |  |
| Música instrumental curta | Them | pendente | pendente | não se aplica | pendente | pendente |  |
| Vídeo com fala masculina | Them | pendente | pendente | pendente | pendente | pendente |  |
| Vídeo com fala feminina | Them | pendente | pendente | pendente | pendente | pendente |  |
| Vídeo com fala baixa | Them | pendente | pendente | pendente | pendente | pendente |  |
| Vídeo com música de fundo e fala | Them | pendente | pendente | pendente | pendente | pendente |  |
| Pausa no vídeo | Them | pendente | pendente | pendente | pendente | pendente | Confirmar endpoint sem texto inventado recorrente. |
| Stop durante uma fala | Them | pendente | pendente | pendente | pendente | pendente | Confirmar parte final válida e ausência de resultado tardio. |
| Fala apenas em You | Independência | pendente | pendente | pendente | pendente | pendente | Confirmar ausência de bloqueio por Them. |
| Áudio apenas em Them | Independência | pendente | pendente | pendente | pendente | pendente | Confirmar ausência de bloqueio por You. |
| You e Them simultâneos | Independência | pendente | pendente | pendente | pendente | pendente | Registrar resultado por canal. |
| Lentidão ou silêncio em um canal | Independência | pendente | pendente | pendente | pendente | pendente | Confirmar progresso do outro canal. |
| Repetição com fones | Independência | pendente | pendente | pendente | pendente | pendente | Comparar vazamento acústico. |

## Verificações de encerramento

Em todos os cenários de stop:

1. marcar o instante em que o encerramento foi solicitado;
2. confirmar que nenhuma atualização de transcript aparece depois do retorno do stop;
3. confirmar que a última palavra ou parte final válida aparece no snapshot final;
4. confirmar que a sessão persistida contém o mesmo conteúdo final;
5. registrar qualquer request, `Flush` ou resultado que apareça fora da ordem esperada.

## Critérios de aprovação para a primeira versão

O baseline pode ser aprovado quando:

1. silêncio prolongado não gerar requests Groq;
2. ruídos normais de ambiente não gerarem frases inventadas recorrentes;
3. palavras curtas forem preservadas;
4. fala normal e baixa forem transcritas;
5. o canal Them transcrever vídeos e chamadas;
6. You e Them permanecerem independentes;
7. nenhum transcript chegar após o encerramento;
8. nenhuma parte final válida desaparecer no stop;
9. não houver regressão nos testes automatizados.

O texto isolado `Música` para um som curto do Windows não reprova sozinho esta versão. Essa exceção não deve ser generalizada para frases inventadas, ocorrências recorrentes ou perda de fala.

## Critérios para calibração futura

Não recomendar mudança de thresholds com base em uma ocorrência isolada. Considerar um padrão repetível somente quando a mesma falha for reproduzida em pelo menos três execuções controladas do mesmo cenário.

Abrir uma análise de calibração quando houver padrão como:

- vários falsos positivos durante silêncio;
- digitação ativando frequentemente;
- chimes gerando frases inventadas;
- palavras curtas sendo perdidas;
- fala baixa não sendo detectada;
- alternância excessiva entre `Active` e `Hangover`;
- muitos candidatos rejeitados contendo fala real.

Antes de propor qualquer valor novo, registrar para cada repetição:

- canal;
- cenário;
- probabilidades média e máxima;
- maior sequência positiva;
- duração;
- resultado Groq;
- texto exibido;
- quantidade de repetições.

Também é obrigatório:

1. separar falha acústica de falha da Groq, do gate de resposta, da fila ou da persistência;
2. manter um cenário de controle com “sim”, “não”, fala baixa e fala a um metro;
3. verificar que uma possível correção de falso positivo não remove fala legítima;
4. executar novamente os testes automatizados e os cenários de encerramento.

Se os dados agregados não contiverem probabilidade, registrar `não registrado`; não estimar nem inventar valores.

## Decisão ao final da rodada

Registrar uma das conclusões:

- **Aprovado para a primeira versão:** todos os critérios de aprovação foram atendidos.
- **Repetição necessária:** houve resultado inconclusivo, sem padrão suficiente para calibração.
- **Candidato a calibração:** a mesma falha foi reproduzida pelo menos três vezes e possui as métricas obrigatórias.
- **Regressão funcional:** fala legítima, independência, ordenação ou encerramento falhou; investigar a camada correta antes de alterar thresholds.
