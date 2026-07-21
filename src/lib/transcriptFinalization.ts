import type { TranscriptSegment } from "./types";

export interface FinalPersistencePlan {
  complete: boolean;
  pending: TranscriptSegment[];
  incompleteSegmentIds: string[];
}

export class IncompleteTranscriptError extends Error {
  readonly segmentIds: string[];

  constructor(segmentIds: string[]) {
    super("Transcript finalization is incomplete");
    this.name = "IncompleteTranscriptError";
    this.segmentIds = segmentIds;
  }
}

export function mergeTranscriptSegments(
  current: readonly TranscriptSegment[],
  incoming: readonly TranscriptSegment[]
): TranscriptSegment[] {
  const merged = [...current];
  const positions = new Map(current.map((segment, index) => [segment.id, index]));
  const additions: Array<{ segment: TranscriptSegment; order: number }> = [];
  for (const segment of incoming) {
    const existing = positions.get(segment.id);
    if (existing === undefined) {
      positions.set(segment.id, merged.length + additions.length);
      additions.push({ segment, order: additions.length });
    } else if (existing < merged.length) {
      merged[existing] = segment;
    }
  }

  additions
    .sort(
      (left, right) =>
        left.segment.timestamp_ms - right.segment.timestamp_ms ||
        left.order - right.order
    )
    .forEach(({ segment }) => merged.push(segment));
  return merged;
}

export function createFinalPersistencePlan(
  segments: readonly TranscriptSegment[],
  lastPersistedIndex: number
): FinalPersistencePlan {
  if (lastPersistedIndex < 0 || lastPersistedIndex > segments.length) {
    return {
      complete: false,
      pending: [],
      incompleteSegmentIds: ["invalid-persistence-checkpoint"],
    };
  }

  const pending = segments.slice(lastPersistedIndex);
  const incompleteSegmentIds = pending
    .filter((segment) => !segment.is_final)
    .map((segment) => segment.id);

  return {
    complete: incompleteSegmentIds.length === 0,
    pending,
    incompleteSegmentIds,
  };
}

let persistenceTail: Promise<void> = Promise.resolve();

export function withTranscriptPersistenceLock<T>(task: () => Promise<T>): Promise<T> {
  const result = persistenceTail.then(task, task);
  persistenceTail = result.then(
    () => undefined,
    () => undefined
  );
  return result;
}

export async function persistFinalTranscript(options: {
  segments: readonly TranscriptSegment[];
  lastPersistedIndex: number;
  persist: (segment: TranscriptSegment) => Promise<void>;
  onPersisted?: (nextIndex: number) => void;
}): Promise<number> {
  const plan = createFinalPersistencePlan(
    options.segments,
    options.lastPersistedIndex
  );
  if (!plan.complete) {
    throw new IncompleteTranscriptError(plan.incompleteSegmentIds);
  }

  let nextIndex = options.lastPersistedIndex;
  for (const segment of plan.pending) {
    await options.persist(segment);
    nextIndex += 1;
    options.onPersisted?.(nextIndex);
  }
  return nextIndex;
}

export function createSingleFlight<TArgs extends readonly unknown[], TResult>(
  operation: (...args: TArgs) => Promise<TResult>
): (...args: TArgs) => Promise<TResult> {
  let active: Promise<TResult> | null = null;
  return (...args) => {
    if (active) return active;
    active = operation(...args).finally(() => {
      active = null;
    });
    return active;
  };
}
