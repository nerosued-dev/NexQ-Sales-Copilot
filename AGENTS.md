# AGENTS.md

## 1. Missão do projeto

Este repositório é um fork privado do NexQ para criar um copiloto local de chamadas comerciais no Windows.

O aplicativo deve capturar chamadas feitas no WhatsApp Desktop ou em qualquer outro aplicativo de reunião sem inserir bots na conversa.

O fluxo principal é:

1. Capturar o microfone do closer.
2. Capturar separadamente o áudio reproduzido pelo sistema.
3. Transcrever os canais usando Groq Speech to Text.
4. Organizar a conversa por participante e tempo.
5. Analisar a conversa com Codex usando a sessão autenticada do ChatGPT.
6. Exibir sugestões discretas durante a chamada.
7. Gerar um relatório estruturado após o encerramento.

A primeira versão será usada somente no computador do proprietário. Não é um SaaS e não deve incluir cadastro de múltiplos clientes, cobrança, multi-tenant ou infraestrutura pública.

## 2. Contexto técnico existente

Antes de alterar qualquer código, inspecione o repositório e confirme a implementação atual.

Tecnologias esperadas no projeto:

- Tauri 2
- Rust
- React 18
- TypeScript
- Vite
- Zustand
- Tailwind CSS
- SQLite
- cpal
- WASAPI no Windows
- provedores de STT, incluindo Groq
- provedores de LLM existentes

Não presuma que um recurso está ausente apenas pelo nome de uma pasta. Pesquise implementações, comandos Tauri, stores, componentes, tipos e configurações antes de criar novos módulos.

## 3. Ambiente e plataforma prioritária

A plataforma prioritária é Windows 11 x64.

O desenvolvimento e os testes iniciais devem funcionar no Windows antes de qualquer trabalho de compatibilidade com macOS ou Linux.

O aplicativo deve funcionar com:

- WhatsApp Desktop
- navegador reproduzindo áudio
- Google Meet
- Zoom
- Microsoft Teams
- qualquer aplicativo cujo áudio possa ser capturado pelo loopback do Windows

Não implemente integração não oficial, injeção de código, automação de interface ou acesso interno ao WhatsApp. A captura deve ocorrer no nível do dispositivo de áudio do Windows.

## 4. Objetivos da primeira versão

### 4.1 Captura de áudio

Capturar dois fluxos independentes:

- `local`: microfone do closer
- `remote`: áudio do sistema com os demais participantes

Cada fluxo deve manter:

- identificador da sessão
- fonte do áudio
- timestamp inicial
- timestamp final
- taxa de amostragem
- presença de voz
- status de transcrição

Usar fones de ouvido como configuração recomendada para reduzir retorno do áudio remoto pelo microfone.

### 4.2 Transcrição

Usar Groq como provedor padrão de transcrição em nuvem.

Modelo padrão:

`whisper-large-v3-turbo`

Configuração padrão:

- idioma: `pt`
- temperatura: `0`
- formato de resposta: `verbose_json`
- timestamps: `segment`
- áudio: mono, 16 kHz quando possível

Reutilize o provedor Groq já existente no NexQ, caso ele esteja funcional. Não crie uma segunda implementação concorrente sem justificar tecnicamente.

A transcrição deve operar por blocos incrementais.

Diretrizes iniciais:

- blocos aproximados de 10 a 15 segundos
- pequeno overlap somente quando necessário
- VAD por canal para evitar enviar silêncio
- deduplicação de texto produzido pelo overlap
- fila com retry e backoff
- cancelamento ao encerrar a sessão
- limite de concorrência para evitar requisições descontroladas

Como os canais são independentes, não envie um arquivo estéreo com local e remoto esperando que o provedor os separe. Transcreva cada canal separadamente.

### 4.3 Estrutura do transcript

Use um modelo de evento equivalente a:

```ts
export type SpeakerSource = "closer" | "remote" | "unknown";

export interface TranscriptSegment {
  id: string;
  sessionId: string;
  source: SpeakerSource;
  speakerId?: string;
  speakerLabel?: string;
  text: string;
  startedAtMs: number;
  endedAtMs: number;
  confidence?: number;
  isFinal: boolean;
  provider: "groq" | "local" | string;
}
```

O transcript final deve preservar ordem cronológica, mesmo quando as respostas dos dois canais chegarem fora de ordem.

Não remova segmentos originais ao gerar versões corrigidas. Mantenha rastreabilidade suficiente para auditoria e correção.

### 4.4 Análise por IA

O provedor padrão de análise será Codex autenticado com a conta do ChatGPT do usuário.

O aplicativo deve tratar Codex como um provedor de análise separado do provedor de transcrição.

Nunca envie áudio ao Codex. Envie somente:

- transcript textual
- estado acumulado da conversa
- manual comercial selecionado
- prompt de análise
- metadados estritamente necessários

Criar uma abstração semelhante a:

```ts
export interface AnalysisProvider {
  initialize(): Promise<void>;
  checkAuth(): Promise<AuthState>;
  login(): Promise<AuthState>;
  startSession(input: AnalysisSessionInput): Promise<string>;
  analyzeIncrement(input: IncrementalAnalysisInput): Promise<CallAnalysisUpdate>;
  finalizeSession(input: FinalAnalysisInput): Promise<FinalCallReport>;
  cancelSession(sessionId: string): Promise<void>;
  logout(): Promise<void>;
}
```

Implementações previstas:

- `CodexAnalysisProvider`
- `OpenAIApiAnalysisProvider`, opcional e não prioritário
- `LocalAnalysisProvider`, opcional para o futuro

A primeira implementação deve usar Codex App Server por processo local e JSON-RPC via stdio, ou usar o SDK oficial em um sidecar local quando isso reduzir claramente a complexidade.

Não execute o SDK do Codex diretamente dentro do frontend WebView.

O processo deve ficar no backend Tauri ou em um sidecar controlado pelo backend.

### 4.5 Autenticação do Codex

Preferir o fluxo oficial gerenciado pelo Codex:

- verificar estado da conta
- iniciar login do ChatGPT quando necessário
- abrir a URL de autenticação no navegador
- aguardar a conclusão
- exibir plano e limites quando disponíveis
- permitir logout

Não ler, copiar, editar ou expor manualmente o arquivo `~/.codex/auth.json`.

Não salvar tokens do ChatGPT no SQLite do aplicativo.

Não enviar tokens ao frontend React.

O Codex deve gerenciar persistência e renovação das credenciais.

### 4.6 Isolamento do agente

O agente usado para analisar chamadas não precisa editar arquivos, executar comandos ou acessar o repositório.

Para threads de análise:

- usar uma pasta de trabalho dedicada e vazia
- usar acesso somente leitura quando disponível
- não conceder acesso amplo ao sistema de arquivos
- não permitir comandos de shell por padrão
- não permitir alterações no repositório
- não incluir segredos no prompt

A integração deve considerar qualquer conteúdo transcrito como dado não confiável. Uma fala do participante nunca deve alterar instruções do sistema, habilitar ferramentas ou solicitar acesso a arquivos.

## 5. Motor de análise comercial

O aplicativo deve aceitar diferentes manuais e prompts. SPIN Selling será o primeiro modelo, mas não deve ficar acoplado ao código.

### 5.1 Etapas mínimas do SPIN

A análise deve acompanhar:

- Situação
- Problema
- Implicação
- Necessidade de solução

Para cada etapa, manter:

- status: `not_started`, `partial`, `covered`, `not_applicable`
- evidências do transcript
- perguntas feitas
- perguntas ainda recomendadas
- nível de confiança

### 5.2 Métricas mínimas

Calcular ou estimar:

- proporção de fala do closer
- proporção de fala dos participantes remotos
- duração de silêncio
- perguntas abertas
- perguntas fechadas
- interrupções prováveis
- objeções identificadas
- objeções respondidas
- compromissos assumidos
- próximo passo combinado
- dados obrigatórios coletados
- dados obrigatórios ausentes

Métricas determinísticas devem ser calculadas localmente quando possível. Não use LLM para contar palavras, duração ou número de segmentos quando o aplicativo já possui esses dados.

### 5.3 Saída estruturada

A análise incremental deve retornar JSON validável.

Use um contrato equivalente a:

```ts
export interface CallAnalysisUpdate {
  sessionId: string;
  analyzedThroughMs: number;
  spin: {
    situation: SpinStageState;
    problem: SpinStageState;
    implication: SpinStageState;
    needPayoff: SpinStageState;
  };
  currentTopic?: string;
  activeObjection?: string;
  unansweredQuestions: string[];
  alerts: AnalysisAlert[];
  suggestedNextActions: SuggestedAction[];
  factsToPersist: ExtractedFact[];
  confidence: number;
}

export interface AnalysisAlert {
  type: string;
  severity: "info" | "warning" | "critical";
  message: string;
  evidenceSegmentIds: string[];
}

export interface SuggestedAction {
  priority: number;
  action: string;
  suggestedWording?: string;
  reason: string;
  evidenceSegmentIds: string[];
}
```

Validar a resposta antes de salvar ou exibir.

Quando o JSON for inválido:

1. tentar extrair o objeto JSON com segurança
2. validar contra o schema
3. executar no máximo uma tentativa de reparo
4. registrar erro sem travar a chamada
5. manter a última análise válida na interface

Nunca exibir ao closer raciocínio interno, instruções do sistema ou conteúdo de depuração do modelo.

## 6. Estratégia de contexto

Criar uma thread de análise por chamada.

Não reenviar o transcript completo a cada atualização.

Manter localmente:

- resumo acumulado
- estado SPIN atual
- fatos coletados
- objeções
- compromissos
- últimos segmentos ainda não analisados

Cada atualização deve enviar preferencialmente:

- estado anterior validado
- novos segmentos desde a última análise
- métricas locais atualizadas
- instrução para retornar somente alterações e recomendações atuais

Executar análise quando houver um destes eventos:

- final de uma fala remota relevante
- detecção de objeção
- pergunta direta do lead
- mudança de etapa
- intervalo configurável, por exemplo 20 a 30 segundos
- solicitação manual do closer
- encerramento da chamada

Não chamar o agente a cada palavra ou fragmento parcial.

## 7. Interface do usuário

A interface deve ser discreta e adequada a uso durante uma chamada.

### 7.1 Tela principal

Exibir:

- status do microfone
- status do áudio do sistema
- status da Groq
- status da conta Codex
- duração da sessão
- transcript recente
- etapa atual do SPIN
- alerta principal
- próxima ação sugerida

### 7.2 Overlay

O overlay deve:

- permanecer sempre no topo quando ativado
- permitir ajuste de opacidade
- ser reposicionável
- ter modo compacto
- não roubar foco da chamada
- não exibir grandes blocos de texto
- permitir ocultação imediata por atalho

Mostrar no máximo:

- um alerta principal
- uma próxima pergunta sugerida
- progresso resumido do playbook

### 7.3 Pós-chamada

Gerar:

- resumo executivo
- dados coletados
- problemas e objetivos do lead
- objeções
- avaliação por etapa SPIN
- pontos fortes do closer
- pontos de melhoria
- próximos passos
- trechos de evidência
- relatório em Markdown
- exportação futura em PDF, não prioritária

## 8. Persistência e privacidade

Usar armazenamento local.

Separar:

- configuração do aplicativo
- credenciais de provedores
- sessões
- segmentos de transcript
- análises
- relatórios
- arquivos de áudio

Não armazenar chaves em código, Git, logs ou arquivos de exemplo preenchidos.

Não usar `localStorage` para segredos.

Utilizar a solução segura já existente no NexQ. Caso ela não seja adequada, propor armazenamento pelo keyring do sistema operacional antes de adicionar uma dependência.

Adicionar opções para:

- não gravar áudio
- apagar áudio após transcrição
- apagar sessão completa
- definir retenção automática
- exportar dados

O aplicativo deve deixar claro quando captura, grava ou transcreve áudio. Não implemente gravação oculta.

## 9. Gerenciamento da chave Groq

A chave deve ser fornecida pelo usuário nas configurações.

Nome lógico:

`GROQ_API_KEY`

Durante desenvolvimento, variáveis de ambiente podem ser usadas. Na aplicação compilada, preferir armazenamento seguro controlado pelo backend.

Nunca:

- colocar a chave no frontend
- incluir a chave no repositório
- imprimir a chave em logs
- retornar a chave em comandos Tauri
- salvar a chave junto ao transcript

Criar um botão de teste de conexão que retorne somente sucesso, falha e uma mensagem sanitizada.

## 10. Tratamento de erros

O aplicativo deve continuar gravando e organizando áudio mesmo quando um provedor externo falhar temporariamente.

Implementar estados claros:

- `idle`
- `starting`
- `running`
- `degraded`
- `stopping`
- `failed`

Falhas da Groq:

- manter bloco pendente localmente
- aplicar retry com backoff
- permitir reprocessamento
- não duplicar segmentos

Falhas do Codex:

- manter transcript funcionando
- preservar última análise válida
- permitir reconexão
- finalizar relatório posteriormente quando possível dentro da sessão atual do aplicativo

Nunca perder o transcript por causa de falha do analisador.

## 11. Logs e observabilidade

Logs devem ser úteis e sanitizados.

Registrar:

- início e fim de sessão
- dispositivos selecionados
- duração dos blocos
- latência de transcrição
- latência de análise
- tamanho das filas
- retries
- erros normalizados

Não registrar:

- chaves
- tokens
- URLs de login completas
- conteúdo integral do transcript por padrão
- respostas completas do modelo por padrão

Adicionar um modo de diagnóstico explícito para testes locais.

## 12. Testes obrigatórios

Antes de considerar uma funcionalidade concluída, executar os testes existentes e adicionar testes para a nova lógica.

Prioridades:

### 12.1 Testes unitários

- divisão de blocos de áudio
- detecção de silêncio
- deduplicação de overlap
- ordenação cronológica
- merge do estado de análise
- validação do JSON
- cálculo de métricas
- sanitização de logs

### 12.2 Testes de integração

- mock do endpoint Groq
- mock do Codex App Server
- login cancelado
- limite de uso atingido
- resposta JSON inválida
- queda de rede
- encerramento durante requisição

### 12.3 Teste manual antes do WhatsApp

Usar arquivos WAV de teste antes da chamada real.

Ordem recomendada:

1. reproduzir áudio conhecido como canal remoto
2. falar no microfone como canal local
3. verificar separação
4. verificar timestamps
5. verificar transcrição
6. verificar análise
7. verificar encerramento e relatório
8. testar no WhatsApp Desktop

## 13. Regras para alterações no código

Antes de implementar uma tarefa:

1. ler os arquivos relacionados
2. localizar implementações existentes
3. explicar brevemente a causa ou arquitetura atual
4. propor um plano pequeno
5. alterar somente o necessário
6. executar formatação, typecheck e testes
7. informar arquivos alterados e limitações

Não faça refatorações amplas junto com uma nova funcionalidade sem necessidade.

Não troque bibliotecas existentes apenas por preferência pessoal.

Não duplique providers, stores, schemas ou componentes.

Não remova funcionalidades originais do NexQ sem uma decisão explícita.

Não altere dependências ou versões sem explicar o motivo.

Não edite `package-lock.json` manualmente.

Não esconda erros de compilação com casts inseguros, `any`, `unwrap()` indiscriminado ou tratamento vazio.

## 14. Padrões de código

### TypeScript

- evitar `any`
- usar tipos de domínio explícitos
- validar dados vindos do backend e de provedores
- manter regras de negócio fora de componentes React
- não armazenar grandes transcripts em estado global sem necessidade
- cancelar listeners e timers no unmount

### Rust

- usar erros tipados
- evitar `unwrap()` em fluxo de produção
- não bloquear a thread principal
- usar tarefas assíncronas para rede e processamento
- encerrar subprocessos filhos corretamente
- limitar filas e concorrência
- sanitizar mensagens enviadas ao frontend

### Tauri

- comandos devem ter contratos pequenos
- recursos sensíveis ficam no backend
- frontend não acessa sistema de arquivos ou processos diretamente
- revisar capabilities e permissões ao adicionar plugins
- aplicar princípio do menor privilégio

## 15. Git e commits

Usar branches curtas e commits pequenos.

Padrão sugerido:

- `feat/groq-live-transcription`
- `feat/codex-analysis-provider`
- `feat/spin-playbook-engine`
- `fix/audio-loopback-buffer`

Commits em Conventional Commits:

- `feat(stt): add queued Groq transcription`
- `feat(codex): add ChatGPT managed login`
- `feat(spin): add incremental playbook state`
- `fix(audio): preserve channel timestamps`
- `test(analysis): validate malformed provider output`

Não misturar mudanças não relacionadas no mesmo commit.

## 16. Fases de implementação

### Fase 0: executar o fork sem alterações

Critérios:

- `npm install` concluído
- `npx tauri dev` inicia o aplicativo
- frontend abre
- backend Rust compila
- dispositivos de áudio são listados
- uma sessão de teste pode ser iniciada e encerrada

### Fase 1: validar captura e Groq existentes

Critérios:

- microfone e sistema aparecem separados
- Groq transcreve português
- segmentos possuem timestamps e fonte
- erros não encerram a sessão

Não criar integração Codex antes de essa fase funcionar.

### Fase 2: normalizar transcript e métricas locais

Critérios:

- transcript único e ordenado
- deduplicação funcional
- proporção de fala calculada localmente
- persistência no SQLite
- replay ou revisão da sessão

### Fase 3: integrar Codex

Critérios:

- detectar autenticação existente
- permitir login gerenciado pelo ChatGPT
- criar uma thread por chamada
- enviar apenas texto e estado resumido
- receber saída estruturada
- continuar funcionando sem análise quando Codex estiver indisponível

### Fase 4: implementar SPIN

Critérios:

- manual e prompt configuráveis
- quatro etapas acompanhadas
- evidências ligadas a segmentos
- alertas e próxima ação no overlay
- relatório final

### Fase 5: refinamento

Critérios:

- atalhos
- overlay compacto
- retenção de dados
- exportação
- instalador local
- testes de estabilidade em chamadas longas

### Fase futura: múltiplos participantes remotos

A primeira versão pode tratar todo áudio remoto como `remote`.

Diarização de vários participantes remotos é uma fase separada. Não adicionar Python, PyTorch ou pyannote na primeira versão sem decisão explícita.

## 17. Definition of Done

Uma tarefa só está concluída quando:

- compila no Windows
- não quebra captura existente
- não expõe segredos
- possui tratamento de erro
- possui testes proporcionais ao risco
- passou por typecheck e formatação
- possui instruções de teste manual
- não introduz dependência desnecessária
- não deixa processo filho aberto após fechar o aplicativo
- não perde transcript quando o analisador falha

## 18. Comandos esperados

Use os scripts existentes do repositório sempre que possível.

Comandos básicos:

```powershell
npm install
npm run build
npx tauri dev
npx tauri build
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
```

Antes de executar comandos destrutivos, excluir arquivos, alterar migrations ou atualizar muitas dependências, explique o impacto.

## 19. Primeira tarefa recomendada ao agente

Ao iniciar o trabalho neste repositório, faça primeiro uma auditoria sem modificar arquivos.

A resposta deve informar:

1. como o NexQ captura microfone e áudio do sistema
2. onde estão os providers de Groq STT
3. como as configurações e chaves são armazenadas
4. como transcripts são representados e persistidos
5. onde o overlay recebe atualizações
6. quais módulos podem ser reutilizados
7. quais riscos existem para integrar Codex App Server
8. um plano de implementação em fases com arquivos prováveis

Somente após essa auditoria, implemente a primeira alteração pequena e testável.
