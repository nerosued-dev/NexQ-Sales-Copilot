// Runs meeting-critical hooks at the App level so they persist
// even when the user navigates away from the overlay view.
// This ensures transcription, timers, and persistence continue
// in the background regardless of which page is displayed.
//
// Two-window architecture: launcher window handles persistence/audio control;
// overlay window subscribes to events for display only.

import { useMeetingStore } from "../stores/meetingStore";
import { useMeetingTimer } from "../hooks/useMeetingTimer";
import { useTranscriptPersistence } from "../hooks/useTranscriptPersistence";
import { useTranscript } from "../hooks/useTranscript";
import { useSpeechRecognition } from "../hooks/useSpeechRecognition";
import { useAudioConfigSync } from "../hooks/useAudioConfigSync";
import { useStreamBuffer } from "../hooks/useStreamBuffer";
import { useCallLogCapture } from "../hooks/useCallLogCapture";
import { useSTTStatus } from "../hooks/useSTTStatus";
import { useDevLog } from "../hooks/useDevLog";

export function ActiveMeetingProvider({ isLauncherWindow = true }: { isLauncherWindow?: boolean }) {
  const activeMeeting = useMeetingStore((s) => s.activeMeeting);

  if (!activeMeeting) return null;

  // Overlay window: subscribe to events for display (no persistence, no audio control)
  if (!isLauncherWindow) return <OverlayMeetingHooks />;

  return <LauncherMeetingHooks />;
}

// Overlay window: display-only hooks — populate local Zustand stores from Tauri events
function OverlayMeetingHooks() {
  useMeetingTimer();
  useTranscript();    // transcript_final/update → local store
  useStreamBuffer();  // AI streaming events → local store
  useSTTStatus();     // STT connection status
  useDevLog();        // debug log entries
  return null;
}

// Launcher window: persistence + audio control (hidden during meeting)
function LauncherMeetingHooks() {
  useMeetingTimer();
  useTranscript();         // also subscribe here so persistence hook sees segments
  useSpeechRecognition();  // web speech API (emits cross-window events for overlay)
  useAudioConfigSync();    // hot-swap STT/audio config
  useTranscriptPersistence(); // transcript store → SQLite DB
  useStreamBuffer();
  useCallLogCapture();
  useSTTStatus();
  useDevLog();
  return null;
}
