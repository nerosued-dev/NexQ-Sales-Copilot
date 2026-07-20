import { useEffect, useRef } from "react";
import { useMeetingStore } from "../stores/meetingStore";
import { useTranscriptStore } from "../stores/transcriptStore";
import { appendTranscriptSegment } from "../lib/ipc";
import {
  diagnosticErrorType,
  getTranscriptCounts,
  transcriptDiag,
} from "../lib/transcriptDiagnostics";

const FLUSH_INTERVAL_MS = 30_000; // 30 seconds

/**
 * Hook that persists transcript segments to SQLite incrementally.
 * Every 30 seconds during an active meeting, it flushes any new
 * (not-yet-persisted) transcript segments to the database.
 * Tracks which segments have been persisted by index.
 */
export function useTranscriptPersistence() {
  const activeMeeting = useMeetingStore((s) => s.activeMeeting);
  const lastPersistedIndex = useMeetingStore((s) => s.lastPersistedIndex);
  const setLastPersistedIndex = useMeetingStore((s) => s.setLastPersistedIndex);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => {
    if (!activeMeeting) {
      // No active meeting — clear interval
      if (intervalRef.current) {
        clearInterval(intervalRef.current);
        intervalRef.current = null;
      }
      return;
    }

    const meetingId = activeMeeting.id;
    transcriptDiag("persistence_hook_mounted", { meetingId });

    async function flushSegments(reason: "timer" | "cleanup") {
      const segments = useTranscriptStore.getState().segments;
      const currentLastIndex = useMeetingStore.getState().lastPersistedIndex;
      const counts = getTranscriptCounts(segments);

      transcriptDiag("persistence_flush_started", {
        reason,
        meetingId,
        currentLastIndex,
        ...counts,
      });

      // Persist only the contiguous finalized prefix after the last checkpoint.
      // If a later final arrives while an earlier interim is still open, don't
      // advance past that interim or it can be skipped when it finalizes.
      let persistUntil = currentLastIndex;
      while (persistUntil < segments.length && segments[persistUntil].is_final) {
        persistUntil += 1;
      }

      const newSegments = segments.slice(currentLastIndex, persistUntil);

      transcriptDiag("persistence_flush_selection", {
        reason,
        meetingId,
        currentLastIndex,
        persistUntil,
        selected: newSegments.length,
        ...counts,
      });

      if (newSegments.length === 0) {
        transcriptDiag("persistence_flush_finished", {
          reason,
          meetingId,
          selected: 0,
          persisted: 0,
        });
        return;
      }

      for (const segment of newSegments) {
        transcriptDiag("persistence_insert_started", {
          reason,
          meetingId,
          segmentId: segment.id,
          isFinal: segment.is_final,
        });
        try {
          await appendTranscriptSegment(meetingId, JSON.stringify(segment));
          transcriptDiag("persistence_insert_succeeded", {
            reason,
            meetingId,
            segmentId: segment.id,
            isFinal: segment.is_final,
          });
        } catch (err) {
          transcriptDiag("persistence_insert_failed", {
            reason,
            meetingId,
            segmentId: segment.id,
            isFinal: segment.is_final,
            errorType: diagnosticErrorType(err),
          });
          console.error("[transcriptPersistence] Failed to persist segment:", err);
          // Stop trying to persist more if one fails
          transcriptDiag("persistence_flush_failed", {
            reason,
            meetingId,
            currentLastIndex,
            persistUntil,
            selected: newSegments.length,
          });
          return;
        }
      }

      setLastPersistedIndex(persistUntil);
      transcriptDiag("persistence_index_updated", {
        reason,
        meetingId,
        previousIndex: currentLastIndex,
        newIndex: persistUntil,
      });
      transcriptDiag("persistence_flush_finished", {
        reason,
        meetingId,
        selected: newSegments.length,
        persisted: newSegments.length,
      });
    }

    // Set up the 30-second interval
    intervalRef.current = setInterval(() => {
      void flushSegments("timer");
    }, FLUSH_INTERVAL_MS);

    return () => {
      // On cleanup, do a final flush
      transcriptDiag("persistence_hook_cleanup", {
        meetingId,
        asyncFlushAwaited: false,
      });
      void flushSegments("cleanup");

      if (intervalRef.current) {
        clearInterval(intervalRef.current);
        intervalRef.current = null;
      }
      transcriptDiag("persistence_hook_unmounted", { meetingId });
    };
  }, [activeMeeting, setLastPersistedIndex]);
}
