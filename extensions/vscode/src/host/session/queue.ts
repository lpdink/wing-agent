/**
 * A minimal promise chain: tasks run one at a time, in submission order.
 *
 * Session lifecycle operations (create / resume / fork / close / resubscribe)
 * touch shared maps and the gateway's route table; serializing them keeps tab
 * order deterministic and makes "close right after open" a well-defined
 * sequence instead of a race. A failed task never blocks the queue (the next
 * task still runs), and the caller still sees the rejection.
 */
export class SerialQueue {
  private tail: Promise<unknown> = Promise.resolve();

  run<T>(task: () => Promise<T>): Promise<T> {
    const result = this.tail.then(task, task);
    this.tail = result.catch(() => undefined);
    return result;
  }
}
