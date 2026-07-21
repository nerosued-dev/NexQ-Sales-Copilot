import assert from "node:assert/strict";
import test from "node:test";
import type { TranscriptSegment } from "../src/lib/types.ts";
import {
  createFinalPersistencePlan,
  createSingleFlight,
  IncompleteTranscriptError,
  mergeTranscriptSegments,
  persistFinalTranscript,
} from "../src/lib/transcriptFinalization.ts";

function segment(id: string, isFinal: boolean, timestamp = 0): TranscriptSegment {
  return {
    id,
    text: `text-${id}`,
    speaker: "User",
    timestamp_ms: timestamp,
    is_final: isFinal,
    confidence: 0.9,
  };
}

test("shutdown merge is idempotent and replaces a partial with its final", () => {
  const partial = segment("one", false, 20);
  const final = segment("one", true, 20);
  const first = mergeTranscriptSegments([partial], [final]);
  const second = mergeTranscriptSegments(first, [final]);

  assert.deepEqual(first, [final]);
  assert.deepEqual(second, [final]);
});

test("shutdown merge preserves chronological order for both channels", () => {
  const merged = mergeTranscriptSegments(
    [],
    [segment("them", true, 30), segment("you", true, 10)]
  );
  assert.deepEqual(merged.map(({ id }) => id), ["you", "them"]);
});

test("a genuinely empty meeting has a complete empty persistence plan", () => {
  assert.deepEqual(createFinalPersistencePlan([], 0), {
    complete: true,
    pending: [],
    incompleteSegmentIds: [],
  });
});

test("a short partial becomes persistable after shutdown finalization", async () => {
  const merged = mergeTranscriptSegments(
    [segment("short", false)],
    [segment("short", true)]
  );
  const persisted: string[] = [];
  const nextIndex = await persistFinalTranscript({
    segments: merged,
    lastPersistedIndex: 0,
    persist: async ({ id }) => {
      persisted.push(id);
    },
  });

  assert.deepEqual(persisted, ["short"]);
  assert.equal(nextIndex, 1);
});

test("a partial followed by finals is rejected instead of reporting selected zero", async () => {
  const segments = [
    segment("partial", false),
    segment("final-1", true),
    segment("final-2", true),
  ];
  let writes = 0;

  await assert.rejects(
    persistFinalTranscript({
      segments,
      lastPersistedIndex: 0,
      persist: async () => {
        writes += 1;
      },
    }),
    (error: unknown) =>
      error instanceof IncompleteTranscriptError &&
      error.segmentIds[0] === "partial"
  );
  assert.equal(writes, 0);
  assert.deepEqual(segments.map(({ id }) => id), ["partial", "final-1", "final-2"]);
});

test("persistence failure reports progress without mutating transcript data", async () => {
  const segments = [segment("one", true), segment("two", true)];
  const checkpoints: number[] = [];

  await assert.rejects(
    persistFinalTranscript({
      segments,
      lastPersistedIndex: 0,
      persist: async ({ id }) => {
        if (id === "two") throw new Error("database unavailable");
      },
      onPersisted: (index) => checkpoints.push(index),
    }),
    /database unavailable/
  );
  assert.deepEqual(checkpoints, [1]);
  assert.equal(segments.length, 2);
});

test("two finalization calls share one in-flight operation", async () => {
  let calls = 0;
  let release: (() => void) | undefined;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const finalize = createSingleFlight(async () => {
    calls += 1;
    await gate;
    return "done";
  });

  const first = finalize();
  const second = finalize();
  assert.equal(first, second);
  release?.();
  assert.equal(await first, "done");
  assert.equal(await second, "done");
  assert.equal(calls, 1);
});
