# Plan — Web search rápido, modo "Opinión" y fix transparencia overlay

Fecha: 2026-06-12
Branch: `spans`

## Contexto

Pedido del usuario:
1. Revisar por qué la búsqueda de info "no funciona bien" comparado con apps tipo Cluely (referencias: [pluely](https://github.com/iamsrikanthnani/pluely), [OpenCluely](https://github.com/TechyCSR/OpenCluely)).
2. Agregar opción para que la IA dé una **opinión/análisis propio**, no solo un resumen de los chunks de memoria/RAG.
3. Implementar **web search rápido**, inspirado en cómo lo hacen pluely/OpenCluely.
4. Continuar con la **transparencia del overlay** (sesión 2026-04-28, quedó sin confirmar — usuario reporta que solo ve blanco/gris).

---

## 1. Fix transparencia overlay

### Diagnóstico

La sesión 04-28 dejó implementado:
- `src-tauri/src/lib.rs:68-79` (`show_overlay`): llama `window_vibrancy::apply_blur(&overlay, Some((0,0,0,0)))`.
- `src/App.tsx:115-125`: además del `transparent:true` + `setBackgroundColor([0,0,0,0])`, hace `win.setEffects({ effects: [Effect.Blur] })`.

`apply_blur` / `Effect.Blur` usan `DwmEnableBlurBehindWindow`, una API de Windows Aero **deprecada desde Windows 8**. En Windows 10/11 normalmente no compone un "ver a través" nítido de otras apps — pinta un fondo gris/blanco opaco o un blur estático del wallpaper. Esto explica el reporte del usuario ("solo veo blanco/gris").

### Cómo lo hacen pluely / OpenCluely

- **Pluely** (`src-tauri/tauri.conf.json`): ventana con `"transparent": true, "decorations": false, "shadow": false, "contentProtected": true`. Sin ningún efecto de blur/vibrancy — el passthrough real lo da WebView2/CoreAnimation con fondo alfa 0.
- **OpenCluely** (Electron `window.manager.js`): `transparent: true, backgroundColor: '#00000000', frame: false, hasShadow: false`. Tampoco usa blur.

Ambos confían en que `transparent:true` + fondo del webview con alfa 0 + CSS `background: transparent` es suficiente para passthrough real en Win10/11. El blur-behind es lo que rompe esto.

### Fix propuesto

- **`src-tauri/src/lib.rs`**: eliminar el bloque `apply_blur` en `show_overlay()` (líneas 70-72).
- **`src/App.tsx`**: eliminar el `import("@tauri-apps/api/window").then(({ Effect }) => win.setEffects(...))` (líneas 121-124). Mantener `setBackgroundColor([0,0,0,0])` y el CSS `background: transparent`.
- **`src-tauri/Cargo.toml`** / **`Cargo.lock`**: quitar dependencia `window-vibrancy` si no se usa en ningún otro lugar (verificar con grep antes de borrar).
- **`src-tauri/capabilities/default.json`**: revisar si `allow-set-effects` sigue siendo necesario; si no, quitarlo (mantener `allow-set-background-color`, `allow-set-decorations`, etc.).
- Opcional, alinear con pluely: agregar `"shadow": false` a la config de la ventana `overlay` en `tauri.conf.json` para evitar sombra residual cuando opacity es bajo.
- La opacidad visual la sigue controlando `.overlay-bg` (`src/overlay/OverlayView.tsx:142`) vía `background: hsl(var(--background) / overlayOpacity)` — eso ya está bien, es CSS normal sobre un webview transparente.

### Verificación
- `npm run tauri dev`, abrir overlay, bajar opacidad con el botón del ojo a 10%/35% y confirmar que se ve el escritorio/otras apps detrás, no gris/blanco.
- Confirmar que a 90-100% el panel se ve sólido (comportamiento actual no debería cambiar).

---

## 2. Web search rápido ("como Cluely")

### Cómo lo hacen pluely / OpenCluely (investigado)

Ninguno de los dos implementa un backend de búsqueda propio (no hay SerpAPI/Tavily/Brave/etc.):

- **Pluely**: lista de proveedores en `src/config/ai-providers.constants.ts` incluye `perplexity` (modelo con acceso a web nativo) y `openrouter`. El "buscar rápido" de Cluely-likes en general viene de:
  - Perplexity (búsqueda integrada en el modelo), o
  - OpenRouter con el sufijo `:online` / plugin `web` (usa Exa por debajo) — funciona con **cualquier modelo** sin tocar nada más.
- **OpenCluely**: usa Gemini directo (`src/services/llm.service.js`, `@google/generative-ai`) **sin** tools de grounding (`googleSearch`). Su "inteligencia" viene de capturas de pantalla + contexto, no de búsqueda web real. No es un buen ejemplo a copiar para "buscar en la web".

### Recomendación para NexQ

NexQ ya tiene proveedores Gemini y OpenRouter (`src-tauri/src/llm/gemini.rs`, `openrouter_models.rs`). El camino más rápido y nativo:

1. **Gemini grounding nativo**: en `gemini.rs:155` (`stream_completion`), cuando el toggle esté activo, agregar al body:
   ```json
   "tools": [{ "google_search": {} }]
   ```
   Soportado en Gemini 2.0/2.5. Devuelve `groundingMetadata` con las fuentes — esto es exactamente "buscar en la web rápido" sin API key adicional.

2. **OpenRouter `:online`**: cuando el toggle esté activo y el provider activo sea OpenRouter, anexar `:online` al id del modelo (o enviar `plugins:[{id:"web"}]` en el body) — funciona con cualquier modelo del catálogo.

3. **Nuevo toggle por acción** `web_search: bool` en `ActionConfig` (`src-tauri/src/intelligence/action_config.rs`), default `false`. Tiene más sentido habilitarlo por defecto en `AskQuestion` (modo "Ask").

4. **Hilo de datos**:
   - `action_config.rs`: agregar campo `web_search: bool` a `ActionConfig`.
   - `intelligence_commands.rs` (`generate_assist`): leer `action_cfg.web_search`, pasarlo a `GenerationParams` (nuevo campo `enable_web_search: bool` en `provider.rs`).
   - `gemini.rs` / proveedor OpenRouter (`openai_compat.rs` o el que maneje OpenRouter): si `enable_web_search`, inyectar `tools`/`:online`/`plugins` en el body.

5. **UI**: en `AIActionsSettings.tsx`, agregar un toggle (ícono `Globe`) junto a los toggles existentes (`include_rag_chunks`, `include_transcript`, etc.) por acción.

6. **Mostrar fuentes**: si Gemini devuelve `groundingMetadata.groundingChunks` (urls/títulos), mostrarlas como links al final de la respuesta en `AIResponsePanel.tsx` (similar a como Perplexity/Cluely muestran "Sources").

### Notas
- Esto NO requiere ninguna API key nueva si se usa Gemini (la búsqueda viene incluida en la llamada al modelo).
- Para OpenRouter `:online` sí puede tener costo extra por el plugin Exa — documentarlo en la UI.
- Otros proveedores (Anthropic, custom, Ollama) no tienen equivalente nativo simple — el toggle debería deshabilitarse/ocultarse si el provider activo no soporta grounding.

---

## 3. Modo "Opinión" (no solo resumen de la memoria)

### Diagnóstico actual

- `ASK_QUESTION_PROMPT` (`prompt_templates.rs:127-131`): "Answer directly... based on all available context... Be precise and cite specific parts" — grounded, sin pedir opinión.
- `ASSIST_PROMPT`: similar, factual.
- `RECAP_PROMPT`: explícitamente "do not add interpretation".
- `InstructionPresets` (`action_config.rs:49-65`) ya tiene un patrón de presets opcionales (`tone`, `format`, `length`) que se componen en `compose_instructions()` (`intelligence_commands.rs:12-44`) y se agregan al system prompt. Es el lugar natural para un cuarto preset.

### Propuesta

- Agregar campo `opinion: Option<String>` a `InstructionPresets` (valores: `None` = comportamiento actual / `"add"` = agregar opinión).
- En `compose_instructions()` (Rust) y su espejo `composeInstructions()` (`src/stores/aiActionsStore.ts:121`), cuando `opinion == "add"`, anexar algo como:
  > "After answering based on the provided context, add a short section '## My Take' with your own analysis, interpretation, or recommendation — clearly separated from the factual answer above."
- UI: en `AIActionsSettings.tsx`, junto a los pills de Tone/Format/Length (líneas ~360-410), agregar un cuarto grupo "Perspective" con opciones `Factual only` (default) / `Add my take`.
- Default = off (no cambia el comportamiento existente; opt-in).
- Aplica a nivel global (`instructionPresets`), igual que tone/format/length — no requiere tocar RAG/search pipeline, solo el system prompt final.

---

## Orden sugerido de implementación

| Orden | Tarea | Esfuerzo | Riesgo |
|-------|-------|----------|--------|
| 1 | Fix transparencia (quitar blur) | Bajo (~20 min) | Bajo — solo revierte cambios recientes |
| 2 | Modo "Opinión" (preset nuevo) | Medio (~1h) | Bajo — sigue patrón existente de presets |
| 3 | Web search (Gemini grounding + OpenRouter `:online`) | Medio-alto (~2-4h) | Medio — toca proveedores LLM y streaming |

## Archivos a tocar (resumen)

```
# 1. Transparencia
src-tauri/src/lib.rs                  — quitar apply_blur en show_overlay()
src/App.tsx                           — quitar Effect.Blur / setEffects
src-tauri/Cargo.toml / Cargo.lock     — quitar window-vibrancy (si no se usa en otro lado)
src-tauri/capabilities/default.json   — revisar allow-set-effects
src-tauri/tauri.conf.json             — opcional: "shadow": false en overlay

# 2. Modo Opinión
src-tauri/src/intelligence/action_config.rs   — InstructionPresets.opinion
src-tauri/src/commands/intelligence_commands.rs — compose_instructions()
src/stores/aiActionsStore.ts                   — composeInstructions(), tipos
src/lib/types.ts                               — InstructionPresets type
src/settings/AIActionsSettings.tsx             — UI toggle "Perspective"

# 3. Web search
src-tauri/src/intelligence/action_config.rs   — ActionConfig.web_search
src-tauri/src/llm/provider.rs                  — GenerationParams.enable_web_search
src-tauri/src/llm/gemini.rs                    — tools: [{google_search:{}}]
src-tauri/src/llm/openai_compat.rs (OpenRouter) — :online / plugins web
src-tauri/src/commands/intelligence_commands.rs — hilo del toggle
src/settings/AIActionsSettings.tsx             — UI toggle "Web search" (Globe)
src/overlay/AIResponsePanel.tsx                — mostrar fuentes/citations
```
