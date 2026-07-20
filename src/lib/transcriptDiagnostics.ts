import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import type { TranscriptSegment } from "./types";

const DIAG_PREFIX = "NEXQ_TRANSCRIPT_DIAG";

export interface TranscriptCounts {
  total: number;
  finals: number;
  partials: number;
}

export function getTranscriptCounts(
  segments: readonly TranscriptSegment[]
): TranscriptCounts {
  const finals = segments.filter((segment) => segment.is_final).length;
  return {
    total: segments.length,
    finals,
    partials: segments.length - finals,
  };
}

export function getDiagnosticWindowLabel(): string {
  try {
    return getCurrentWebviewWindow().label;
  } catch {
    return "unknown";
  }
}

export function transcriptDiag(
  event: string,
  fields: Record<string, string | number | boolean | null | undefined> = {}
): void {
  const isDevelopment =
    (import.meta as ImportMeta & { env?: { DEV?: boolean } }).env?.DEV === true;
  if (!isDevelopment) return;

  console.info(
    `${DIAG_PREFIX} ${JSON.stringify({
      timestampMs: Date.now(),
      window: getDiagnosticWindowLabel(),
      event,
      ...fields,
    })}`
  );
}

export function diagnosticErrorType(error: unknown): string {
  return error instanceof Error ? error.name : typeof error;
}
